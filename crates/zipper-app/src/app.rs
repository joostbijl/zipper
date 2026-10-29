//! Application delegate: launch, file opening (Finder, Outlook, Dock,
//! File → Open), the main menu, the document registry and the empty
//! "drop an archive here" window.

use std::cell::RefCell;
use std::path::PathBuf;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSAutoresizingMaskOptions,
    NSBackingStoreType, NSDragOperation, NSDraggingDestination, NSDraggingInfo,
    NSEventModifierFlags, NSFilenamesPboardType, NSMenu, NSMenuItem, NSModalResponse,
    NSModalResponseOK, NSOpenPanel, NSTextAlignment, NSTextField, NSView, NSWindow,
    NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
    ns_string, NSArray, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
    NSString, NSURL,
};

use crate::document::Document;
use crate::sheets;
use crate::util::{ns, url_to_path};

thread_local! {
    static DOCUMENTS: RefCell<Vec<Retained<Document>>> = const { RefCell::new(Vec::new()) };
    static EMPTY_WINDOW: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
}

/// Open an archive in a new window (or focus the window that already shows it).
pub fn open_path(path: PathBuf, mtm: MainThreadMarker) {
    let existing = DOCUMENTS.with(|d| {
        d.borrow()
            .iter()
            .find(|doc| doc.path().as_ref() == Some(&path))
            .cloned()
    });
    if let Some(doc) = existing {
        doc.window().makeKeyAndOrderFront(None);
        return;
    }
    let doc = Document::open(path, mtm);
    DOCUMENTS.with(|d| d.borrow_mut().push(doc));
    close_empty_window();
}

pub fn forget_document(doc: &Document) {
    DOCUMENTS.with(|d| d.borrow_mut().retain(|x| !std::ptr::eq(&**x, doc)));
}

fn close_empty_window() {
    EMPTY_WINDOW.with(|w| {
        if let Some(w) = w.borrow_mut().take() {
            w.close();
        }
    });
}

pub fn show_open_panel(mtm: MainThreadMarker) {
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(true);
    panel.setCanChooseDirectories(false);
    panel.setAllowsMultipleSelection(true);
    panel.setMessage(Some(&ns("Choose an archive to open")));
    let panel_c = panel.clone();
    let block = RcBlock::new(move |resp: NSModalResponse| {
        if resp != NSModalResponseOK {
            return;
        }
        let urls = panel_c.URLs();
        let paths: Vec<PathBuf> = (0..urls.count())
            .filter_map(|i| url_to_path(&urls.objectAtIndex(i)))
            .collect();
        // Open on the next run-loop turn, after the panel's view service has
        // been torn down, rather than inside its completion handler.
        dispatch2::DispatchQueue::main().exec_async(move || {
            // SAFETY: the main queue runs on the main thread.
            let mtm = unsafe { MainThreadMarker::new_unchecked() };
            for p in paths {
                open_path(p, mtm);
            }
        });
    });
    unsafe { panel.beginWithCompletionHandler(&block) };
}

// ------------------------------------------------------------------ delegate

#[derive(Default)]
pub struct AppIvars {}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ZipperAppDelegate"]
    #[ivars = AppIvars]
    pub struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationWillFinishLaunching:))]
        fn will_finish_launching(&self, _n: &NSNotification) {
            let mtm = self.mtm();
            let app = NSApplication::sharedApplication(mtm);
            app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
            app.setMainMenu(Some(&build_menu(mtm)));
        }

        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _n: &NSNotification) {
            let app = NSApplication::sharedApplication(self.mtm());
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
        }

        #[unsafe(method(application:openURLs:))]
        fn open_urls(&self, _app: &NSApplication, urls: &NSArray<NSURL>) {
            let mtm = self.mtm();
            for i in 0..urls.count() {
                if let Some(p) = url_to_path(&urls.objectAtIndex(i)) {
                    open_path(p, mtm);
                }
            }
        }

        #[unsafe(method(applicationShouldOpenUntitledFile:))]
        fn should_open_untitled(&self, _app: &NSApplication) -> bool {
            true
        }

        #[unsafe(method(applicationOpenUntitledFile:))]
        fn open_untitled(&self, _app: &NSApplication) -> bool {
            show_empty_window(self.mtm());
            true
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn terminate_after_last_window(&self, _app: &NSApplication) -> bool {
            false
        }

        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _n: &NSNotification) {
            DOCUMENTS.with(|d| {
                for doc in d.borrow().iter() {
                    doc.cancel_current();
                    doc.remove_temp_dir();
                }
            });
            // Anything left from crashed sessions.
            let _ = std::fs::remove_dir_all(std::env::temp_dir().join("Zipper"));
        }

        #[unsafe(method(applicationSupportsSecureRestorableState:))]
        fn supports_secure_restorable_state(&self, _app: &NSApplication) -> bool {
            true
        }
    }

    // Menu actions that are not document-specific.
    impl AppDelegate {
        #[unsafe(method(openDocument:))]
        fn open_document(&self, _sender: Option<&AnyObject>) {
            show_open_panel(self.mtm());
        }

        #[unsafe(method(newEmptyWindow:))]
        fn new_empty_window(&self, _sender: Option<&AnyObject>) {
            show_empty_window(self.mtm());
        }
    }
);

impl AppDelegate {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(AppIvars::default());
        unsafe { msg_send![super(this), init] }
    }
}

// ------------------------------------------------------------------ menu

fn item(
    title: &str,
    action: Option<objc2::runtime::Sel>,
    key: &str,
    mtm: MainThreadMarker,
) -> Retained<NSMenuItem> {
    unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &ns(title),
            action,
            &ns(key),
        )
    }
}

fn build_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let main = NSMenu::new(mtm);

    // Application menu
    let app_item = item("Zipper", None, "", mtm);
    let app_menu = NSMenu::new(mtm);
    app_menu.addItem(&item(
        "About Zipper",
        Some(sel!(orderFrontStandardAboutPanel:)),
        "",
        mtm,
    ));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&item("Hide Zipper", Some(sel!(hide:)), "h", mtm));
    let hide_others = item("Hide Others", Some(sel!(hideOtherApplications:)), "h", mtm);
    hide_others
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Option);
    app_menu.addItem(&hide_others);
    app_menu.addItem(&item(
        "Show All",
        Some(sel!(unhideAllApplications:)),
        "",
        mtm,
    ));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&item("Quit Zipper", Some(sel!(terminate:)), "q", mtm));
    app_item.setSubmenu(Some(&app_menu));
    main.addItem(&app_item);

    // File
    let file_item = item("File", None, "", mtm);
    let file_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("File"));
    file_menu.addItem(&item("Open…", Some(sel!(openDocument:)), "o", mtm));
    file_menu.addItem(&item("New Window", Some(sel!(newEmptyWindow:)), "n", mtm));
    file_menu.addItem(&NSMenuItem::separatorItem(mtm));
    file_menu.addItem(&item("Close", Some(sel!(performClose:)), "w", mtm));
    file_item.setSubmenu(Some(&file_menu));
    main.addItem(&file_item);

    // Edit
    let edit_item = item("Edit", None, "", mtm);
    let edit_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Edit"));
    edit_menu.addItem(&item("Cut", Some(sel!(cut:)), "x", mtm));
    edit_menu.addItem(&item("Copy", Some(sel!(copy:)), "c", mtm));
    edit_menu.addItem(&item("Paste", Some(sel!(paste:)), "v", mtm));
    edit_menu.addItem(&item("Select All", Some(sel!(selectAll:)), "a", mtm));
    edit_item.setSubmenu(Some(&edit_menu));
    main.addItem(&edit_item);

    // Archive
    let arch_item = item("Archive", None, "", mtm);
    let arch_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Archive"));
    arch_menu.addItem(&item("Extract…", Some(sel!(extractSelected:)), "e", mtm));
    let extract_all = item("Extract All…", Some(sel!(extractAll:)), "e", mtm);
    extract_all
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Shift);
    arch_menu.addItem(&extract_all);
    arch_menu.addItem(&item(
        "Open Selected File",
        Some(sel!(openSelected:)),
        "\r",
        mtm,
    ));
    arch_menu.addItem(&NSMenuItem::separatorItem(mtm));
    arch_menu.addItem(&item("Test Archive", Some(sel!(testArchive:)), "t", mtm));
    arch_item.setSubmenu(Some(&arch_menu));
    main.addItem(&arch_item);

    // Window
    let win_item = item("Window", None, "", mtm);
    let win_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Window"));
    win_menu.addItem(&item("Minimize", Some(sel!(performMiniaturize:)), "m", mtm));
    win_menu.addItem(&item("Zoom", Some(sel!(performZoom:)), "", mtm));
    win_menu.addItem(&NSMenuItem::separatorItem(mtm));
    win_menu.addItem(&item(
        "Bring All to Front",
        Some(sel!(arrangeInFront:)),
        "",
        mtm,
    ));
    win_item.setSubmenu(Some(&win_menu));
    main.addItem(&win_item);
    let app = NSApplication::sharedApplication(mtm);
    app.setWindowsMenu(Some(&win_menu));

    main
}

// ------------------------------------------------------------------ empty window

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "ZipperDropView"]
    pub struct DropView;

    unsafe impl NSObjectProtocol for DropView {}

    unsafe impl NSDraggingDestination for DropView {
        #[unsafe(method(draggingEntered:))]
        fn dragging_entered(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            if dragged_paths(sender).is_empty() {
                NSDragOperation::None
            } else {
                NSDragOperation::Copy
            }
        }

        #[unsafe(method(performDragOperation:))]
        #[allow(clippy::useless_conversion)]
        fn perform_drag(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            let paths = dragged_paths(sender);
            if paths.is_empty() {
                return false.into();
            }
            let mtm = self.mtm();
            for p in paths {
                open_path(p, mtm);
            }
            true.into()
        }
    }
);

impl DropView {
    fn new(frame: NSRect, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        let this: Retained<Self> = unsafe { msg_send![this, initWithFrame: frame] };
        let types = NSArray::from_slice(&[unsafe { NSFilenamesPboardType }]);
        this.registerForDraggedTypes(&types);
        this
    }
}

fn dragged_paths(info: &ProtocolObject<dyn NSDraggingInfo>) -> Vec<PathBuf> {
    let pb = unsafe { info.draggingPasteboard() };
    let Some(list) = pb.propertyListForType(unsafe { NSFilenamesPboardType }) else {
        return vec![];
    };
    let Some(arr) = list.downcast_ref::<NSArray>() else {
        return vec![];
    };
    let mut out = Vec::new();
    for i in 0..arr.count() {
        let obj = arr.objectAtIndex(i);
        if let Some(s) = obj.downcast_ref::<NSString>() {
            let p = PathBuf::from(s.to_string());
            if p.is_file() {
                out.push(p);
            }
        }
    }
    out
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ZipperEmptyWindowDelegate"]
    pub struct EmptyWindowDelegate;

    unsafe impl NSObjectProtocol for EmptyWindowDelegate {}

    unsafe impl NSWindowDelegate for EmptyWindowDelegate {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _n: &NSNotification) {
            EMPTY_WINDOW.with(|w| *w.borrow_mut() = None);
        }
    }
);

thread_local! {
    static EMPTY_DELEGATE: RefCell<Option<Retained<EmptyWindowDelegate>>> = const { RefCell::new(None) };
}

pub fn show_empty_window(mtm: MainThreadMarker) {
    let existing = EMPTY_WINDOW.with(|w| w.borrow().clone());
    if let Some(w) = existing {
        w.makeKeyAndOrderFront(None);
        return;
    }
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(480.0, 300.0));
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::Miniaturizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(ns_string!("Zipper"));
    let delegate = EmptyWindowDelegate::alloc(mtm);
    let delegate: Retained<EmptyWindowDelegate> = unsafe { msg_send![delegate, init] };
    window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    EMPTY_DELEGATE.with(|d| *d.borrow_mut() = Some(delegate));

    let root = window.contentView().expect("content view");
    let drop = DropView::new(root.bounds(), mtm);
    drop.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    let label = NSTextField::wrappingLabelWithString(
        &ns("Drop an archive here to open it,\nor choose File → Open… (⌘O)."),
        mtm,
    );
    label.setAlignment(NSTextAlignment::Center);
    label.setFrame(NSRect::new(
        NSPoint::new(40.0, 120.0),
        NSSize::new(400.0, 60.0),
    ));
    label.setTextColor(Some(&objc2_app_kit::NSColor::secondaryLabelColor()));
    label.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable
            | NSAutoresizingMaskOptions::ViewMinYMargin
            | NSAutoresizingMaskOptions::ViewMaxYMargin,
    );
    drop.addSubview(&label);
    root.addSubview(&drop);

    window.center();
    window.makeKeyAndOrderFront(None);
    EMPTY_WINDOW.with(|w| *w.borrow_mut() = Some(window));
    let _ = sheets::label; // keep module referenced
}
