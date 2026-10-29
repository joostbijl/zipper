//! Threading glue: run archive work on a background thread and hand results
//! back to AppKit objects on the main thread.

use std::sync::mpsc;
use std::sync::Arc;

use dispatch2::{DispatchQueue, MainThreadBound};
use objc2::rc::Retained;
use objc2::MainThreadMarker;

/// A `Send + Sync + Clone` handle to a main-thread-only object. Worker
/// threads hold one of these and use [`MainHandle::on_main`] to touch the
/// object.
pub struct MainHandle<T>(Arc<MainThreadBound<Retained<T>>>);

impl<T> Clone for MainHandle<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: 'static> MainHandle<T> {
    pub fn new(obj: Retained<T>, mtm: MainThreadMarker) -> Self {
        Self(Arc::new(MainThreadBound::new(obj, mtm)))
    }

    /// Run `f` on the main thread, asynchronously, with the object.
    pub fn on_main<F>(&self, f: F)
    where
        F: FnOnce(&T, MainThreadMarker) + Send + 'static,
    {
        let h = self.clone();
        DispatchQueue::main().exec_async(move || {
            // SAFETY: the main queue runs on the main thread.
            let mtm = unsafe { MainThreadMarker::new_unchecked() };
            f(h.0.get(mtm), mtm);
        });
    }

    /// Ask the main thread a question and block until it answers.
    ///
    /// `f` runs on the main thread and receives a `reply` callback; it must
    /// arrange for `reply` to be called exactly once (typically from a sheet's
    /// completion handler). The worker thread blocks meanwhile. Never call
    /// this from the main thread.
    pub fn ask<R, F>(&self, f: F) -> R
    where
        R: Send + 'static,
        F: FnOnce(&T, MainThreadMarker, Reply<R>) + Send + 'static,
    {
        assert!(
            MainThreadMarker::new().is_none(),
            "ask() would deadlock on the main thread"
        );
        let (tx, rx) = mpsc::channel::<R>();
        self.on_main(move |obj, mtm| f(obj, mtm, Reply(tx)));
        rx.recv()
            .expect("main thread dropped the reply without answering")
    }
}

/// One-shot reply channel handed to the main thread by [`MainHandle::ask`].
/// Cloneable so it can live inside a `Fn` completion block.
#[derive(Clone)]
pub struct Reply<R>(mpsc::Sender<R>);

impl<R> Reply<R> {
    pub fn send(&self, value: R) {
        let _ = self.0.send(value);
    }
}

/// Spawn a named worker thread.
pub fn spawn<F>(name: &str, f: F)
where
    F: FnOnce() + Send + 'static,
{
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(f)
        .expect("spawn worker thread");
}
