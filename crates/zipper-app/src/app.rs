//! Application delegate: launch, file opening (Finder, Outlook, Dock,
//! File → Open), the main menu, the document registry and the empty
//! "drop an archive here" window.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationOptions, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSAutoresizingMaskOptions, NSBackingStoreType, NSDragOperation,
    NSDraggingDestination, NSDraggingInfo, NSEventModifierFlags, NSFilenamesPboardType, NSMenu,
    NSMenuItem, NSModalResponse, NSModalResponseOK, NSOpenPanel, NSPasteboard,
    NSRunningApplication, NSTextAlignment, NSTextField, NSView, NSWindow, NSWindowDelegate,
    NSWindowStyleMask,
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
    activate(mtm);
}

/// Bring the app to the front, the way macOS 14+ wants it done.
pub fn activate(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    app.activate();
    let _ = NSRunningApplication::currentApplication()
        .activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
}

/// File paths on a pasteboard (drag or service).
pub fn pasteboard_paths(pb: &NSPasteboard) -> Vec<PathBuf> {
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
            if p.exists() {
                out.push(p);
            }
        }
    }
    out
}

/// Finder-style: archives open, anything else becomes one new archive
/// (SPEC §4, item 11).
pub fn open_paths(paths: Vec<PathBuf>, mtm: MainThreadMarker) {
    let mut sources = Vec::new();
    for p in paths {
        if p.is_file() && looks_like_archive(&p) {
            open_path(p, mtm);
        } else if p.exists() {
            sources.push(p);
        }
    }
    if !sources.is_empty() {
        new_document(sources, mtm);
    }
}

/// Make one new archive from exactly these paths, whatever they are, and go
/// straight to the Save panel. Used by the Finder service / Quick Action and
/// the `--add` command-line flag.
pub fn add_to_new_archive(paths: Vec<PathBuf>, mtm: MainThreadMarker) {
    let paths: Vec<PathBuf> = paths.into_iter().filter(|p| p.exists()).collect();
    if paths.is_empty() {
        return;
    }
    activate(mtm);
    let doc = Document::new_untitled(paths, mtm);
    DOCUMENTS.with(|d| d.borrow_mut().push(doc.clone()));
    close_empty_window();
    doc.save_now();
}

/// Paths from a `zipper://add?p=…&p=…` URL (each `p` percent-encoded).
fn zipper_url_paths(url: &NSURL) -> Vec<PathBuf> {
    let Some(query) = url.query().map(|q| q.to_string()) else {
        return vec![];
    };
    query
        .split('&')
        .filter_map(|kv| kv.strip_prefix("p="))
        .map(percent_decode)
        .map(PathBuf::from)
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// For a drop or a Finder "open": is this a file the user means as an
/// archive? Zip-based document formats (Office, OpenDocument, jar, …) are
/// zips technically, but dropping one on Zipper almost always means "put it
/// in an archive", not "show me its XML".
pub fn looks_like_archive(p: &Path) -> bool {
    const DOCUMENT_EXTS: &[&str] = &[
        "docx", "xlsx", "pptx", "docm", "xlsm", "pptm", "dotx", "xltx", "potx", "odt", "ods",
        "odp", "odg", "jar", "war", "ear", "apk", "ipa", "epub", "pages", "numbers", "key",
        "sketch", "xmind", "kmz", "vsdx", "whl", "crx", "xpi", "oxt",
    ];
    let ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if DOCUMENT_EXTS.contains(&ext.as_str()) {
        return false;
    }
    archive_core::detect_format(p).is_ok()
}

/// A new, unsaved archive window.
pub fn new_document(sources: Vec<PathBuf>, mtm: MainThreadMarker) {
    let doc = Document::new_untitled(sources, mtm);
    DOCUMENTS.with(|d| d.borrow_mut().push(doc));
    close_empty_window();
    activate(mtm);
}

pub fn forget_document(doc: &Document) {
    DOCUMENTS.with(|d| d.borrow_mut().retain(|x| !std::ptr::eq(&**x, doc)));
}

fn close_empty_window() {
    // Take the window out first and drop the borrow: `close()` calls the
    // delegate's windowWillClose: synchronously, which touches the same cell.
    let window = EMPTY_WINDOW.with(|w| w.borrow_mut().take());
    if let Some(w) = window {
        w.close();
    }
}

pub fn show_open_panel(mtm: MainThreadMarker) {
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(true);
    panel.setCanChooseDirectories(false);
    panel.setAllowsMultipleSelection(true);
    panel.setMessage(Some(&ns(
        "Choose an archive to open (other files start a new archive)",
    )));
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
            open_paths(paths, mtm);
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
            // Finder "Services" entries (see NSServices in Info.plist).
            let me: &AnyObject = self;
            unsafe { app.setServicesProvider(Some(me)) };
        }

        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _n: &NSNotification) {
            let mtm = self.mtm();
            activate(mtm);
        }

        #[unsafe(method(application:openURLs:))]
        fn open_urls(&self, _app: &NSApplication, urls: &NSArray<NSURL>) {
            let mtm = self.mtm();
            let mut paths: Vec<PathBuf> = Vec::new();
            let mut add: Vec<PathBuf> = Vec::new();
            for i in 0..urls.count() {
                let url = urls.objectAtIndex(i);
                let scheme = url.scheme().map(|s| s.to_string()).unwrap_or_default();
                if scheme.eq_ignore_ascii_case("zipper") {
                    // zipper://add?p=<percent-encoded path>&p=…
                    add.extend(zipper_url_paths(&url));
                } else if let Some(p) = url_to_path(&url) {
                    paths.push(p);
                }
            }
            if !paths.is_empty() {
                open_paths(paths, mtm);
            }
            if !add.is_empty() {
                add_to_new_archive(add, mtm);
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

        #[unsafe(method(newDocument:))]
        fn new_document_action(&self, _sender: Option<&AnyObject>) {
            new_document(Vec::new(), self.mtm());
        }

        #[unsafe(method(showPreferences:))]
        fn show_preferences(&self, _sender: Option<&AnyObject>) {
            crate::prefs::show(self.mtm());
        }

        // Finder service: selected files → new archive, straight to Save.
        #[unsafe(method(addToArchiveService:userData:error:))]
        fn add_to_archive_service(&self, pboard: &NSPasteboard, _user_data: Option<&NSString>, _error: *mut *mut NSString) {
            add_to_new_archive(pasteboard_paths(pboard), self.mtm());
        }

        // Finder service: selected archives → open and extract everything.
        #[unsafe(method(extractService:userData:error:))]
        fn extract_service(&self, pboard: &NSPasteboard, _user_data: Option<&NSString>, _error: *mut *mut NSString) {
            let mtm = self.mtm();
            activate(mtm);
            for p in pasteboard_paths(pboard) {
                if p.is_file() && archive_core::detect_format(&p).is_ok() {
                    let doc = Document::open_then_extract(p, mtm);
                    DOCUMENTS.with(|d| d.borrow_mut().push(doc));
                }
            }
            close_empty_window();
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
    app_menu.addItem(&item("Settings…", Some(sel!(showPreferences:)), ",", mtm));
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
    file_menu.addItem(&item("New Archive", Some(sel!(newDocument:)), "n", mtm));
    file_menu.addItem(&item("Open…", Some(sel!(openDocument:)), "o", mtm));
    file_menu.addItem(&NSMenuItem::separatorItem(mtm));
    file_menu.addItem(&item("Close", Some(sel!(performClose:)), "w", mtm));
    file_menu.addItem(&item("Save", Some(sel!(saveDocument:)), "s", mtm));
    file_item.setSubmenu(Some(&file_menu));
    main.addItem(&file_item);

    // Edit
    let edit_item = item("Edit", None, "", mtm);
    let edit_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Edit"));
    edit_menu.addItem(&item("Cut", Some(sel!(cut:)), "x", mtm));
    edit_menu.addItem(&item("Copy", Some(sel!(copy:)), "c", mtm));
    edit_menu.addItem(&item("Paste", Some(sel!(paste:)), "v", mtm));
    edit_menu.addItem(&item("Select All", Some(sel!(selectAll:)), "a", mtm));
    edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let rename = item("Rename", Some(sel!(renameSelected:)), "r", mtm);
    rename
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Shift);
    edit_menu.addItem(&rename);
    let delete = item("Delete", Some(sel!(deleteSelected:)), "\u{8}", mtm);
    delete.setKeyEquivalentModifierMask(NSEventModifierFlags::Command);
    edit_menu.addItem(&delete);
    edit_item.setSubmenu(Some(&edit_menu));
    main.addItem(&edit_item);

    // Archive
    let arch_item = item("Archive", None, "", mtm);
    let arch_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Archive"));
    let add = item("Add Files…", Some(sel!(addFiles:)), "a", mtm);
    add.setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Shift);
    arch_menu.addItem(&add);
    arch_menu.addItem(&NSMenuItem::separatorItem(mtm));
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
    let ql = item("Quick Look", Some(sel!(quickLookSelected:)), " ", mtm);
    ql.setKeyEquivalentModifierMask(NSEventModifierFlags::empty());
    arch_menu.addItem(&ql);
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
            open_paths(paths, mtm);
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

pub fn dragged_paths(info: &ProtocolObject<dyn NSDraggingInfo>) -> Vec<PathBuf> {
    let pb = unsafe { info.draggingPasteboard() };
    pasteboard_paths(&pb)
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
            // May run re-entrantly from close_empty_window(); never panic here.
            EMPTY_WINDOW.with(|w| {
                if let Ok(mut slot) = w.try_borrow_mut() {
                    *slot = None;
                }
            });
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
        &ns("Drop an archive here to open it, or files to create one.\nFile → Open… (⌘O) · File → New Archive (⌘N)"),
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

// These run on the Mac (`cargo test -p zipper-app`); on Linux the UI crate
// cannot link, so they are checked but not run.
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn decodes_percent_and_plus() {
        assert_eq!(
            percent_decode("/Users/j/My%20File%2B1.png"),
            "/Users/j/My File+1.png"
        );
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("%C3%A9"), "é");
    }

    #[test]
    fn office_files_are_not_archives() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.docx");
        std::fs::write(&p, b"PK\x03\x04zzzz").unwrap();
        assert!(!looks_like_archive(&p));
        let z = d.path().join("x.zip");
        std::fs::write(&z, b"PK\x03\x04zzzz").unwrap();
        assert!(looks_like_archive(&z));
    }
}
