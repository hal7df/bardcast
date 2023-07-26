///! Utility types and functions that have broad applicable use throughout the
///! application.

pub mod fmt;
pub mod io;
pub mod task;

use log::warn;
use tokio::sync::oneshot::Sender as OneshotSender;
use tokio::sync::watch::Receiver as WatchReceiver;

/// Checks whether the given [`WatchReceiver`] for application shutdown has
/// been issued a shutdown notification without `await`ing.
pub fn check_shutdown_rx(rx: &mut WatchReceiver<bool>) -> bool {
    if let Ok(has_changed) = rx.has_changed() {
        !has_changed || *rx.borrow_and_update()
    } else {
        warn!("Shutdown notifier closed unexpectedly, terminating task");
        false
    }
}

/// Attempts to send the provided value on the given channel, if it has not yet
/// been closed or dropped, returning the value if unsuccessful.
pub fn opt_oneshot_try_send<T>(
    tx: &mut Option<OneshotSender<T>>,
    value: T
) -> Result<(), T> {
    if let Some(tx) = tx.take() {
        tx.send(value)
    } else {
        Err(value)
    }
}
