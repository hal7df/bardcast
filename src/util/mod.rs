///! Utility types and functions that have broad applicable use throughout the
///! application.

pub mod fmt;
pub mod io;
pub mod task;

use log::warn;
use tokio::sync::watch::Receiver;

///! Checks whether the given [`Receiver`] for application shutdown has
///! been issued a shutdown notification without `await`ing.
pub fn check_shutdown_rx(rx: &mut Receiver<bool>) -> bool {
    if let Ok(has_changed) = rx.has_changed() {
        !has_changed || *rx.borrow_and_update()
    } else {
        warn!("Shutdown notifier closed unexpectedly, terminating task");
        false
    }
}
