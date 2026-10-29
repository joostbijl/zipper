//! Minimal QuickLookUI bindings: just `QLPreviewPanel` and its data source.
//!
//! The `objc2-quick-look-ui` crate links the framework unconditionally and so
//! cannot be type-checked on Linux (see the `linux-check` feature); these few
//! declarations cover everything Zipper needs and link only on Apple targets.
#![allow(non_snake_case, clippy::missing_safety_doc)]

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{extern_class, extern_conformance, extern_methods, extern_protocol, MainThreadMarker};
use objc2_app_kit::{NSPanel, NSResponder, NSWindow};
use objc2_foundation::{NSObject, NSObjectProtocol, NSString, NSURL};

#[cfg_attr(
    target_vendor = "apple",
    link(name = "QuickLookUI", kind = "framework")
)]
extern "C" {}

extern_class!(
    #[unsafe(super(NSPanel, NSWindow, NSResponder, NSObject))]
    #[derive(Debug, PartialEq, Eq, Hash)]
    pub struct QLPreviewPanel;
);

impl QLPreviewPanel {
    extern_methods!(
        #[unsafe(method(sharedPreviewPanel))]
        #[unsafe(method_family = none)]
        pub fn sharedPreviewPanel(mtm: MainThreadMarker) -> Option<Retained<QLPreviewPanel>>;

        #[unsafe(method(sharedPreviewPanelExists))]
        #[unsafe(method_family = none)]
        pub fn sharedPreviewPanelExists(mtm: MainThreadMarker) -> bool;

        #[unsafe(method(setDataSource:))]
        #[unsafe(method_family = none)]
        pub unsafe fn setDataSource(
            &self,
            data_source: Option<&ProtocolObject<dyn QLPreviewPanelDataSource>>,
        );

        #[unsafe(method(reloadData))]
        #[unsafe(method_family = none)]
        pub fn reloadData(&self);

        #[unsafe(method(updateController))]
        #[unsafe(method_family = none)]
        pub fn updateController(&self);
    );
}

extern_protocol!(
    pub unsafe trait QLPreviewItem: NSObjectProtocol {
        #[unsafe(method(previewItemURL))]
        #[unsafe(method_family = none)]
        unsafe fn previewItemURL(&self) -> Option<Retained<NSURL>>;

        #[optional]
        #[unsafe(method(previewItemTitle))]
        #[unsafe(method_family = none)]
        unsafe fn previewItemTitle(&self) -> Option<Retained<NSString>>;
    }
);

extern_conformance!(
    unsafe impl QLPreviewItem for NSURL {}
);

extern_protocol!(
    pub unsafe trait QLPreviewPanelDataSource {
        #[unsafe(method(numberOfPreviewItemsInPreviewPanel:))]
        #[unsafe(method_family = none)]
        unsafe fn numberOfPreviewItemsInPreviewPanel(
            &self,
            panel: Option<&QLPreviewPanel>,
        ) -> isize;

        #[unsafe(method(previewPanel:previewItemAtIndex:))]
        #[unsafe(method_family = none)]
        unsafe fn previewPanel_previewItemAtIndex(
            &self,
            panel: Option<&QLPreviewPanel>,
            index: isize,
        ) -> Option<Retained<ProtocolObject<dyn QLPreviewItem>>>;
    }
);
