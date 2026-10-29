//! One open archive = one window. The `Document` object is the window's
//! delegate, the outline view's data source/delegate and the target of the
//! window's actions (Extract, Test, double-click, Cancel).

use std::cell::{Cell, OnceCell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBackingStoreType, NSBezelStyle, NSButton, NSControlStateValueOff,
    NSControlStateValueOn, NSControlTextEditingDelegate, NSImageView, NSModalResponse,
    NSModalResponseOK, NSOpenPanel, NSOutlineView, NSOutlineViewDataSource, NSOutlineViewDelegate,
    NSPanel, NSProgressIndicator, NSProgressIndicatorStyle, NSScrollView, NSTableCellView,
    NSTableColumn, NSTableColumnResizingOptions, NSTableViewColumnAutoresizingStyle,
    NSTableViewStyle, NSTextField, NSUserInterfaceItemIdentification, NSView, NSWindow,
    NSWindowDelegate, NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{
    ns_string, NSArray, NSIndexSet, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect,
    NSSize, NSSortDescriptor, NSString, NSURL,
};

use archive_core::{
    Archive, Cancel, ConflictAction, ConflictDecision, Error, ExtractOptions, ExtractReport,
    Overwrite, Progress,
};

use crate::node::{build_tree, sort_tree, Node, SortKey};
use crate::sheets;
use crate::tasks::{spawn, MainHandle};
use crate::util::{
    default_extract_dir, format_date, format_size, ns, path_to_url, temp_dir_for, url_to_path,
};

const COL_NAME: &str = "name";
const COL_SIZE: &str = "size";
const COL_PACKED: &str = "packed";
const COL_MODIFIED: &str = "modified";
const BOTTOM_BAR_HEIGHT: f64 = 34.0;

/// What the current background job is doing; drives the progress sheet.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Job {
    Extract,
    Test,
    OpenEntry,
}

struct ProgressUi {
    panel: Retained<NSPanel>,
    label: Retained<NSTextField>,
    bar: Retained<NSProgressIndicator>,
    shown: Cell<bool>,
    shown_at: Cell<Option<Instant>>,
}

#[derive(Default)]
pub struct DocIvars {
    window: OnceCell<Retained<NSWindow>>,
    outline: OnceCell<Retained<NSOutlineView>>,
    status: OnceCell<Retained<NSTextField>>,
    extract_button: OnceCell<Retained<NSButton>>,
    spinner: OnceCell<Retained<NSProgressIndicator>>,
    progress: OnceCell<ProgressUi>,

    path: RefCell<Option<PathBuf>>,
    archive: RefCell<Option<Arc<dyn Archive>>>,
    roots: RefCell<Vec<Retained<Node>>>,
    password: RefCell<Option<String>>,
    temp_dir: RefCell<Option<PathBuf>>,
    last_extract_dir: RefCell<Option<PathBuf>>,

    cancel: RefCell<Option<Arc<Cancel>>>,
    job: Cell<Option<Job>>,
    closed: Cell<bool>,
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - Document does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ZipperDocument"]
    #[ivars = DocIvars]
    pub struct Document;

    unsafe impl NSObjectProtocol for Document {}

    // ---------------------------------------------------------- actions
    impl Document {
        #[unsafe(method(extractSelected:))]
        fn extract_selected_action(&self, _sender: Option<&AnyObject>) {
            self.begin_extract(false);
        }

        #[unsafe(method(extractAll:))]
        fn extract_all_action(&self, _sender: Option<&AnyObject>) {
            self.begin_extract(true);
        }

        #[unsafe(method(testArchive:))]
        fn test_action(&self, _sender: Option<&AnyObject>) {
            self.begin_test();
        }

        #[unsafe(method(outlineDoubleClicked:))]
        fn double_clicked(&self, _sender: Option<&AnyObject>) {
            self.open_clicked_row();
        }

        #[unsafe(method(openSelected:))]
        fn open_selected_action(&self, _sender: Option<&AnyObject>) {
            self.open_clicked_row();
        }

        #[unsafe(method(cancelTask:))]
        fn cancel_action(&self, _sender: Option<&AnyObject>) {
            if let Some(c) = self.ivars().cancel.borrow().as_ref() {
                c.cancel();
            }
            if let Some(p) = self.ivars().progress.get() {
                p.label.setStringValue(&ns("Cancelling…"));
            }
        }
    }

    // ---------------------------------------------------------- window
    unsafe impl NSWindowDelegate for Document {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _n: &NSNotification) {
            self.close_cleanup();
        }
    }

    // ---------------------------------------------------------- outline
    unsafe impl NSOutlineViewDataSource for Document {
        #[unsafe(method(outlineView:numberOfChildrenOfItem:))]
        unsafe fn number_of_children(&self, _ov: &NSOutlineView, item: Option<&AnyObject>) -> isize {
            match item.and_then(|i| i.downcast_ref::<Node>()) {
                None => self.ivars().roots.borrow().len() as isize,
                Some(n) => n.child_count() as isize,
            }
        }

        #[unsafe(method_id(outlineView:child:ofItem:))]
        unsafe fn child_of_item(&self, _ov: &NSOutlineView, index: isize, item: Option<&AnyObject>) -> Retained<AnyObject> {
            let node = match item.and_then(|i| i.downcast_ref::<Node>()) {
                None => self.ivars().roots.borrow()[index as usize].clone(),
                Some(n) => n.child(index as usize).expect("child index in range"),
            };
            Retained::into_super(Retained::into_super(node))
        }

        #[unsafe(method(outlineView:isItemExpandable:))]
        unsafe fn is_expandable(&self, _ov: &NSOutlineView, item: &AnyObject) -> bool {
            item.downcast_ref::<Node>().map(|n| n.data().is_dir).unwrap_or(false)
        }

        #[unsafe(method(outlineView:sortDescriptorsDidChange:))]
        fn sort_descriptors_changed(&self, ov: &NSOutlineView, _old: &NSArray<NSSortDescriptor>) {
            let descs = ov.sortDescriptors();
            if let Some(d) = descs.firstObject() {
                let key = d.key().map(|k| k.to_string()).unwrap_or_default();
                if let Some(k) = SortKey::from_identifier(&key) {
                    sort_tree(&mut self.ivars().roots.borrow_mut(), k, d.ascending());
                    ov.reloadData();
                }
            }
        }
    }

    unsafe impl NSControlTextEditingDelegate for Document {}

    unsafe impl NSOutlineViewDelegate for Document {
        #[unsafe(method_id(outlineView:viewForTableColumn:item:))]
        unsafe fn view_for_column(&self, ov: &NSOutlineView, column: Option<&NSTableColumn>, item: &AnyObject) -> Option<Retained<NSView>> {
            self.cell_view(ov, column, item)
        }

        #[unsafe(method(outlineViewSelectionDidChange:))]
        fn selection_changed(&self, _n: &NSNotification) {
            self.update_status();
        }
    }
);

// ------------------------------------------------------------------ impl

impl Document {
    /// Create the window and start loading `path` in the background.
    pub fn open(path: PathBuf, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DocIvars::default());
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        *this.ivars().path.borrow_mut() = Some(path.clone());
        this.build_window(mtm);
        this.load(None);
        this
    }

    pub fn window(&self) -> &NSWindow {
        self.ivars().window.get().expect("window built")
    }

    pub fn path(&self) -> Option<PathBuf> {
        self.ivars().path.borrow().clone()
    }

    fn handle(&self) -> MainHandle<Self> {
        MainHandle::new(self.retain(), self.mtm())
    }

    fn archive_name(&self) -> String {
        self.path()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "archive".into())
    }

    // ---------------------------------------------------------- building

    fn build_window(&self, mtm: MainThreadMarker) {
        let iv = self.ivars();
        let content = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(760.0, 480.0));
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                content,
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Miniaturizable
                    | NSWindowStyleMask::Resizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&ns(&self.archive_name()));
        unsafe { window.setContentMinSize(NSSize::new(420.0, 240.0)) };
        window.setFrameAutosaveName(ns_string!("ZipperDocumentWindow"));
        window.setDelegate(Some(ProtocolObject::from_ref(self)));

        let root = window.contentView().expect("content view");
        let bounds = root.bounds();

        // --- outline view inside a scroll view
        let scroll_frame = NSRect::new(
            NSPoint::new(0.0, BOTTOM_BAR_HEIGHT),
            NSSize::new(bounds.size.width, bounds.size.height - BOTTOM_BAR_HEIGHT),
        );
        let scroll = unsafe { NSScrollView::initWithFrame(NSScrollView::alloc(mtm), scroll_frame) };
        scroll.setHasVerticalScroller(true);
        scroll.setHasHorizontalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );

        let outline =
            unsafe { NSOutlineView::initWithFrame(NSOutlineView::alloc(mtm), scroll_frame) };
        outline.setAllowsMultipleSelection(true);
        outline.setAllowsEmptySelection(true);
        outline.setUsesAlternatingRowBackgroundColors(true);
        outline.setColumnAutoresizingStyle(
            NSTableViewColumnAutoresizingStyle::FirstColumnOnlyAutoresizingStyle,
        );
        outline.setStyle(NSTableViewStyle::Inset);
        outline.setAutosaveTableColumns(true);
        unsafe { outline.setAutosaveName(Some(ns_string!("ZipperOutline"))) };

        let name_col = make_column(COL_NAME, "Name", 320.0, 120.0, mtm);
        let size_col = make_column(COL_SIZE, "Size", 90.0, 60.0, mtm);
        let packed_col = make_column(COL_PACKED, "Packed", 90.0, 60.0, mtm);
        let mod_col = make_column(COL_MODIFIED, "Modified", 150.0, 90.0, mtm);
        outline.addTableColumn(&name_col);
        outline.addTableColumn(&size_col);
        outline.addTableColumn(&packed_col);
        outline.addTableColumn(&mod_col);
        unsafe { outline.setOutlineTableColumn(Some(&name_col)) };

        let me: &AnyObject = self;
        unsafe {
            outline.setDataSource(Some(ProtocolObject::from_ref(self)));
            outline.setDelegate(Some(ProtocolObject::from_ref(self)));
            outline.setTarget(Some(me));
            outline.setDoubleAction(Some(sel!(outlineDoubleClicked:)));
        }
        unsafe { scroll.setDocumentView(Some(&outline)) };
        root.addSubview(&scroll);

        // --- bottom bar: status label, spinner, Extract button
        let bar_frame = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(bounds.size.width, BOTTOM_BAR_HEIGHT),
        );
        let bar = unsafe { NSView::initWithFrame(NSView::alloc(mtm), bar_frame) };
        bar.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );

        let status = NSTextField::labelWithString(&ns("Loading…"), mtm);
        status.setFrame(NSRect::new(
            NSPoint::new(12.0, 8.0),
            NSSize::new(bounds.size.width - 160.0, 18.0),
        ));
        status.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        unsafe { status.setUsesSingleLineMode(true) };
        status.setFont(Some(&objc2_app_kit::NSFont::systemFontOfSize(11.0)));
        status.setTextColor(Some(&objc2_app_kit::NSColor::secondaryLabelColor()));
        bar.addSubview(&status);

        let spinner = unsafe {
            NSProgressIndicator::initWithFrame(
                NSProgressIndicator::alloc(mtm),
                NSRect::new(
                    NSPoint::new(bounds.size.width - 140.0, 9.0),
                    NSSize::new(16.0, 16.0),
                ),
            )
        };
        spinner.setStyle(NSProgressIndicatorStyle::Spinning);
        spinner.setControlSize(objc2_app_kit::NSControlSize::Small);
        spinner.setDisplayedWhenStopped(false);
        spinner.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
        bar.addSubview(&spinner);

        let button = unsafe {
            NSButton::buttonWithTitle_target_action(
                &ns("Extract…"),
                Some(me),
                Some(sel!(extractSelected:)),
                mtm,
            )
        };
        button.setBezelStyle(NSBezelStyle::Rounded);
        button.setFrame(NSRect::new(
            NSPoint::new(bounds.size.width - 112.0, 3.0),
            NSSize::new(100.0, 28.0),
        ));
        button.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
        button.setKeyEquivalent(ns_string!("\r"));
        button.setEnabled(false);
        bar.addSubview(&button);

        root.addSubview(&bar);

        iv.window.set(window.clone()).ok();
        iv.outline.set(outline).ok();
        iv.status.set(status).ok();
        iv.extract_button.set(button).ok();
        iv.spinner.set(spinner).ok();

        window.center();
        window.makeKeyAndOrderFront(None);
    }

    // ---------------------------------------------------------- loading

    fn load(&self, password: Option<String>) {
        let Some(path) = self.path() else { return };
        self.set_busy(true, "Opening…");
        let h = self.handle();
        spawn("zipper-open", move || {
            let result = archive_core::open(&path, password.as_deref()).map(Arc::from);
            h.on_main(move |doc, mtm| doc.loaded(result, password, mtm));
        });
    }

    fn loaded(
        &self,
        result: archive_core::Result<Arc<dyn Archive>>,
        password: Option<String>,
        mtm: MainThreadMarker,
    ) {
        if self.ivars().closed.get() {
            return;
        }
        self.set_busy(false, "");
        match result {
            Ok(archive) => {
                let roots = build_tree(archive.entries());
                *self.ivars().roots.borrow_mut() = roots;
                *self.ivars().archive.borrow_mut() = Some(archive);
                *self.ivars().password.borrow_mut() = password;
                if let Some(ov) = self.ivars().outline.get() {
                    ov.reloadData();
                    // Expand a single top-level folder so the user sees content immediately.
                    let roots = self.ivars().roots.borrow();
                    if roots.len() == 1 && roots[0].data().is_dir {
                        let item: &AnyObject = &roots[0];
                        unsafe { ov.expandItem(Some(item)) };
                    }
                }
                if let Some(b) = self.ivars().extract_button.get() {
                    b.setEnabled(true);
                }
                if let Some(p) = self.path() {
                    unsafe { self.window().setRepresentedURL(Some(&path_to_url(&p))) };
                }
                self.update_status();
            }
            Err(Error::PasswordRequired) | Err(Error::WrongPassword) => {
                let retry = password.is_some();
                let name = self.archive_name();
                let h = self.handle();
                sheets::ask_password(self.window(), &name, retry, mtm, move |pw| {
                    let h = h.clone();
                    h.on_main(move |doc, _| match pw {
                        Some(pw) => doc.load(Some(pw)),
                        None => doc.window().close(),
                    });
                });
            }
            Err(e) => {
                let name = self.archive_name();
                let h = self.handle();
                let w = self.window().retain();
                let a = objc2_app_kit::NSAlert::new(mtm);
                a.setMessageText(&ns(&format!("Could not open “{name}”")));
                a.setInformativeText(&ns(&e.to_string()));
                a.setAlertStyle(objc2_app_kit::NSAlertStyle::Critical);
                a.addButtonWithTitle(&ns("OK"));
                let block = RcBlock::new(move |_resp: NSModalResponse| {
                    h.on_main(|doc, _| doc.window().close());
                });
                a.beginSheetModalForWindow_completionHandler(&w, Some(&block));
            }
        }
    }

    fn set_busy(&self, busy: bool, text: &str) {
        if let Some(s) = self.ivars().spinner.get() {
            unsafe {
                if busy {
                    s.startAnimation(None)
                } else {
                    s.stopAnimation(None)
                }
            }
        }
        if busy {
            if let Some(l) = self.ivars().status.get() {
                l.setStringValue(&ns(text));
            }
        }
    }

    fn update_status(&self) {
        let Some(label) = self.ivars().status.get() else {
            return;
        };
        let archive = self.ivars().archive.borrow();
        let Some(archive) = archive.as_ref() else {
            return;
        };
        let entries = archive.entries();
        let files = entries.iter().filter(|e| !e.is_dir()).count();
        let total: Option<u64> = entries.iter().filter(|e| !e.is_dir()).map(|e| e.size).sum();
        let packed: Option<u64> = entries
            .iter()
            .filter(|e| !e.is_dir())
            .map(|e| e.packed_size)
            .sum();
        let mut s = format!(
            "{} {}, {}",
            files,
            if files == 1 { "file" } else { "files" },
            format_size(total)
        );
        if packed.is_some() {
            s.push_str(&format!(" ({} packed)", format_size(packed)));
        }
        s.push_str(&format!(" · {}", archive.format().display_name()));
        let sel = self.selected_nodes();
        if !sel.is_empty() {
            s.push_str(&format!(" · {} selected", sel.len()));
        }
        if archive.needs_password() && self.ivars().password.borrow().is_none() {
            s.push_str(" · encrypted");
        }
        label.setStringValue(&ns(&s));
    }

    // ---------------------------------------------------------- cells

    fn cell_view(
        &self,
        ov: &NSOutlineView,
        column: Option<&NSTableColumn>,
        item: &AnyObject,
    ) -> Option<Retained<NSView>> {
        let node = item.downcast_ref::<Node>()?;
        let column = column?;
        let ident = column.identifier();
        let id = ident.to_string();
        let d = node.data();
        let text = match id.as_str() {
            COL_NAME => d.name.clone(),
            COL_SIZE => format_size(node.total_size()),
            COL_PACKED => {
                if d.is_dir {
                    String::new()
                } else {
                    format_size(d.packed)
                }
            }
            COL_MODIFIED => format_date(d.modified),
            _ => String::new(),
        };
        let mtm = self.mtm();
        // Reuse a cell view if the table has one spare for this column.
        let existing = unsafe { ov.makeViewWithIdentifier_owner(&ident, None) };
        let cell: Retained<NSTableCellView> =
            match existing.and_then(|v| v.downcast::<NSTableCellView>().ok()) {
                Some(c) => c,
                None => make_cell_view(&ident, id == COL_NAME, id != COL_NAME, mtm),
            };
        if let Some(tf) = unsafe { cell.textField() } {
            tf.setStringValue(&ns(&text));
        }
        if id == COL_NAME {
            if let Some(iv) = unsafe { cell.imageView() } {
                iv.setImage(icon_for(&d.name, d.is_dir).as_deref());
            }
        }
        Some(Retained::into_super(cell))
    }

    // ---------------------------------------------------------- selection

    fn selected_nodes(&self) -> Vec<Retained<Node>> {
        let Some(ov) = self.ivars().outline.get() else {
            return vec![];
        };
        let indexes: Retained<NSIndexSet> = ov.selectedRowIndexes();
        let mut out = Vec::new();
        let mut i = indexes.firstIndex();
        while i != objc2_foundation::NSNotFound as usize {
            if let Some(item) = ov.itemAtRow(i as isize) {
                if let Some(n) = item.downcast_ref::<Node>() {
                    out.push(n.retain());
                }
            }
            i = indexes.indexGreaterThanIndex(i);
        }
        out
    }

    fn clicked_node(&self) -> Option<Retained<Node>> {
        let ov = self.ivars().outline.get()?;
        let row = ov.clickedRow();
        let row = if row < 0 { ov.selectedRow() } else { row };
        if row < 0 {
            return None;
        }
        let item = ov.itemAtRow(row)?;
        item.downcast_ref::<Node>().map(|n| n.retain())
    }

    // ---------------------------------------------------------- extract

    fn begin_extract(&self, all: bool) {
        if self.ivars().job.get().is_some() {
            return;
        }
        let Some(archive) = self.ivars().archive.borrow().clone() else {
            return;
        };
        let selection: Vec<PathBuf> = if all {
            vec![]
        } else {
            self.selected_nodes()
                .iter()
                .map(|n| n.data().path.clone())
                .collect()
        };
        let mtm = self.mtm();
        let archive_path = archive.path().to_path_buf();

        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseDirectories(true);
        panel.setCanChooseFiles(false);
        panel.setCanCreateDirectories(true);
        panel.setAllowsMultipleSelection(false);
        panel.setPrompt(Some(&ns("Extract")));
        let what = if selection.is_empty() {
            "all files".to_string()
        } else if selection.len() == 1 {
            format!(
                "“{}”",
                selection[0]
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            )
        } else {
            format!("{} items", selection.len())
        };
        panel.setMessage(Some(&ns(&format!(
            "Choose where to extract {what} from “{}”.",
            self.archive_name()
        ))));
        let start = self
            .ivars()
            .last_extract_dir
            .borrow()
            .clone()
            .unwrap_or_else(|| default_extract_dir(&archive_path));
        panel.setDirectoryURL(Some(&path_to_url(&start)));

        // Accessory: "Into a subfolder named after the archive" (only meaningful for extract-all).
        let roots_len = self.ivars().roots.borrow().len();
        let checkbox = unsafe {
            NSButton::checkboxWithTitle_target_action(
                &ns("Into a subfolder named after the archive"),
                None,
                None,
                mtm,
            )
        };
        checkbox.setState(if selection.is_empty() && roots_len > 1 {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
        checkbox.sizeToFit();
        let cb_frame = checkbox.frame();
        let acc = unsafe {
            NSView::initWithFrame(
                NSView::alloc(mtm),
                NSRect::new(
                    NSPoint::new(0.0, 0.0),
                    NSSize::new(cb_frame.size.width + 40.0, cb_frame.size.height + 16.0),
                ),
            )
        };
        checkbox.setFrameOrigin(NSPoint::new(20.0, 8.0));
        acc.addSubview(&checkbox);
        panel.setAccessoryView(Some(&acc));

        let h = self.handle();
        let panel_c = panel.clone();
        let checkbox_c = checkbox.clone();
        let block = RcBlock::new(move |resp: NSModalResponse| {
            if resp != NSModalResponseOK {
                return;
            }
            let Some(url) = panel_c.URL() else { return };
            let Some(dest) = url_to_path(&url) else {
                return;
            };
            let into_subfolder = checkbox_c.state() == NSControlStateValueOn;
            let selection = selection.clone();
            h.on_main(move |doc, _| {
                *doc.ivars().last_extract_dir.borrow_mut() = Some(dest.clone());
                doc.run_extract(selection, dest, into_subfolder);
            });
        });
        panel.beginSheetModalForWindow_completionHandler(self.window(), &block);
    }

    fn run_extract(&self, selection: Vec<PathBuf>, dest: PathBuf, into_subfolder: bool) {
        let Some(archive) = self.ivars().archive.borrow().clone() else {
            return;
        };
        if archive.needs_password() && self.ivars().password.borrow().is_none() {
            let name = self.archive_name();
            let h = self.handle();
            sheets::ask_password(self.window(), &name, false, self.mtm(), move |pw| {
                let (selection, dest) = (selection.clone(), dest.clone());
                h.on_main(move |doc, _| {
                    if let Some(pw) = pw {
                        *doc.ivars().password.borrow_mut() = Some(pw);
                        doc.run_extract(selection, dest, into_subfolder);
                    }
                });
            });
            return;
        }
        let password = self.ivars().password.borrow().clone();
        let cancel = Arc::new(Cancel::new());
        *self.ivars().cancel.borrow_mut() = Some(cancel.clone());
        self.ivars().job.set(Some(Job::Extract));
        self.show_progress("Preparing…", None);

        let h = self.handle();
        spawn("zipper-extract", move || {
            let progress_h = h.clone();
            let last = std::sync::Mutex::new(Instant::now() - Duration::from_secs(1));
            let progress = move |p: &Progress| {
                let mut l = last.lock().unwrap();
                if l.elapsed() < Duration::from_millis(80) {
                    return;
                }
                *l = Instant::now();
                let p = p.clone();
                progress_h.on_main(move |doc, _| doc.update_progress(&p));
            };
            let conflict_h = h.clone();
            let on_conflict = move |path: &Path| -> ConflictDecision {
                let shown = path.to_string_lossy().into_owned();
                conflict_h.ask(move |doc, mtm, reply| {
                    let parent = doc.sheet_parent_for_nested();
                    sheets::ask_conflict(&parent, &shown, mtm, move |d| reply.send(d));
                })
            };
            let opts = ExtractOptions {
                overwrite: Overwrite::Ask(&on_conflict),
                password,
                into_subfolder,
                preserve_mtime: true,
                strip_prefix: None,
            };
            let result = archive.extract(&selection, &dest, &opts, &progress, &cancel);
            h.on_main(move |doc, mtm| {
                doc.extract_finished(result, dest, into_subfolder, selection, mtm)
            });
        });
    }

    fn extract_finished(
        &self,
        result: archive_core::Result<ExtractReport>,
        dest: PathBuf,
        into_subfolder: bool,
        selection: Vec<PathBuf>,
        mtm: MainThreadMarker,
    ) {
        if self.ivars().closed.get() {
            return;
        }
        self.hide_progress();
        self.ivars().job.set(None);
        *self.ivars().cancel.borrow_mut() = None;
        match result {
            Ok(report) => {
                let root = if into_subfolder {
                    // archive-core puts everything under dest/<stem>; reveal that folder.
                    report
                        .written
                        .first()
                        .and_then(|w| w.strip_prefix(&dest).ok())
                        .and_then(|r| r.components().next())
                        .map(|c| dest.join(c.as_os_str()))
                        .unwrap_or(dest.clone())
                } else {
                    dest.clone()
                };
                let mut urls: Vec<Retained<NSURL>> = Vec::new();
                if report.written.len() <= 20 {
                    urls.extend(report.written.iter().map(|p| path_to_url(p)));
                }
                if urls.is_empty() {
                    urls.push(path_to_url(&root));
                }
                let arr = NSArray::from_retained_slice(&urls);
                NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(&arr);
                let mut notes = Vec::new();
                if !report.skipped_existing.is_empty() {
                    notes.push(format!(
                        "{} existing file(s) skipped",
                        report.skipped_existing.len()
                    ));
                }
                if !report.skipped_unsafe.is_empty() {
                    notes.push(format!(
                        "{} unsafe entry/entries not extracted",
                        report.skipped_unsafe.len()
                    ));
                }
                if let Some(l) = self.ivars().status.get() {
                    let mut s = format!(
                        "Extracted {} file(s) to {}",
                        report.written.len(),
                        root.display()
                    );
                    if !notes.is_empty() {
                        s.push_str(&format!(" — {}", notes.join(", ")));
                    }
                    l.setStringValue(&ns(&s));
                }
                if !report.skipped_unsafe.is_empty() {
                    sheets::alert(
                        Some(self.window()),
                        "Some entries were not extracted",
                        &format!(
                            "{} entries pointed outside the destination folder and were skipped for safety:\n{}",
                            report.skipped_unsafe.len(),
                            report.skipped_unsafe.iter().take(10).map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n")
                        ),
                        false,
                        mtm,
                    );
                }
            }
            Err(Error::Cancelled) => {
                self.update_status();
            }
            Err(Error::WrongPassword) | Err(Error::PasswordRequired) => {
                *self.ivars().password.borrow_mut() = None;
                let name = self.archive_name();
                let h = self.handle();
                sheets::ask_password(self.window(), &name, true, mtm, move |pw| {
                    let (selection, dest) = (selection.clone(), dest.clone());
                    h.on_main(move |doc, _| {
                        if let Some(pw) = pw {
                            *doc.ivars().password.borrow_mut() = Some(pw);
                            doc.run_extract(selection, dest, into_subfolder);
                        }
                    });
                });
            }
            Err(e) => {
                self.update_status();
                sheets::alert(
                    Some(self.window()),
                    "Extraction failed",
                    &e.to_string(),
                    true,
                    mtm,
                );
            }
        }
    }

    // ---------------------------------------------------------- open entry

    fn open_clicked_row(&self) {
        let Some(node) = self.clicked_node() else {
            return;
        };
        let Some(ov) = self.ivars().outline.get() else {
            return;
        };
        if node.data().is_dir {
            let item: &AnyObject = &node;
            unsafe {
                if ov.isItemExpanded(Some(item)) {
                    ov.collapseItem(Some(item));
                } else {
                    ov.expandItem(Some(item));
                }
            }
            return;
        }
        if self.ivars().job.get().is_some() {
            return;
        }
        let Some(archive) = self.ivars().archive.borrow().clone() else {
            return;
        };
        let temp = self.temp_dir();
        let entry_path = node.data().path.clone();
        let target = temp.join(&entry_path);
        if target.is_file() {
            // Already extracted once this session: reuse.
            NSWorkspace::sharedWorkspace().openURL(&path_to_url(&target));
            return;
        }
        if archive.needs_password() && self.ivars().password.borrow().is_none() {
            let name = self.archive_name();
            let h = self.handle();
            sheets::ask_password(self.window(), &name, false, self.mtm(), move |pw| {
                h.on_main(move |doc, _| {
                    if let Some(pw) = pw {
                        *doc.ivars().password.borrow_mut() = Some(pw);
                        doc.open_clicked_row();
                    }
                });
            });
            return;
        }
        let password = self.ivars().password.borrow().clone();
        let cancel = Arc::new(Cancel::new());
        *self.ivars().cancel.borrow_mut() = Some(cancel.clone());
        self.ivars().job.set(Some(Job::OpenEntry));
        self.show_progress(&format!("Opening {}…", node.data().name), None);
        let h = self.handle();
        spawn("zipper-open-entry", move || {
            let progress_h = h.clone();
            let progress = move |p: &Progress| {
                let p = p.clone();
                progress_h.on_main(move |doc, _| doc.update_progress(&p));
            };
            let opts = ExtractOptions {
                overwrite: Overwrite::Always(ConflictAction::Replace),
                password,
                into_subfolder: false,
                preserve_mtime: true,
                strip_prefix: None,
            };
            let result = archive
                .extract(&[entry_path], &temp, &opts, &progress, &cancel)
                .map(|_| target);
            h.on_main(move |doc, mtm| doc.open_entry_finished(result, mtm));
        });
    }

    fn open_entry_finished(&self, result: archive_core::Result<PathBuf>, mtm: MainThreadMarker) {
        if self.ivars().closed.get() {
            return;
        }
        self.hide_progress();
        self.ivars().job.set(None);
        *self.ivars().cancel.borrow_mut() = None;
        self.update_status();
        match result {
            Ok(target) => {
                NSWorkspace::sharedWorkspace().openURL(&path_to_url(&target));
            }
            Err(Error::Cancelled) => {}
            Err(Error::WrongPassword) | Err(Error::PasswordRequired) => {
                *self.ivars().password.borrow_mut() = None;
                let name = self.archive_name();
                let h = self.handle();
                sheets::ask_password(self.window(), &name, true, mtm, move |pw| {
                    h.on_main(move |doc, _| {
                        if let Some(pw) = pw {
                            *doc.ivars().password.borrow_mut() = Some(pw);
                            doc.open_clicked_row();
                        }
                    });
                });
            }
            Err(e) => sheets::alert(
                Some(self.window()),
                "Could not open file",
                &e.to_string(),
                true,
                mtm,
            ),
        }
    }

    fn temp_dir(&self) -> PathBuf {
        let mut td = self.ivars().temp_dir.borrow_mut();
        if let Some(t) = td.as_ref() {
            return t.clone();
        }
        let p = temp_dir_for(self.path().as_deref().unwrap_or(Path::new("archive")));
        *td = Some(p.clone());
        p
    }

    // ---------------------------------------------------------- test

    fn begin_test(&self) {
        if self.ivars().job.get().is_some() {
            return;
        }
        let Some(archive) = self.ivars().archive.borrow().clone() else {
            return;
        };
        if archive.needs_password() && self.ivars().password.borrow().is_none() {
            let name = self.archive_name();
            let h = self.handle();
            sheets::ask_password(self.window(), &name, false, self.mtm(), move |pw| {
                h.on_main(move |doc, _| {
                    if let Some(pw) = pw {
                        *doc.ivars().password.borrow_mut() = Some(pw);
                        doc.begin_test();
                    }
                });
            });
            return;
        }
        let password = self.ivars().password.borrow().clone();
        let cancel = Arc::new(Cancel::new());
        *self.ivars().cancel.borrow_mut() = Some(cancel.clone());
        self.ivars().job.set(Some(Job::Test));
        self.show_progress("Testing…", None);
        let h = self.handle();
        spawn("zipper-test", move || {
            let progress_h = h.clone();
            let last = std::sync::Mutex::new(Instant::now() - Duration::from_secs(1));
            let progress = move |p: &Progress| {
                let mut l = last.lock().unwrap();
                if l.elapsed() < Duration::from_millis(80) {
                    return;
                }
                *l = Instant::now();
                let p = p.clone();
                progress_h.on_main(move |doc, _| doc.update_progress(&p));
            };
            let result = archive.test(password.as_deref(), &progress, &cancel);
            h.on_main(move |doc, mtm| doc.test_finished(result, mtm));
        });
    }

    fn test_finished(&self, result: archive_core::Result<()>, mtm: MainThreadMarker) {
        if self.ivars().closed.get() {
            return;
        }
        self.hide_progress();
        self.ivars().job.set(None);
        *self.ivars().cancel.borrow_mut() = None;
        self.update_status();
        match result {
            Ok(()) => sheets::alert(
                Some(self.window()),
                "Archive OK",
                &format!("“{}” passed the integrity test.", self.archive_name()),
                false,
                mtm,
            ),
            Err(Error::Cancelled) => {}
            Err(Error::WrongPassword) | Err(Error::PasswordRequired) => {
                *self.ivars().password.borrow_mut() = None;
                let name = self.archive_name();
                let h = self.handle();
                sheets::ask_password(self.window(), &name, true, mtm, move |pw| {
                    h.on_main(move |doc, _| {
                        if let Some(pw) = pw {
                            *doc.ivars().password.borrow_mut() = Some(pw);
                            doc.begin_test();
                        }
                    });
                });
            }
            Err(e) => sheets::alert(
                Some(self.window()),
                "Archive test failed",
                &e.to_string(),
                true,
                mtm,
            ),
        }
    }

    // ---------------------------------------------------------- progress sheet

    fn progress_ui(&self) -> &ProgressUi {
        self.ivars().progress.get_or_init(|| {
            let mtm = self.mtm();
            let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(440.0, 110.0));
            let panel = unsafe {
                NSPanel::initWithContentRect_styleMask_backing_defer(
                    NSPanel::alloc(mtm),
                    frame,
                    NSWindowStyleMask::Titled,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            unsafe { panel.setReleasedWhenClosed(false) };
            let content = panel.contentView().expect("content view");
            let label = NSTextField::labelWithString(&ns("Preparing…"), mtm);
            label.setFrame(NSRect::new(
                NSPoint::new(20.0, 70.0),
                NSSize::new(400.0, 20.0),
            ));
            unsafe { label.setUsesSingleLineMode(true) };
            label.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingMiddle);
            content.addSubview(&label);
            let bar = unsafe {
                NSProgressIndicator::initWithFrame(
                    NSProgressIndicator::alloc(mtm),
                    NSRect::new(NSPoint::new(20.0, 44.0), NSSize::new(400.0, 20.0)),
                )
            };
            bar.setStyle(NSProgressIndicatorStyle::Bar);
            bar.setIndeterminate(true);
            bar.setMinValue(0.0);
            bar.setMaxValue(1.0);
            content.addSubview(&bar);
            let me: &AnyObject = self;
            let cancel = unsafe {
                NSButton::buttonWithTitle_target_action(
                    &ns("Cancel"),
                    Some(me),
                    Some(sel!(cancelTask:)),
                    mtm,
                )
            };
            cancel.setBezelStyle(NSBezelStyle::Rounded);
            cancel.setFrame(NSRect::new(
                NSPoint::new(330.0, 8.0),
                NSSize::new(90.0, 28.0),
            ));
            cancel.setKeyEquivalent(ns_string!("\u{1b}"));
            content.addSubview(&cancel);
            ProgressUi {
                panel,
                label,
                bar,
                shown: Cell::new(false),
                shown_at: Cell::new(None),
            }
        })
    }

    fn show_progress(&self, text: &str, fraction: Option<f64>) {
        let ui = self.progress_ui();
        ui.label.setStringValue(&ns(text));
        match fraction {
            Some(f) => {
                ui.bar.setIndeterminate(false);
                ui.bar.setDoubleValue(f);
            }
            None => {
                ui.bar.setIndeterminate(true);
                unsafe { ui.bar.startAnimation(None) };
            }
        }
        if !ui.shown.get() {
            ui.shown.set(true);
            ui.shown_at.set(Some(Instant::now()));
            self.window().beginSheet_completionHandler(&ui.panel, None);
        }
        self.set_busy(true, text);
    }

    fn hide_progress(&self) {
        let ui = self.progress_ui();
        if ui.shown.get() {
            ui.shown.set(false);
            unsafe { ui.bar.stopAnimation(None) };
            self.window().endSheet(&ui.panel);
        }
        self.set_busy(false, "");
    }

    fn update_progress(&self, p: &Progress) {
        if self.ivars().job.get().is_none() {
            return;
        }
        let ui = self.progress_ui();
        let name = p
            .current
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let text = match p.total {
            Some(t) if t > 0 => format!(
                "{} of {} — {}",
                format_size(Some(p.done)),
                format_size(Some(t)),
                name
            ),
            _ => format!("{} — {}", format_size(Some(p.done)), name),
        };
        ui.label.setStringValue(&ns(&text));
        match p.total {
            Some(t) if t > 0 => {
                if ui.bar.isIndeterminate() {
                    unsafe { ui.bar.stopAnimation(None) };
                    ui.bar.setIndeterminate(false);
                }
                ui.bar.setDoubleValue(p.done as f64 / t as f64);
            }
            _ => {}
        }
    }

    /// Window to attach a nested sheet (conflict prompt) to: the progress
    /// panel when it is up, else the main window.
    fn sheet_parent_for_nested(&self) -> Retained<NSWindow> {
        let ui = self.progress_ui();
        if ui.shown.get() {
            Retained::into_super(ui.panel.clone())
        } else {
            self.window().retain()
        }
    }

    // ---------------------------------------------------------- closing

    fn close_cleanup(&self) {
        if self.ivars().closed.replace(true) {
            return;
        }
        if let Some(c) = self.ivars().cancel.borrow().as_ref() {
            c.cancel();
        }
        self.remove_temp_dir();
        crate::app::forget_document(self);
    }

    pub fn remove_temp_dir(&self) {
        if let Some(t) = self.ivars().temp_dir.borrow_mut().take() {
            let _ = std::fs::remove_dir_all(&t);
        }
    }

    pub fn cancel_current(&self) {
        if let Some(c) = self.ivars().cancel.borrow().as_ref() {
            c.cancel();
        }
    }
}

// ------------------------------------------------------------------ helpers

fn make_column(
    id: &str,
    title: &str,
    width: f64,
    min: f64,
    mtm: MainThreadMarker,
) -> Retained<NSTableColumn> {
    let ident = ns(id);
    let col = NSTableColumn::initWithIdentifier(NSTableColumn::alloc(mtm), &ident);
    col.setTitle(&ns(title));
    col.setWidth(width);
    col.setMinWidth(min);
    col.setResizingMask(
        NSTableColumnResizingOptions::UserResizingMask
            | NSTableColumnResizingOptions::AutoresizingMask,
    );
    let proto = NSSortDescriptor::sortDescriptorWithKey_ascending(Some(&ident), true);
    col.setSortDescriptorPrototype(Some(&proto));
    col
}

fn make_cell_view(
    ident: &NSString,
    with_icon: bool,
    right_align: bool,
    mtm: MainThreadMarker,
) -> Retained<NSTableCellView> {
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(200.0, 20.0));
    let cell = unsafe { NSTableCellView::initWithFrame(NSTableCellView::alloc(mtm), frame) };
    cell.setIdentifier(Some(ident));
    let tf = NSTextField::labelWithString(&ns(""), mtm);
    unsafe { tf.setUsesSingleLineMode(true) };
    tf.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingMiddle);
    if right_align {
        tf.setAlignment(objc2_app_kit::NSTextAlignment::Right);
        tf.setFrame(NSRect::new(
            NSPoint::new(2.0, 1.0),
            NSSize::new(196.0, 18.0),
        ));
        tf.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
    } else if with_icon {
        tf.setFrame(NSRect::new(
            NSPoint::new(22.0, 1.0),
            NSSize::new(176.0, 18.0),
        ));
        tf.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
    } else {
        tf.setFrame(NSRect::new(
            NSPoint::new(2.0, 1.0),
            NSSize::new(196.0, 18.0),
        ));
        tf.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
    }
    cell.addSubview(&tf);
    unsafe { cell.setTextField(Some(&tf)) };
    if with_icon {
        let iv = unsafe {
            NSImageView::initWithFrame(
                NSImageView::alloc(mtm),
                NSRect::new(NSPoint::new(2.0, 2.0), NSSize::new(16.0, 16.0)),
            )
        };
        iv.setImageScaling(objc2_app_kit::NSImageScaling::ScaleProportionallyUpOrDown);
        cell.addSubview(&iv);
        unsafe { cell.setImageView(Some(&iv)) };
    }
    cell
}

/// Folder / document icons as SF Symbols. This deliberately avoids the
/// deprecated `-[NSWorkspace iconForFileType:]` (Icon Services), which is a
/// suspect in a HIServices crash seen on macOS 26/27.
fn icon_for(name: &str, is_dir: bool) -> Option<Retained<objc2_app_kit::NSImage>> {
    let symbol = if is_dir {
        "folder.fill"
    } else {
        let ext = Path::new(name)
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "heic" | "tif" | "tiff" | "bmp" | "webp" | "svg" => {
                "photo"
            }
            "pdf" => "doc.richtext",
            "txt" | "md" | "log" | "csv" | "json" | "xml" | "yaml" | "yml" | "toml" => "doc.text",
            "zip" | "7z" | "tar" | "gz" | "xz" | "bz2" | "zst" | "rar" => "doc.zipper",
            "mp3" | "wav" | "flac" | "m4a" | "aac" => "music.note",
            "mp4" | "mov" | "mkv" | "avi" => "film",
            "app" | "exe" | "dmg" | "pkg" => "shippingbox",
            _ => "doc",
        }
    };
    unsafe {
        objc2_app_kit::NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &ns(symbol),
            None,
        )
    }
}
