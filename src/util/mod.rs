///! Utility types and functions that have broad applicable use throughout the
///! application.

pub mod fmt;
pub mod io;
pub mod task;

use log::warn;
use tokio::sync::watch::Receiver as WatchReceiver;
use tokio::sync::oneshot::Receiver as OneshotReceiver;
use tokio::sync::oneshot::error::TryRecvError;

///! Checks whether the given [`WatchReceiver`] for application shutdown has
///! been issued a shutdown notification without `await`ing.
pub fn check_shutdown_rx(rx: &mut WatchReceiver<bool>) -> bool {
    if let Ok(has_changed) = rx.has_changed() {
        !has_changed || *rx.borrow_and_update()
    } else {
        warn!("Shutdown notifier closed unexpectedly, terminating task");
        false
    }
}

///! Checks whether the given unit [`OneshotReceiver`] has received a
///! notification, without `await`ing.
pub fn check_oneshot_rx(rx: &mut OneshotReceiver<()>) -> bool {
    if let Err(e) = rx.try_recv() {
        e == TryRecvError::Empty
    } else {
        false
    }
}

///! Manual reimplementation for [`Option::is_some_and`] for compatibility with
///! stable Rust versions prior to 1.70.0.
///!
///! TODO: This should be removed after support for older versions is dropped.
pub fn opt_is_some_and<T, F: FnOnce(&T) -> bool>(opt: &Option<T>, predicate: F) -> bool {
    if let Some(t) = opt.as_ref() {
        predicate(t)
    } else {
        false
    }
}
