//! Alert sheets: errors, password prompt, overwrite conflicts, test result.

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSAlertStyle, NSButton,
    NSControlStateValueOn, NSSecureTextField, NSTextField, NSView, NSWindow,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use archive_core::{ConflictAction, ConflictDecision};

use crate::util::ns;

/// Show an informational or error alert as a sheet (or app-modal if there
/// is no window).
pub fn alert(
    window: Option<&NSWindow>,
    title: &str,
    message: &str,
    is_error: bool,
    mtm: MainThreadMarker,
) {
    let a = NSAlert::new(mtm);
    a.setMessageText(&ns(title));
    a.setInformativeText(&ns(message));
    a.setAlertStyle(if is_error {
        NSAlertStyle::Critical
    } else {
        NSAlertStyle::Informational
    });
    a.addButtonWithTitle(&ns("OK"));
    match window {
        Some(w) => a.beginSheetModalForWindow_completionHandler(w, None),
        None => {
            let _ = a.runModal();
        }
    }
}

/// Ask for a password. `done(None)` means the user cancelled.
pub fn ask_password<F>(
    window: &NSWindow,
    archive_name: &str,
    retry: bool,
    mtm: MainThreadMarker,
    done: F,
) where
    F: Fn(Option<String>) + 'static,
{
    let a = NSAlert::new(mtm);
    a.setMessageText(&ns(if retry {
        "Wrong password"
    } else {
        "Password required"
    }));
    a.setInformativeText(&ns(&format!("Enter the password for “{archive_name}”.")));
    a.addButtonWithTitle(&ns("OK"));
    a.addButtonWithTitle(&ns("Cancel"));
    let field = unsafe {
        NSSecureTextField::initWithFrame(
            NSSecureTextField::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(260.0, 24.0)),
        )
    };
    field.setPlaceholderString(Some(&ns("Password")));
    let view: &NSView = &field;
    a.setAccessoryView(Some(view));
    // Make the field first responder once the sheet is up.
    unsafe { a.window().setInitialFirstResponder(Some(view)) };

    let field_c = field.clone();
    let alert_c = a.clone();
    let block = RcBlock::new(move |resp| {
        let _keep = &alert_c;
        if resp == NSAlertFirstButtonReturn {
            let s = field_c.stringValue().to_string();
            done(Some(s));
        } else {
            done(None);
        }
    });
    a.beginSheetModalForWindow_completionHandler(window, Some(&block));
}

/// Ask what to do about an existing file.
pub fn ask_conflict<F>(window: &NSWindow, path: &str, mtm: MainThreadMarker, done: F)
where
    F: Fn(ConflictDecision) + 'static,
{
    let a = NSAlert::new(mtm);
    a.setMessageText(&ns("File already exists"));
    a.setInformativeText(&ns(&format!(
        "“{path}” already exists in the destination folder."
    )));
    a.addButtonWithTitle(&ns("Skip"));
    a.addButtonWithTitle(&ns("Replace"));
    a.addButtonWithTitle(&ns("Keep Both"));
    let checkbox =
        unsafe { NSButton::checkboxWithTitle_target_action(&ns("Apply to all"), None, None, mtm) };
    let cb_view: &NSView = &checkbox;
    a.setAccessoryView(Some(cb_view));

    let checkbox_c = checkbox.clone();
    let alert_c = a.clone();
    let block = RcBlock::new(move |resp| {
        let _keep = &alert_c;
        let action = if resp == NSAlertFirstButtonReturn {
            ConflictAction::Skip
        } else if resp == NSAlertSecondButtonReturn {
            ConflictAction::Replace
        } else {
            ConflictAction::KeepBoth
        };
        let apply_to_all = checkbox_c.state() == NSControlStateValueOn;
        done(ConflictDecision {
            action,
            apply_to_all,
        });
    });
    a.beginSheetModalForWindow_completionHandler(window, Some(&block));
}

/// Yes/No confirmation. `done(true)` on the first (default) button.
#[allow(dead_code)] // Phase 2 (delete entries)
pub fn confirm<F>(
    window: &NSWindow,
    title: &str,
    message: &str,
    yes: &str,
    mtm: MainThreadMarker,
    done: F,
) where
    F: Fn(bool) + 'static,
{
    let a = NSAlert::new(mtm);
    a.setMessageText(&ns(title));
    a.setInformativeText(&ns(message));
    a.addButtonWithTitle(&ns(yes));
    a.addButtonWithTitle(&ns("Cancel"));
    let alert_c = a.clone();
    let block = RcBlock::new(move |resp| {
        let _keep = &alert_c;
        done(resp == NSAlertFirstButtonReturn);
    });
    a.beginSheetModalForWindow_completionHandler(window, Some(&block));
}

/// A plain label for accessory views.
pub fn label(text: &str, mtm: MainThreadMarker) -> Retained<NSTextField> {
    NSTextField::labelWithString(&ns(text), mtm)
}
