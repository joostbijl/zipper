//! Zipper — native macOS archive manager. See SPEC.md.
#![allow(unused_unsafe)]
#![allow(clippy::too_many_arguments)]
#![allow(deprecated)]

mod app;
mod document;
mod node;
mod sheets;
mod tasks;
mod util;

use objc2::runtime::ProtocolObject;
use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;

fn main() {
    let mtm = MainThreadMarker::new().expect("must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    let delegate = app::AppDelegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
}
