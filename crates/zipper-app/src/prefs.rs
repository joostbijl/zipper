//! Preferences (⌘,): stored in NSUserDefaults, read through [`Prefs`].

use std::cell::RefCell;
use std::path::PathBuf;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSButton, NSControlStateValueOff, NSControlStateValueOn, NSModalResponse,
    NSModalResponseOK, NSOpenPanel, NSPopUpButton, NSTextAlignment, NSTextField, NSView, NSWindow,
    NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
    ns_string, NSArray, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
    NSString, NSUserDefaults,
};

use crate::util::{ns, path_to_url, url_to_path};

const K_EXTRACT_MODE: &str = "ExtractLocationMode"; // 0 ask, 1 next to archive, 2 fixed folder
const K_EXTRACT_FOLDER: &str = "ExtractFixedFolder";
const K_DEFAULT_FORMAT: &str = "DefaultFormat"; // 0 zip, 1 7z
const K_REVEAL: &str = "RevealAfterExtract"; // default true
const K_TRASH: &str = "TrashArchiveAfterExtract"; // default false

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractMode {
    Ask,
    ArchiveFolder,
    Fixed,
}

/// Snapshot of the current preferences.
#[derive(Debug, Clone)]
pub struct Prefs {
    pub extract_mode: ExtractMode,
    pub extract_folder: Option<PathBuf>,
    pub default_7z: bool,
    pub reveal: bool,
    pub trash_after_extract: bool,
}

impl Prefs {
    pub fn load() -> Self {
        let d = NSUserDefaults::standardUserDefaults();
        let extract_mode = match d.integerForKey(&ns(K_EXTRACT_MODE)) {
            1 => ExtractMode::ArchiveFolder,
            2 => ExtractMode::Fixed,
            _ => ExtractMode::Ask,
        };
        let extract_folder = d
            .stringForKey(&ns(K_EXTRACT_FOLDER))
            .map(|s| PathBuf::from(s.to_string()))
            .filter(|p| p.is_dir());
        let reveal = d
            .objectForKey(&ns(K_REVEAL))
            .map(|_| d.boolForKey(&ns(K_REVEAL)))
            .unwrap_or(true);
        Self {
            extract_mode: if extract_mode == ExtractMode::Fixed && extract_folder.is_none() {
                ExtractMode::Ask
            } else {
                extract_mode
            },
            extract_folder,
            default_7z: d.integerForKey(&ns(K_DEFAULT_FORMAT)) == 1,
            reveal,
            trash_after_extract: d.boolForKey(&ns(K_TRASH)),
        }
    }
}

struct Controls {
    window: Retained<NSWindow>,
    mode: Retained<NSPopUpButton>,
    folder_label: Retained<NSTextField>,
    folder_button: Retained<NSButton>,
    format: Retained<NSPopUpButton>,
    reveal: Retained<NSButton>,
    trash: Retained<NSButton>,
}

#[derive(Default)]
pub struct PrefsIvars {
    controls: RefCell<Option<Controls>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ZipperPreferences"]
    #[ivars = PrefsIvars]
    pub struct PrefsController;

    unsafe impl NSObjectProtocol for PrefsController {}

    unsafe impl NSWindowDelegate for PrefsController {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _n: &NSNotification) {}
    }

    impl PrefsController {
        #[unsafe(method(modeChanged:))]
        fn mode_changed(&self, _sender: Option<&AnyObject>) {
            let idx = {
                let c = self.ivars().controls.borrow();
                let Some(c) = c.as_ref() else { return };
                c.mode.indexOfSelectedItem()
            };
            NSUserDefaults::standardUserDefaults().setInteger_forKey(idx, &ns(K_EXTRACT_MODE));
            if idx == 2 && Prefs::load().extract_folder.is_none() {
                self.choose_folder();
            } else {
                self.sync_from_defaults();
            }
        }

        #[unsafe(method(chooseFolder:))]
        fn choose_folder_action(&self, _sender: Option<&AnyObject>) {
            self.choose_folder();
        }

        #[unsafe(method(formatChanged:))]
        fn format_changed(&self, _sender: Option<&AnyObject>) {
            let c = self.ivars().controls.borrow();
            let Some(c) = c.as_ref() else { return };
            NSUserDefaults::standardUserDefaults()
                .setInteger_forKey(c.format.indexOfSelectedItem(), &ns(K_DEFAULT_FORMAT));
        }

        #[unsafe(method(revealChanged:))]
        fn reveal_changed(&self, _sender: Option<&AnyObject>) {
            let c = self.ivars().controls.borrow();
            let Some(c) = c.as_ref() else { return };
            NSUserDefaults::standardUserDefaults()
                .setBool_forKey(c.reveal.state() == NSControlStateValueOn, &ns(K_REVEAL));
        }

        #[unsafe(method(makeDefault:))]
        fn make_default(&self, _sender: Option<&AnyObject>) {
            make_default_for(vec!["zip", "7z", "tar.gz", "tar", "xz"]);
        }

        #[unsafe(method(trashChanged:))]
        fn trash_changed(&self, _sender: Option<&AnyObject>) {
            let c = self.ivars().controls.borrow();
            let Some(c) = c.as_ref() else { return };
            NSUserDefaults::standardUserDefaults()
                .setBool_forKey(c.trash.state() == NSControlStateValueOn, &ns(K_TRASH));
        }
    }
);

thread_local! {
    static PREFS: RefCell<Option<Retained<PrefsController>>> = const { RefCell::new(None) };
}

/// Show (or bring forward) the Preferences window.
pub fn show(mtm: MainThreadMarker) {
    let existing = PREFS.with(|p| p.borrow().clone());
    let ctl = match existing {
        Some(c) => c,
        None => {
            let c = PrefsController::new(mtm);
            PREFS.with(|p| *p.borrow_mut() = Some(c.clone()));
            c
        }
    };
    ctl.sync_from_defaults();
    let window = ctl
        .ivars()
        .controls
        .borrow()
        .as_ref()
        .map(|c| c.window.clone());
    if let Some(w) = window {
        w.makeKeyAndOrderFront(None);
    }
}

impl PrefsController {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(PrefsIvars::default());
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        this.build(mtm);
        this
    }

    fn build(&self, mtm: MainThreadMarker) {
        let me: &AnyObject = self;
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(480.0, 236.0));
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(ns_string!("Zipper Settings"));
        window.setDelegate(Some(ProtocolObject::from_ref(self)));
        let root = window.contentView().expect("content view");

        let label = |text: &str, y: f64| {
            let l = NSTextField::labelWithString(&ns(text), mtm);
            l.setFrame(NSRect::new(NSPoint::new(20.0, y), NSSize::new(150.0, 20.0)));
            l.setAlignment(NSTextAlignment::Right);
            root.addSubview(&l);
        };
        let popup = |y: f64, items: &[&str], action: objc2::runtime::Sel| {
            let p = unsafe {
                NSPopUpButton::initWithFrame_pullsDown(
                    NSPopUpButton::alloc(mtm),
                    NSRect::new(NSPoint::new(180.0, y - 3.0), NSSize::new(270.0, 26.0)),
                    false,
                )
            };
            let titles: Vec<Retained<NSString>> = items.iter().map(|s| ns(s)).collect();
            p.addItemsWithTitles(&NSArray::from_retained_slice(&titles));
            unsafe {
                p.setTarget(Some(me));
                p.setAction(Some(action));
            }
            root.addSubview(&p);
            p
        };

        label("Extract to:", 190.0);
        let mode = popup(
            190.0,
            &["Ask every time", "The archive’s folder", "A fixed folder…"],
            sel!(modeChanged:),
        );

        let folder_label = NSTextField::labelWithString(&ns(""), mtm);
        folder_label.setFrame(NSRect::new(
            NSPoint::new(180.0, 160.0),
            NSSize::new(180.0, 20.0),
        ));
        unsafe { folder_label.setUsesSingleLineMode(true) };
        folder_label.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingMiddle);
        folder_label.setTextColor(Some(&objc2_app_kit::NSColor::secondaryLabelColor()));
        root.addSubview(&folder_label);
        let folder_button = unsafe {
            NSButton::buttonWithTitle_target_action(
                &ns("Choose…"),
                Some(me),
                Some(sel!(chooseFolder:)),
                mtm,
            )
        };
        folder_button.setFrame(NSRect::new(
            NSPoint::new(362.0, 155.0),
            NSSize::new(90.0, 28.0),
        ));
        root.addSubview(&folder_button);

        label("New archives:", 122.0);
        let format = popup(122.0, &["zip", "7z"], sel!(formatChanged:));

        let reveal = unsafe {
            NSButton::checkboxWithTitle_target_action(
                &ns("Show extracted files in Finder"),
                Some(me),
                Some(sel!(revealChanged:)),
                mtm,
            )
        };
        reveal.setFrame(NSRect::new(
            NSPoint::new(180.0, 84.0),
            NSSize::new(280.0, 20.0),
        ));
        root.addSubview(&reveal);

        let trash = unsafe {
            NSButton::checkboxWithTitle_target_action(
                &ns("Move archive to Trash after extracting everything"),
                Some(me),
                Some(sel!(trashChanged:)),
                mtm,
            )
        };
        trash.setFrame(NSRect::new(
            NSPoint::new(180.0, 56.0),
            NSSize::new(290.0, 20.0),
        ));
        root.addSubview(&trash);

        let default_button = unsafe {
            NSButton::buttonWithTitle_target_action(
                &ns("Open zip, 7z and tar files with Zipper…"),
                Some(me),
                Some(sel!(makeDefault:)),
                mtm,
            )
        };
        default_button.setFrame(NSRect::new(
            NSPoint::new(176.0, 20.0),
            NSSize::new(290.0, 28.0),
        ));
        root.addSubview(&default_button);

        let note = NSTextField::wrappingLabelWithString(&ns("macOS asks once per file type."), mtm);
        note.setFrame(NSRect::new(
            NSPoint::new(180.0, 2.0),
            NSSize::new(280.0, 18.0),
        ));
        note.setTextColor(Some(&objc2_app_kit::NSColor::secondaryLabelColor()));
        note.setFont(Some(&objc2_app_kit::NSFont::systemFontOfSize(11.0)));
        root.addSubview(&note);

        window.center();
        *self.ivars().controls.borrow_mut() = Some(Controls {
            window,
            mode,
            folder_label,
            folder_button,
            format,
            reveal,
            trash,
        });
    }

    fn sync_from_defaults(&self) {
        let c = self.ivars().controls.borrow();
        let Some(c) = c.as_ref() else { return };
        let p = Prefs::load();
        c.mode.selectItemAtIndex(match p.extract_mode {
            ExtractMode::Ask => 0,
            ExtractMode::ArchiveFolder => 1,
            ExtractMode::Fixed => 2,
        });
        c.format.selectItemAtIndex(if p.default_7z { 1 } else { 0 });
        c.reveal.setState(if p.reveal {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
        c.trash.setState(if p.trash_after_extract {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
        self.refresh(c);
    }

    fn refresh(&self, c: &Controls) {
        let p = Prefs::load();
        let fixed = c.mode.indexOfSelectedItem() == 2;
        c.folder_button.setEnabled(fixed);
        c.folder_label
            .setStringValue(&ns(&match (&p.extract_folder, fixed) {
                (Some(f), true) => f.display().to_string(),
                (None, true) => "No folder chosen".into(),
                _ => String::new(),
            }));
    }

    fn choose_folder(&self) {
        let mtm = self.mtm();
        let window = self
            .ivars()
            .controls
            .borrow()
            .as_ref()
            .map(|c| c.window.clone());
        let Some(window) = window else { return };
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseDirectories(true);
        panel.setCanChooseFiles(false);
        panel.setCanCreateDirectories(true);
        panel.setPrompt(Some(&ns("Use this folder")));
        if let Some(f) = Prefs::load().extract_folder {
            panel.setDirectoryURL(Some(&path_to_url(&f)));
        }
        let me = self.retain_self();
        let panel_c = panel.clone();
        let block = RcBlock::new(move |resp: NSModalResponse| {
            let d = NSUserDefaults::standardUserDefaults();
            if resp == NSModalResponseOK {
                if let Some(p) = panel_c.URL().and_then(|u| url_to_path(&u)) {
                    let v: Retained<NSString> = ns(&p.to_string_lossy());
                    let obj: &AnyObject = &v;
                    unsafe { d.setObject_forKey(Some(obj), &ns(K_EXTRACT_FOLDER)) };
                    d.setInteger_forKey(2, &ns(K_EXTRACT_MODE));
                }
            }
            if Prefs::load().extract_folder.is_none() && d.integerForKey(&ns(K_EXTRACT_MODE)) == 2 {
                // Cancelled without a folder: fall back to asking.
                d.setInteger_forKey(0, &ns(K_EXTRACT_MODE));
            }
            me.sync_from_defaults();
        });
        panel.beginSheetModalForWindow_completionHandler(&window, &block);
    }

    fn retain_self(&self) -> Retained<Self> {
        use objc2::Message;
        self.retain()
    }
}

/// Convenience for the Save panel: the preferred format index (0 zip, 1 7z).
pub fn default_format_index() -> isize {
    if Prefs::load().default_7z {
        1
    } else {
        0
    }
}

/// Ask macOS to make this app the default for the given extensions, one
/// system confirmation at a time. Content types are taken from empty sample
/// files, which is what the API wants (no UTType bindings needed).
fn make_default_for(mut exts: Vec<&'static str>) {
    let Some(ext) = exts.first().copied() else {
        return;
    };
    exts.remove(0);
    let dir = std::env::temp_dir().join("Zipper").join("defaults");
    let _ = std::fs::create_dir_all(&dir);
    let sample = dir.join(format!("sample.{ext}"));
    let _ = std::fs::write(&sample, b"");
    let app_url = objc2_foundation::NSBundle::mainBundle().bundleURL();
    let rest = exts.clone();
    let block = RcBlock::new(move |_err: *mut objc2_foundation::NSError| {
        let rest = rest.clone();
        dispatch2::DispatchQueue::main().exec_async(move || make_default_for(rest));
    });
    objc2_app_kit::NSWorkspace::sharedWorkspace()
        .setDefaultApplicationAtURL_toOpenContentTypeOfFileAtURL_completionHandler(
            &app_url,
            &path_to_url(&sample),
            Some(&block),
        );
}

#[allow(dead_code)]
fn _views(_: &NSView) {}
