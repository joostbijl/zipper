//! One archive = one window. The `Document` object is the window's
//! delegate, the outline view's data source/delegate, the file-promise
//! delegate for drag-out, and the target of the window's actions.
//!
//! Two kinds of document:
//! - an opened archive (`path` set, `archive` loaded); edits on a writable
//!   format (zip, 7z) are applied immediately through `write::modify`;
//! - a new, unsaved archive (`path` None): dropped files are kept as
//!   `pending` sources and written on Save.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{
    define_class, msg_send, sel, AnyThread, ClassType, DefinedClass, MainThreadMarker,
    MainThreadOnly, Message,
};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBackingStoreType, NSBezelStyle, NSButton, NSControlStateValueOff,
    NSControlStateValueOn, NSControlTextEditingDelegate, NSDragOperation, NSDraggingInfo,
    NSFilePromiseProvider, NSFilePromiseProviderDelegate, NSFilenamesPboardType, NSImageView,
    NSMenuItem, NSMenuItemValidation, NSModalResponse, NSModalResponseOK, NSOpenPanel,
    NSOutlineView, NSOutlineViewDataSource, NSOutlineViewDelegate, NSOutlineViewDropOnItemIndex,
    NSPanel, NSPasteboardWriting, NSPopUpButton, NSProgressIndicator, NSProgressIndicatorStyle,
    NSSavePanel, NSScrollView, NSSecureTextField, NSTableCellView, NSTableColumn,
    NSTableColumnResizingOptions, NSTableViewColumnAutoresizingStyle, NSTableViewStyle,
    NSTextField, NSUserInterfaceItemIdentification, NSView, NSWindow, NSWindowDelegate,
    NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{
    ns_string, NSArray, NSError, NSIndexSet, NSNotification, NSObject, NSObjectProtocol,
    NSOperationQueue, NSPoint, NSRect, NSSize, NSSortDescriptor, NSString, NSURL,
};

use crate::quicklook::{QLPreviewItem, QLPreviewPanel, QLPreviewPanelDataSource};

use crate::prefs::{ExtractMode, Prefs};
use archive_core::write::{self, Edit, Source, WriteOptions};
use archive_core::{
    Archive, Cancel, ConflictAction, ConflictDecision, Entry, EntryKind, Error, ExtractOptions,
    ExtractReport, Format, Overwrite, Progress,
};

use crate::node::{build_tree_with, sort_tree, Node, SortKey};
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
    Modify,
    Save,
    Promise,
    QuickLook,
}

struct ProgressUi {
    panel: Retained<NSPanel>,
    label: Retained<NSTextField>,
    bar: Retained<NSProgressIndicator>,
    shown: Cell<bool>,
}

/// Controls of the Save panel accessory view.
struct SaveUi {
    format: Retained<NSPopUpButton>,
    level: Retained<NSPopUpButton>,
    password: Retained<NSSecureTextField>,
    split: Retained<NSPopUpButton>,
}

type PromiseBlock = RcBlock<dyn Fn(*mut NSError)>;

#[derive(Default)]
pub struct DocIvars {
    window: OnceCell<Retained<NSWindow>>,
    outline: OnceCell<Retained<NSOutlineView>>,
    status: OnceCell<Retained<NSTextField>>,
    extract_button: OnceCell<Retained<NSButton>>,
    spinner: OnceCell<Retained<NSProgressIndicator>>,
    progress: OnceCell<ProgressUi>,
    save_ui: RefCell<Option<SaveUi>>,

    path: RefCell<Option<PathBuf>>,
    archive: RefCell<Option<Arc<dyn Archive>>>,
    /// New-document sources (top level), written on Save.
    pending: RefCell<Vec<Source>>,
    roots: RefCell<Vec<Retained<Node>>>,
    password: RefCell<Option<String>>,
    temp_dir: RefCell<Option<PathBuf>>,
    last_extract_dir: RefCell<Option<PathBuf>>,

    cancel: RefCell<Option<Arc<Cancel>>>,
    job: Cell<Option<Job>>,
    closed: Cell<bool>,

    /// Drag-out promises waiting for their extraction to finish.
    promises: RefCell<HashMap<u64, PromiseBlock>>,
    next_promise: Cell<u64>,

    /// Files currently shown in the Quick Look panel.
    quick_look: RefCell<Vec<PathBuf>>,
    /// Set by the Finder "Extract with Zipper" service: extract everything
    /// as soon as the archive has loaded.
    extract_on_load: Cell<bool>,
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

        #[unsafe(method(addFiles:))]
        fn add_files_action(&self, _sender: Option<&AnyObject>) {
            self.begin_add_files();
        }

        #[unsafe(method(deleteSelected:))]
        fn delete_action(&self, _sender: Option<&AnyObject>) {
            self.begin_delete();
        }

        #[unsafe(method(renameSelected:))]
        fn rename_action(&self, _sender: Option<&AnyObject>) {
            self.begin_rename();
        }

        #[unsafe(method(renameCommitted:))]
        fn rename_committed(&self, sender: Option<&AnyObject>) {
            if let Some(tf) = sender.and_then(|s| s.downcast_ref::<NSTextField>()) {
                self.rename_from_field(tf);
            }
        }

        #[unsafe(method(saveDocument:))]
        fn save_action(&self, _sender: Option<&AnyObject>) {
            self.begin_save();
        }

        #[unsafe(method(saveFormatChanged:))]
        fn save_format_changed(&self, _sender: Option<&AnyObject>) {
            self.sync_save_ui();
        }

        #[unsafe(method(quickLookSelected:))]
        fn quick_look_action(&self, _sender: Option<&AnyObject>) {
            self.toggle_quick_look();
        }

        /// Sent by NSTableView/NSOutlineView when the space bar is pressed.
        #[unsafe(method(quickLookPreviewItems:))]
        fn quick_look_preview_items(&self, _sender: Option<&AnyObject>) {
            self.toggle_quick_look();
        }

        // QLPreviewPanel controller protocol (informal, on NSObject).
        #[unsafe(method(acceptsPreviewPanelControl:))]
        fn accepts_preview_panel_control(&self, _panel: Option<&QLPreviewPanel>) -> bool {
            true
        }

        #[unsafe(method(beginPreviewPanelControl:))]
        fn begin_preview_panel_control(&self, panel: Option<&QLPreviewPanel>) {
            if let Some(p) = panel {
                unsafe { p.setDataSource(Some(ProtocolObject::from_ref(self))) };
            }
        }

        #[unsafe(method(endPreviewPanelControl:))]
        fn end_preview_panel_control(&self, panel: Option<&QLPreviewPanel>) {
            if let Some(p) = panel {
                unsafe { p.setDataSource(None) };
            }
        }
    }

    unsafe impl QLPreviewPanelDataSource for Document {
        #[unsafe(method(numberOfPreviewItemsInPreviewPanel:))]
        unsafe fn number_of_preview_items(&self, _panel: Option<&QLPreviewPanel>) -> isize {
            self.ivars().quick_look.borrow().len() as isize
        }

        #[unsafe(method_id(previewPanel:previewItemAtIndex:))]
        unsafe fn preview_item_at(&self, _panel: Option<&QLPreviewPanel>, index: isize) -> Option<Retained<ProtocolObject<dyn QLPreviewItem>>> {
            self.ivars()
                .quick_look
                .borrow()
                .get(index as usize)
                .map(|p| ProtocolObject::from_retained(path_to_url(p)))
        }
    }

    unsafe impl NSMenuItemValidation for Document {
        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, item: &NSMenuItem) -> bool {
            self.validate(item)
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

        // Drag out of the window.
        #[unsafe(method_id(outlineView:pasteboardWriterForItem:))]
        unsafe fn pasteboard_writer(&self, _ov: &NSOutlineView, item: &AnyObject) -> Option<Retained<ProtocolObject<dyn NSPasteboardWriting>>> {
            self.pasteboard_writer_for(item)
        }

        // Drop into the window.
        #[unsafe(method(outlineView:validateDrop:proposedItem:proposedChildIndex:))]
        unsafe fn validate_drop(&self, ov: &NSOutlineView, info: &ProtocolObject<dyn NSDraggingInfo>, item: Option<&AnyObject>, _index: isize) -> NSDragOperation {
            if !self.can_edit() || self.ivars().job.get().is_some() {
                return NSDragOperation::None;
            }
            if crate::app::dragged_paths(info).is_empty() {
                return NSDragOperation::None;
            }
            // Retarget onto the folder row (or the whole table).
            let target = match item.and_then(|i| i.downcast_ref::<Node>()) {
                Some(n) if n.data().is_dir => Some(n.retain()),
                Some(n) => unsafe { ov.parentForItem(Some(n)) }.and_then(|p| p.downcast::<Node>().ok()),
                None => None,
            };
            let target_obj: Option<&AnyObject> = target.as_deref().map(|n| n as &AnyObject);
            unsafe { ov.setDropItem_dropChildIndex(target_obj, NSOutlineViewDropOnItemIndex as isize) };
            NSDragOperation::Copy
        }

        #[unsafe(method(outlineView:acceptDrop:item:childIndex:))]
        #[allow(clippy::useless_conversion)]
        unsafe fn accept_drop(&self, _ov: &NSOutlineView, info: &ProtocolObject<dyn NSDraggingInfo>, item: Option<&AnyObject>, _index: isize) -> bool {
            let paths = crate::app::dragged_paths(info);
            if paths.is_empty() {
                return false.into();
            }
            let under = item
                .and_then(|i| i.downcast_ref::<Node>())
                .filter(|n| n.data().is_dir)
                .map(|n| n.data().path.clone());
            self.add_sources(paths, under);
            true.into()
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

    // ---------------------------------------------------------- drag-out promises
    unsafe impl NSFilePromiseProviderDelegate for Document {
        #[unsafe(method_id(filePromiseProvider:fileNameForType:))]
        fn promise_file_name(&self, provider: &NSFilePromiseProvider, _file_type: &NSString) -> Retained<NSString> {
            let path = promise_path(provider);
            ns(&path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into()))
        }

        #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
        fn promise_write(&self, provider: &NSFilePromiseProvider, url: &NSURL, handler: &block2::DynBlock<dyn Fn(*mut NSError)>) {
            self.write_promise(provider, url, handler);
        }

        #[unsafe(method_id(operationQueueForFilePromiseProvider:))]
        fn promise_queue(&self, _provider: &NSFilePromiseProvider) -> Retained<NSOperationQueue> {
            // Deliver on the main thread; the actual extraction runs on a worker.
            NSOperationQueue::mainQueue()
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

    /// Open and, once loaded, start "Extract All" (Finder service).
    pub fn open_then_extract(path: PathBuf, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::open(path, mtm);
        this.ivars().extract_on_load.set(true);
        this
    }

    /// Show the Save panel (Finder "Add to Archive" service).
    pub fn save_now(&self) {
        self.begin_save();
    }

    /// A new, unsaved archive, optionally pre-filled with files.
    pub fn new_untitled(sources: Vec<PathBuf>, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DocIvars::default());
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        this.build_window(mtm);
        this.window().setTitle(ns_string!("Untitled archive"));
        if let Some(l) = this.ivars().status.get() {
            l.setStringValue(ns_string!(
                "Drop files here or use Archive → Add Files…, then Save (⌘S)."
            ));
        }
        if !sources.is_empty() {
            this.add_sources(sources, None);
        }
        this
    }

    pub fn window(&self) -> &NSWindow {
        self.ivars().window.get().expect("window built")
    }

    pub fn path(&self) -> Option<PathBuf> {
        self.ivars().path.borrow().clone()
    }

    fn is_new(&self) -> bool {
        self.ivars().path.borrow().is_none()
    }

    /// Writable format, or a new document.
    fn can_edit(&self) -> bool {
        if self.is_new() {
            return true;
        }
        self.ivars()
            .archive
            .borrow()
            .as_ref()
            .map(|a| a.format().is_writable())
            .unwrap_or(false)
    }

    fn handle(&self) -> MainHandle<Self> {
        MainHandle::new(self.retain(), self.mtm())
    }

    fn archive_name(&self) -> String {
        self.path()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "Untitled archive".into())
    }

    fn busy(&self) -> bool {
        self.ivars().job.get().is_some()
    }

    fn validate(&self, item: &NSMenuItem) -> bool {
        let Some(action) = item.action() else {
            return true;
        };
        let has_archive = self.ivars().archive.borrow().is_some();
        let has_selection = self
            .ivars()
            .outline
            .get()
            .map(|ov| ov.selectedRow() >= 0)
            .unwrap_or(false);
        let busy = self.busy();
        if action == sel!(extractSelected:)
            || action == sel!(extractAll:)
            || action == sel!(testArchive:)
        {
            has_archive && !busy
        } else if action == sel!(openSelected:) {
            (has_archive || self.is_new()) && has_selection && !busy
        } else if action == sel!(addFiles:) {
            self.can_edit() && !busy
        } else if action == sel!(deleteSelected:) || action == sel!(renameSelected:) {
            self.can_edit() && has_selection && !busy
        } else if action == sel!(saveDocument:) {
            self.is_new() && !self.ivars().pending.borrow().is_empty() && !busy
        } else if action == sel!(quickLookSelected:) {
            // Never steal the space bar from a text field being edited.
            let editing = self
                .window()
                .firstResponder()
                .map(|r| r.isKindOfClass(objc2_app_kit::NSText::class()))
                .unwrap_or(false);
            has_selection && !editing
        } else {
            true
        }
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
        outline.setUsesAlternatingRowBackgroundColors(false);
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
        // Drag out (file promises) and drop in (Finder files).
        outline.setDraggingSourceOperationMask_forLocal(NSDragOperation::Copy, false);
        outline.registerForDraggedTypes(&NSArray::from_slice(&[unsafe { NSFilenamesPboardType }]));

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
        status.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingMiddle);
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
                let first_load = self.ivars().archive.borrow().is_none();
                *self.ivars().archive.borrow_mut() = Some(archive);
                *self.ivars().password.borrow_mut() = password;
                self.rebuild_tree();
                if first_load {
                    if let Some(ov) = self.ivars().outline.get() {
                        // Expand a single top-level folder so the user sees content immediately.
                        let roots = self.ivars().roots.borrow();
                        if roots.len() == 1 && roots[0].data().is_dir {
                            let item: &AnyObject = &roots[0];
                            unsafe { ov.expandItem(Some(item)) };
                        }
                    }
                }
                if let Some(b) = self.ivars().extract_button.get() {
                    b.setEnabled(true);
                }
                if let Some(p) = self.path() {
                    self.window().setTitle(&ns(&self.archive_name()));
                    unsafe { self.window().setRepresentedURL(Some(&path_to_url(&p))) };
                }
                self.update_status();
                if self.ivars().extract_on_load.replace(false) {
                    self.begin_extract(true);
                }
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

    /// Rebuild the node tree from the archive entries and/or pending sources,
    /// keeping the current sort order and expansion of top-level folders.
    fn rebuild_tree(&self) {
        let mut roots = {
            let archive = self.ivars().archive.borrow();
            let pending = self.ivars().pending.borrow();
            let pending_items = write::list_sources(&pending).unwrap_or_default();
            let pending_entries: Vec<(Entry, PathBuf)> = pending_items
                .into_iter()
                .filter_map(|i| {
                    let path = archive_core::safe_path(&i.name).ok()?;
                    Some((
                        Entry {
                            path,
                            kind: i.kind,
                            size: Some(i.size),
                            packed_size: None,
                            modified: i.modified,
                            encrypted: false,
                        },
                        i.disk,
                    ))
                })
                .collect();
            let archive_entries = archive.as_ref().map(|a| a.entries()).unwrap_or(&[]);
            build_tree_with(
                archive_entries
                    .iter()
                    .map(|e| (e, None))
                    .chain(pending_entries.iter().map(|(e, d)| (e, Some(d.clone())))),
            )
        };
        if let Some(ov) = self.ivars().outline.get() {
            if let Some(d) = ov.sortDescriptors().firstObject() {
                let key = d.key().map(|k| k.to_string()).unwrap_or_default();
                if let Some(k) = SortKey::from_identifier(&key) {
                    sort_tree(&mut roots, k, d.ascending());
                }
            }
            // Remember which top-level folders were expanded, by name.
            let expanded: Vec<String> = self
                .ivars()
                .roots
                .borrow()
                .iter()
                .filter(|n| {
                    let item: &AnyObject = n;
                    unsafe { ov.isItemExpanded(Some(item)) }
                })
                .map(|n| n.data().name.clone())
                .collect();
            *self.ivars().roots.borrow_mut() = roots;
            ov.reloadData();
            let roots = self.ivars().roots.borrow();
            for n in roots.iter().filter(|n| expanded.contains(&n.data().name)) {
                let item: &AnyObject = n;
                unsafe { ov.expandItem(Some(item)) };
            }
        } else {
            *self.ivars().roots.borrow_mut() = roots;
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
        let sel = self.selected_nodes();
        let mut s;
        if let Some(archive) = archive.as_ref() {
            let entries = archive.entries();
            let files = entries.iter().filter(|e| !e.is_dir()).count();
            let total: Option<u64> = entries.iter().filter(|e| !e.is_dir()).map(|e| e.size).sum();
            let packed: Option<u64> = entries
                .iter()
                .filter(|e| !e.is_dir())
                .map(|e| e.packed_size)
                .sum();
            s = format!(
                "{} {}, {}",
                files,
                if files == 1 { "file" } else { "files" },
                format_size(total)
            );
            if packed.is_some() {
                s.push_str(&format!(" ({} packed)", format_size(packed)));
            }
            s.push_str(&format!(" · {}", archive.format().display_name()));
            if !archive.format().is_writable() {
                s.push_str(" · read-only");
            }
            if archive.needs_password() && self.ivars().password.borrow().is_none() {
                s.push_str(" · encrypted");
            }
        } else {
            let pending = self.ivars().pending.borrow();
            if pending.is_empty() {
                s = "Drop files here or use Archive → Add Files…, then Save (⌘S).".into();
            } else {
                let total: u64 = self
                    .ivars()
                    .roots
                    .borrow()
                    .iter()
                    .filter_map(|n| n.total_size())
                    .sum();
                s = format!(
                    "{} item(s), {} — not saved yet (⌘S)",
                    pending.len(),
                    format_size(Some(total))
                );
            }
        }
        if !sel.is_empty() {
            s.push_str(&format!(" · {} selected", sel.len()));
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
                None => {
                    let me: &AnyObject = self;
                    make_cell_view(&ident, id == COL_NAME, id != COL_NAME, me, mtm)
                }
            };
        if let Some(tf) = unsafe { cell.textField() } {
            tf.setStringValue(&ns(&text));
            if id == COL_NAME {
                tf.setEditable(self.can_edit());
            }
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
        if self.busy() {
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
        let roots_len = self.ivars().roots.borrow().len();

        // Preferences may skip the folder question.
        let prefs = Prefs::load();
        let fixed_dest = match prefs.extract_mode {
            ExtractMode::Ask => None,
            ExtractMode::ArchiveFolder => Some(default_extract_dir(&archive_path)),
            ExtractMode::Fixed => prefs.extract_folder.clone(),
        };
        if let Some(dest) = fixed_dest {
            let into_subfolder = selection.is_empty() && roots_len > 1;
            self.run_extract(selection, dest, into_subfolder);
            return;
        }

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

        // Accessory: "Into a subfolder named after the archive".
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
        let cancel = self.start_job(Job::Extract, "Preparing…");

        let h = self.handle();
        spawn("zipper-extract", move || {
            let progress = throttled_progress(h.clone());
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
        self.end_job();
        match result {
            Ok(report) => {
                let root = if into_subfolder {
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
                let prefs = Prefs::load();
                if prefs.reveal {
                    let arr = NSArray::from_retained_slice(&urls);
                    NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(&arr);
                }
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
                if prefs.trash_after_extract
                    && selection.is_empty()
                    && report.skipped_unsafe.is_empty()
                {
                    if let Some(p) = self.path() {
                        let fm = objc2_foundation::NSFileManager::defaultManager();
                        let ok = unsafe {
                            fm.trashItemAtURL_resultingItemURL_error(&path_to_url(&p), None)
                        }
                        .is_ok();
                        if ok {
                            self.remove_temp_dir();
                            self.window().close();
                            return;
                        }
                    }
                }
                if !report.skipped_unsafe.is_empty() {
                    sheets::alert(
                        Some(self.window()),
                        "Some entries were not extracted",
                        &format!(
                            "{} entries pointed outside the destination folder and were skipped for safety:\n{}",
                            report.skipped_unsafe.len(),
                            report
                                .skipped_unsafe
                                .iter()
                                .take(10)
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>()
                                .join("\n")
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
        // Not yet in an archive: open the file where it lives.
        if let Some(disk) = &node.data().disk {
            NSWorkspace::sharedWorkspace().openURL(&path_to_url(disk));
            return;
        }
        if self.busy() {
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
        let cancel = self.start_job(Job::OpenEntry, &format!("Opening {}…", node.data().name));
        let h = self.handle();
        spawn("zipper-open-entry", move || {
            let progress = throttled_progress(h.clone());
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
        self.end_job();
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
        if self.busy() {
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
        let cancel = self.start_job(Job::Test, "Testing…");
        let h = self.handle();
        spawn("zipper-test", move || {
            let progress = throttled_progress(h.clone());
            let result = archive.test(password.as_deref(), &progress, &cancel);
            h.on_main(move |doc, mtm| doc.test_finished(result, mtm));
        });
    }

    fn test_finished(&self, result: archive_core::Result<()>, mtm: MainThreadMarker) {
        if self.ivars().closed.get() {
            return;
        }
        self.end_job();
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

    // ---------------------------------------------------------- editing

    /// Add files from disk, either to the pending list (new document) or
    /// to the archive via a rewrite. `under` is the folder inside the
    /// archive to place them in.
    pub fn add_sources(&self, paths: Vec<PathBuf>, under: Option<PathBuf>) {
        if !self.can_edit() || self.busy() {
            return;
        }
        let sources: Vec<Source> = paths
            .iter()
            .filter_map(|p| {
                let name = p.file_name()?.to_string_lossy().into_owned();
                let name = match &under {
                    Some(u) => format!("{}/{}", u.to_string_lossy(), name),
                    None => name,
                };
                Some(Source {
                    disk: p.clone(),
                    name,
                })
            })
            .collect();
        if sources.is_empty() {
            return;
        }
        if self.is_new() {
            let mut pending = self.ivars().pending.borrow_mut();
            for s in sources {
                pending.retain(|p| p.name != s.name);
                pending.push(s);
            }
            drop(pending);
            self.rebuild_tree();
            self.update_status();
        } else {
            self.run_modify(
                Edit {
                    add: sources,
                    ..Default::default()
                },
                "Adding files…",
            );
        }
    }

    fn begin_add_files(&self) {
        if !self.can_edit() || self.busy() {
            return;
        }
        let mtm = self.mtm();
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseFiles(true);
        panel.setCanChooseDirectories(true);
        panel.setAllowsMultipleSelection(true);
        panel.setPrompt(Some(&ns("Add")));
        panel.setMessage(Some(&ns("Choose files or folders to add to the archive")));
        // Add under the selected folder, if one is selected.
        let under = self
            .selected_nodes()
            .first()
            .map(|n| {
                let d = n.data();
                if d.is_dir {
                    Some(d.path.clone())
                } else {
                    d.path
                        .parent()
                        .filter(|p| !p.as_os_str().is_empty())
                        .map(Path::to_path_buf)
                }
            })
            .unwrap_or(None);
        let h = self.handle();
        let panel_c = panel.clone();
        let block = RcBlock::new(move |resp: NSModalResponse| {
            if resp != NSModalResponseOK {
                return;
            }
            let urls = panel_c.URLs();
            let paths: Vec<PathBuf> = (0..urls.count())
                .filter_map(|i| url_to_path(&urls.objectAtIndex(i)))
                .collect();
            let under = under.clone();
            h.on_main(move |doc, _| doc.add_sources(paths, under));
        });
        panel.beginSheetModalForWindow_completionHandler(self.window(), &block);
    }

    fn begin_delete(&self) {
        if !self.can_edit() || self.busy() {
            return;
        }
        let nodes = self.selected_nodes();
        if nodes.is_empty() {
            return;
        }
        let paths: Vec<PathBuf> = nodes.iter().map(|n| n.data().path.clone()).collect();
        let what = if paths.len() == 1 {
            format!("“{}”", nodes[0].data().name)
        } else {
            format!("{} items", paths.len())
        };
        let h = self.handle();
        let msg = if self.is_new() {
            "They are only removed from this new archive, not from disk.".to_string()
        } else {
            format!(
                "The archive “{}” will be rewritten without them.",
                self.archive_name()
            )
        };
        sheets::confirm(
            self.window(),
            &format!("Delete {what}?"),
            &msg,
            "Delete",
            self.mtm(),
            move |yes| {
                if !yes {
                    return;
                }
                let paths = paths.clone();
                h.on_main(move |doc, _| doc.apply_delete(paths));
            },
        );
    }

    fn apply_delete(&self, paths: Vec<PathBuf>) {
        if self.is_new() {
            {
                let mut pending = self.ivars().pending.borrow_mut();
                pending.retain(|s| {
                    let sp = Path::new(&s.name);
                    !paths.iter().any(|p| sp == p || sp.starts_with(p))
                });
            }
            self.rebuild_tree();
            self.update_status();
        } else {
            self.run_modify(
                Edit {
                    remove: paths,
                    ..Default::default()
                },
                "Deleting…",
            );
        }
    }

    fn begin_rename(&self) {
        if !self.can_edit() || self.busy() {
            return;
        }
        let Some(ov) = self.ivars().outline.get() else {
            return;
        };
        let row = ov.selectedRow();
        if row < 0 {
            return;
        }
        unsafe { ov.editColumn_row_withEvent_select(0, row, None, true) };
    }

    /// Called when the Name cell's text field ends editing.
    fn rename_from_field(&self, tf: &NSTextField) {
        let Some(ov) = self.ivars().outline.get() else {
            return;
        };
        let row = ov.rowForView(tf);
        if row < 0 {
            return;
        }
        let Some(item) = ov.itemAtRow(row) else {
            return;
        };
        let Some(node) = item.downcast_ref::<Node>() else {
            return;
        };
        let old_path = node.data().path.clone();
        let old_name = node.data().name.clone();
        let new_name = tf.stringValue().to_string();
        let new_name = new_name.trim().to_string();
        if new_name.is_empty() || new_name == old_name {
            tf.setStringValue(&ns(&old_name));
            return;
        }
        if new_name.contains('/') || new_name == "." || new_name == ".." {
            tf.setStringValue(&ns(&old_name));
            sheets::alert(
                Some(self.window()),
                "Invalid name",
                "Names cannot contain “/”.",
                false,
                self.mtm(),
            );
            return;
        }
        let new_path = old_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.join(&new_name))
            .unwrap_or_else(|| PathBuf::from(&new_name));
        if self.is_new() {
            {
                let mut pending = self.ivars().pending.borrow_mut();
                let old_s = old_path.to_string_lossy().into_owned();
                let new_s = new_path.to_string_lossy().into_owned();
                for s in pending.iter_mut() {
                    if s.name == old_s {
                        s.name = new_s.clone();
                    } else if let Some(rest) = s.name.strip_prefix(&format!("{old_s}/")) {
                        s.name = format!("{new_s}/{rest}");
                    }
                }
            }
            self.rebuild_tree();
            self.update_status();
        } else {
            self.run_modify(
                Edit {
                    rename: vec![(old_path, new_path)],
                    ..Default::default()
                },
                "Renaming…",
            );
        }
    }

    /// Rewrite the archive with `edit`, then reload it.
    fn run_modify(&self, edit: Edit, text: &str) {
        let Some(path) = self.path() else { return };
        let Some(archive) = self.ivars().archive.borrow().clone() else {
            return;
        };
        if archive.needs_password() && self.ivars().password.borrow().is_none() {
            let name = self.archive_name();
            let h = self.handle();
            let text = text.to_string();
            sheets::ask_password(self.window(), &name, false, self.mtm(), move |pw| {
                let (edit, text) = (edit.clone(), text.clone());
                h.on_main(move |doc, _| {
                    if let Some(pw) = pw {
                        *doc.ivars().password.borrow_mut() = Some(pw);
                        doc.run_modify(edit, &text);
                    }
                });
            });
            return;
        }
        let password = self.ivars().password.borrow().clone();
        let cancel = self.start_job(Job::Modify, text);
        let h = self.handle();
        spawn("zipper-modify", move || {
            let progress = throttled_progress(h.clone());
            let opts = WriteOptions {
                format: archive.format(),
                password,
                ..Default::default()
            };
            let result = write::modify(&path, &edit, &opts, &progress, &cancel);
            h.on_main(move |doc, mtm| doc.modify_finished(result, mtm));
        });
    }

    fn modify_finished(&self, result: archive_core::Result<PathBuf>, mtm: MainThreadMarker) {
        if self.ivars().closed.get() {
            return;
        }
        self.end_job();
        match result {
            Ok(_) => {
                // The archive on disk changed; drop any temp copies and reload.
                self.remove_temp_dir();
                let pw = self.ivars().password.borrow().clone();
                self.load(pw);
            }
            Err(Error::Cancelled) => self.update_status(),
            Err(e) => {
                self.update_status();
                sheets::alert(
                    Some(self.window()),
                    "Could not change the archive",
                    &e.to_string(),
                    true,
                    mtm,
                );
            }
        }
    }

    // ---------------------------------------------------------- save (new document)

    fn begin_save(&self) {
        if !self.is_new() || self.busy() {
            return;
        }
        let pending = self.ivars().pending.borrow().clone();
        if pending.is_empty() {
            return;
        }
        let mtm = self.mtm();
        let panel = NSSavePanel::savePanel(mtm);
        panel.setCanCreateDirectories(true);
        panel.setAllowsOtherFileTypes(true);
        panel.setExtensionHidden(false);
        panel.setMessage(Some(&ns("Save the new archive")));
        let stem = Path::new(&pending[0].name)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Archive".into());
        let stem = if pending.len() == 1 {
            stem
        } else {
            "Archive".to_string()
        };
        panel.setNameFieldStringValue(&ns(&format!("{stem}.zip")));
        if let Some(dir) = pending[0].disk.parent() {
            panel.setDirectoryURL(Some(&path_to_url(dir)));
        }
        let acc = self.build_save_accessory(mtm);
        panel.setAccessoryView(Some(&acc));

        let h = self.handle();
        let panel_c = panel.clone();
        let block = RcBlock::new(move |resp: NSModalResponse| {
            if resp != NSModalResponseOK {
                return;
            }
            let Some(url) = panel_c.URL() else { return };
            let Some(dest) = url_to_path(&url) else {
                return;
            };
            h.on_main(move |doc, _| doc.run_save(dest));
        });
        panel.beginSheetModalForWindow_completionHandler(self.window(), &block);
    }

    fn build_save_accessory(&self, mtm: MainThreadMarker) -> Retained<NSView> {
        let me: &AnyObject = self;
        let view = unsafe {
            NSView::initWithFrame(
                NSView::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(420.0, 124.0)),
            )
        };
        let label = |text: &str, y: f64| {
            let l = NSTextField::labelWithString(&ns(text), mtm);
            l.setFrame(NSRect::new(NSPoint::new(20.0, y), NSSize::new(110.0, 20.0)));
            l.setAlignment(objc2_app_kit::NSTextAlignment::Right);
            view.addSubview(&l);
        };
        let popup = |y: f64, items: &[&str]| {
            let p = unsafe {
                NSPopUpButton::initWithFrame_pullsDown(
                    NSPopUpButton::alloc(mtm),
                    NSRect::new(NSPoint::new(140.0, y - 3.0), NSSize::new(180.0, 26.0)),
                    false,
                )
            };
            let titles: Vec<Retained<NSString>> = items.iter().map(|s| ns(s)).collect();
            p.addItemsWithTitles(&NSArray::from_retained_slice(&titles));
            view.addSubview(&p);
            p
        };
        label("Format:", 94.0);
        let format = popup(94.0, &["zip", "7z"]);
        format.selectItemAtIndex(crate::prefs::default_format_index());
        unsafe {
            format.setTarget(Some(me));
            format.setAction(Some(sel!(saveFormatChanged:)));
        }
        label("Compression:", 66.0);
        let level = popup(66.0, &["Store (no compression)", "Fast", "Normal", "Best"]);
        level.selectItemAtIndex(2);
        label("Password:", 38.0);
        let password = unsafe {
            NSSecureTextField::initWithFrame(
                NSSecureTextField::alloc(mtm),
                NSRect::new(NSPoint::new(140.0, 36.0), NSSize::new(180.0, 24.0)),
            )
        };
        password.setPlaceholderString(Some(&ns("none")));
        view.addSubview(&password);
        label("Split (7z):", 10.0);
        let split = popup(
            10.0,
            &[
                "Don’t split",
                "100 MB volumes",
                "1 GB volumes",
                "4 GB volumes",
            ],
        );
        *self.ivars().save_ui.borrow_mut() = Some(SaveUi {
            format,
            level,
            password,
            split,
        });
        self.sync_save_ui();
        view
    }

    fn sync_save_ui(&self) {
        let ui = self.ivars().save_ui.borrow();
        if let Some(ui) = ui.as_ref() {
            let is_7z = ui.format.indexOfSelectedItem() == 1;
            ui.split.setEnabled(is_7z);
        }
    }

    fn save_options(&self) -> WriteOptions {
        let ui = self.ivars().save_ui.borrow();
        let Some(ui) = ui.as_ref() else {
            return WriteOptions::default();
        };
        let format = if ui.format.indexOfSelectedItem() == 1 {
            Format::SevenZ
        } else {
            Format::Zip
        };
        let level = match ui.level.indexOfSelectedItem() {
            0 => 0,
            1 => 1,
            3 => 9,
            _ => 6,
        };
        let pw = ui.password.stringValue().to_string();
        let volume_size = if format == Format::SevenZ {
            match ui.split.indexOfSelectedItem() {
                1 => Some(100 * 1_000_000),
                2 => Some(1_000_000_000),
                3 => Some(4_000_000_000),
                _ => None,
            }
        } else {
            None
        };
        WriteOptions {
            format,
            level,
            password: if pw.is_empty() { None } else { Some(pw) },
            solid: false,
            encrypt_header: true,
            volume_size,
        }
    }

    fn run_save(&self, mut dest: PathBuf) {
        let opts = self.save_options();
        // Make the extension match the chosen format.
        let ext = opts.format.extension();
        let has_ext = dest
            .extension()
            .map(|e| e.to_string_lossy().eq_ignore_ascii_case(ext))
            .unwrap_or(false);
        if !has_ext {
            let name = dest
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Archive".into());
            let stem = match name.rsplit_once('.') {
                Some((s, e)) if e.eq_ignore_ascii_case("zip") || e.eq_ignore_ascii_case("7z") => {
                    s.to_string()
                }
                _ => name,
            };
            dest.set_file_name(format!("{stem}.{ext}"));
        }
        let sources = self.ivars().pending.borrow().clone();
        let cancel = self.start_job(Job::Save, "Saving…");
        let h = self.handle();
        spawn("zipper-save", move || {
            let progress = throttled_progress(h.clone());
            let result = write::create_from(&dest, &sources, &opts, &progress, &cancel);
            let password = opts.password.clone();
            h.on_main(move |doc, mtm| doc.save_finished(result, password, mtm));
        });
    }

    fn save_finished(
        &self,
        result: archive_core::Result<PathBuf>,
        password: Option<String>,
        mtm: MainThreadMarker,
    ) {
        if self.ivars().closed.get() {
            return;
        }
        self.end_job();
        match result {
            Ok(saved) => {
                // The document now is that archive.
                self.ivars().pending.borrow_mut().clear();
                *self.ivars().path.borrow_mut() = Some(saved.clone());
                *self.ivars().password.borrow_mut() = password.clone();
                NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(
                    &NSArray::from_retained_slice(&[path_to_url(&saved)]),
                );
                self.load(password);
            }
            Err(Error::Cancelled) => self.update_status(),
            Err(e) => {
                self.update_status();
                sheets::alert(
                    Some(self.window()),
                    "Could not save the archive",
                    &e.to_string(),
                    true,
                    mtm,
                );
            }
        }
    }

    // ---------------------------------------------------------- drag out

    fn pasteboard_writer_for(
        &self,
        item: &AnyObject,
    ) -> Option<Retained<ProtocolObject<dyn NSPasteboardWriting>>> {
        let node = item.downcast_ref::<Node>()?;
        let d = node.data();
        // Not in an archive yet: Finder can copy the real file.
        if let Some(disk) = &d.disk {
            let url = path_to_url(disk);
            return Some(ProtocolObject::from_retained(url));
        }
        self.ivars().archive.borrow().as_ref()?;
        let file_type = if d.is_dir {
            "public.folder"
        } else {
            "public.data"
        };
        let provider = NSFilePromiseProvider::initWithFileType_delegate(
            NSFilePromiseProvider::alloc(),
            &ns(file_type),
            ProtocolObject::from_ref(self),
        );
        let info: Retained<NSString> = ns(&d.path.to_string_lossy());
        unsafe { provider.setUserInfo(Some(&info)) };
        Some(ProtocolObject::from_retained(provider))
    }

    fn write_promise(
        &self,
        provider: &NSFilePromiseProvider,
        url: &NSURL,
        handler: &block2::DynBlock<dyn Fn(*mut NSError)>,
    ) {
        let entry_path = promise_path(provider);
        let Some(target) = url_to_path(url) else {
            handler.call((std::ptr::null_mut(),));
            return;
        };
        let Some(archive) = self.ivars().archive.borrow().clone() else {
            handler.call((std::ptr::null_mut(),));
            return;
        };
        let id = self.ivars().next_promise.get();
        self.ivars().next_promise.set(id + 1);
        self.ivars()
            .promises
            .borrow_mut()
            .insert(id, handler.copy());

        let password = self.ivars().password.borrow().clone();
        // Extract <entry> so that it lands exactly at <target>: strip the
        // entry's parent and write into target's parent folder.
        let dest = target
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/"));
        let strip = entry_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf);
        let cancel = Arc::new(Cancel::new());
        if self.ivars().job.get().is_none() {
            self.start_job_with(Job::Promise, "Copying to Finder…", cancel.clone());
        }
        let h = self.handle();
        spawn("zipper-promise", move || {
            let progress = throttled_progress(h.clone());
            let opts = ExtractOptions {
                overwrite: Overwrite::Always(ConflictAction::Replace),
                password,
                into_subfolder: false,
                preserve_mtime: true,
                strip_prefix: strip,
            };
            let result = archive.extract(&[entry_path], &dest, &opts, &progress, &cancel);
            h.on_main(move |doc, mtm| doc.promise_finished(id, result, mtm));
        });
    }

    fn promise_finished(
        &self,
        id: u64,
        result: archive_core::Result<ExtractReport>,
        mtm: MainThreadMarker,
    ) {
        let block = self.ivars().promises.borrow_mut().remove(&id);
        if self.ivars().job.get() == Some(Job::Promise) && self.ivars().promises.borrow().is_empty()
        {
            self.end_job();
            self.update_status();
        }
        if let Some(b) = block {
            b.call((std::ptr::null_mut(),));
        }
        if let Err(e) = result {
            if !matches!(e, Error::Cancelled) && !self.ivars().closed.get() {
                sheets::alert(
                    Some(self.window()),
                    "Could not copy file",
                    &e.to_string(),
                    true,
                    mtm,
                );
            }
        }
    }

    // ---------------------------------------------------------- quick look

    fn quick_look_panel(&self) -> Option<Retained<QLPreviewPanel>> {
        QLPreviewPanel::sharedPreviewPanel(self.mtm())
    }

    fn toggle_quick_look(&self) {
        let mtm = self.mtm();
        if QLPreviewPanel::sharedPreviewPanelExists(mtm) {
            if let Some(p) = self.quick_look_panel() {
                if p.isVisible() {
                    p.orderOut(None);
                    return;
                }
            }
        }
        if self.busy() {
            return;
        }
        let nodes: Vec<Retained<Node>> = self
            .selected_nodes()
            .into_iter()
            .filter(|n| !n.data().is_dir)
            .collect();
        if nodes.is_empty() {
            return;
        }
        // Files on disk already (unsaved document) need no extraction.
        let mut ready: Vec<PathBuf> = Vec::new();
        let mut to_extract: Vec<PathBuf> = Vec::new();
        let temp = self.temp_dir();
        for n in &nodes {
            let d = n.data();
            if let Some(disk) = &d.disk {
                ready.push(disk.clone());
            } else if temp.join(&d.path).is_file() {
                ready.push(temp.join(&d.path));
            } else {
                to_extract.push(d.path.clone());
            }
        }
        if to_extract.is_empty() {
            self.show_quick_look(ready);
            return;
        }
        let Some(archive) = self.ivars().archive.borrow().clone() else {
            self.show_quick_look(ready);
            return;
        };
        if archive.needs_password() && self.ivars().password.borrow().is_none() {
            let name = self.archive_name();
            let h = self.handle();
            sheets::ask_password(self.window(), &name, false, mtm, move |pw| {
                h.on_main(move |doc, _| {
                    if let Some(pw) = pw {
                        *doc.ivars().password.borrow_mut() = Some(pw);
                        doc.toggle_quick_look();
                    }
                });
            });
            return;
        }
        let password = self.ivars().password.borrow().clone();
        let cancel = self.start_job(Job::QuickLook, "Preparing preview…");
        let h = self.handle();
        let all: Vec<PathBuf> = nodes.iter().map(|n| n.data().path.clone()).collect();
        spawn("zipper-quicklook", move || {
            let progress = throttled_progress(h.clone());
            let opts = ExtractOptions {
                overwrite: Overwrite::Always(ConflictAction::Replace),
                password,
                into_subfolder: false,
                preserve_mtime: true,
                strip_prefix: None,
            };
            let result = archive.extract(&to_extract, &temp, &opts, &progress, &cancel);
            h.on_main(move |doc, mtm| {
                if doc.ivars().closed.get() {
                    return;
                }
                doc.end_job();
                doc.update_status();
                match result {
                    Ok(_) => {
                        let temp = doc.temp_dir();
                        let paths: Vec<PathBuf> = nodes_paths_on_disk(&all, &ready, &temp);
                        doc.show_quick_look(paths);
                    }
                    Err(Error::Cancelled) => {}
                    Err(Error::WrongPassword) | Err(Error::PasswordRequired) => {
                        *doc.ivars().password.borrow_mut() = None;
                        sheets::alert(
                            Some(doc.window()),
                            "Wrong password",
                            "The password was not accepted.",
                            true,
                            mtm,
                        );
                    }
                    Err(e) => sheets::alert(
                        Some(doc.window()),
                        "Could not preview",
                        &e.to_string(),
                        true,
                        mtm,
                    ),
                }
            });
        });
    }

    fn show_quick_look(&self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        *self.ivars().quick_look.borrow_mut() = paths;
        let Some(panel) = self.quick_look_panel() else {
            return;
        };
        unsafe { panel.setDataSource(Some(ProtocolObject::from_ref(self))) };
        panel.reloadData();
        panel.makeKeyAndOrderFront(None);
    }

    // ---------------------------------------------------------- jobs & progress sheet

    fn start_job(&self, job: Job, text: &str) -> Arc<Cancel> {
        let cancel = Arc::new(Cancel::new());
        self.start_job_with(job, text, cancel.clone());
        cancel
    }

    fn start_job_with(&self, job: Job, text: &str, cancel: Arc<Cancel>) {
        *self.ivars().cancel.borrow_mut() = Some(cancel);
        self.ivars().job.set(Some(job));
        self.show_progress(text, None);
    }

    fn end_job(&self) {
        self.hide_progress();
        self.ivars().job.set(None);
        *self.ivars().cancel.borrow_mut() = None;
    }

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
        // Answer any dangling drag promises so Finder does not wait forever.
        let promises: Vec<PromiseBlock> = self
            .ivars()
            .promises
            .borrow_mut()
            .drain()
            .map(|(_, b)| b)
            .collect();
        for b in promises {
            b.call((std::ptr::null_mut(),));
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

/// Progress callback for worker threads: forwards at most ~12 updates per
/// second to the document on the main thread.
fn throttled_progress(h: MainHandle<Document>) -> impl Fn(&Progress) + Sync {
    let last = std::sync::Mutex::new(Instant::now() - Duration::from_secs(1));
    move |p: &Progress| {
        let mut l = last.lock().unwrap();
        if l.elapsed() < Duration::from_millis(80) {
            return;
        }
        *l = Instant::now();
        let p = p.clone();
        h.on_main(move |doc, _| doc.update_progress(&p));
    }
}

fn promise_path(provider: &NSFilePromiseProvider) -> PathBuf {
    unsafe { provider.userInfo() }
        .and_then(|o| o.downcast::<NSString>().ok())
        .map(|s| PathBuf::from(s.to_string()))
        .unwrap_or_default()
}

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
    target: &AnyObject,
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
    } else if with_icon {
        tf.setFrame(NSRect::new(
            NSPoint::new(22.0, 1.0),
            NSSize::new(176.0, 18.0),
        ));
        // Inline rename: committing the edit sends renameCommitted:.
        unsafe {
            tf.setTarget(Some(target));
            tf.setAction(Some(sel!(renameCommitted:)));
        }
    } else {
        tf.setFrame(NSRect::new(
            NSPoint::new(2.0, 1.0),
            NSSize::new(196.0, 18.0),
        ));
    }
    tf.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
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
/// deprecated `-[NSWorkspace iconForFileType:]` (Icon Services).
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

#[allow(dead_code)]
fn _kinds(_: EntryKind) {}

/// Quick Look item order follows the selection: disk files as they are,
/// archive entries at their temp location.
fn nodes_paths_on_disk(all: &[PathBuf], ready: &[PathBuf], temp: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in all {
        let t = temp.join(p);
        if t.is_file() {
            out.push(t);
        } else if let Some(r) = ready.iter().find(|r| r.file_name() == p.file_name()) {
            out.push(r.clone());
        }
    }
    out
}
