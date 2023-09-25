///! Utility types and functions that have broad applicable use throughout the
///! application.

pub mod fmt;
pub mod io;
pub mod task;

use std::mem::{self, ManuallyDrop};
use std::ops::{Deref, DerefMut, Drop};

use log::{debug, warn};
use tokio::sync::oneshot::{
    self,
    Receiver as OneshotReceiver,
    Sender as OneshotSender
};
use tokio::sync::watch::Receiver as WatchReceiver;

// TYPE DEFINITIONS ************************************************************

/// Internal state representation used by [`Lessor`].
enum LessorState<T> {
    Owned(T),
    Leased(OneshotReceiver<T>),
    Processing
}

/// Mechanism for sharing ownership over a value without using any form of
/// mutual exclusion.
///
/// By default, the `Lessor` owns the value it wraps, and can be accessed as
/// normal via the [`Lessor::as_ref()`], [`Lessor::as_mut()`], and
/// [`Lessor::take()`] methods. Ownership of the value can be passed using
/// [`Lessor::lease()`], which will return the leased value to the `Lessor`
/// when the lease is dropped. Values cannot be taken from a lease.
pub struct Lessor<T>(LessorState<T>);

/// Wraps an owned value provided by a [`Lessor`], returning ownership upon
/// being dropped.
///
/// A raw owned value cannot be taken from a `Lease`.
pub struct Lease<T> {
    value: ManuallyDrop<T>,
    tx: OneshotSender<T>,
}

// TYPE IMPLS ******************************************************************
impl<T> LessorState<T> {
    /// Returns true if the value controlled by the lessor is currently leased.
    fn is_leased(&self) -> bool {
        !matches!(self, Self::Owned(_))
    }

    /// Attempts to take ownership of the controlled value, if present. If not
    /// present, returns the current state.
    fn take(self) -> Result<T, Self> {
        if let Self::Owned(value) = self {
            Ok(value)
        } else {
            Err(self)
        }
    }

    /// Waits for the active lease to be returned, and returns an owned lessor
    /// state wrapping the value.
    ///
    /// Panics if the lease dropped the return channel without sending the
    /// value.
    async fn await_release(self) -> Self {
        match self {
            Self::Leased(rx) => {
                if let Ok(value) = rx.await {
                    Self::Owned(value)
                } else {
                    panic!("Leased value went out of scope without releasing");
                }
            },
            state => {
                debug!(
                    "Attempted to await release on a value that was not \
                     leased, or while the Lessor was processing"
                );
                state
            }
        }
    }
}

impl<T> Lessor<T> {
    /// Creates a new Lessor responsible for the given value.
    fn new(value: T) -> Self {
        Self(LessorState::Owned(value))
    }

    /// Returns true if the value controlled by the `Lessor` is currently
    /// leased.
    fn is_leased(&self) -> bool {
        !self.0.is_leased()
    }

    /// Takes an immutable reference to the controlled value, if currently
    /// owned by the `Lessor1.
    fn as_ref(&self) -> Option<&T> {
        if let LessorState::Owned(value) = &self.0 {
            Some(value)
        } else {
            None
        }
    }

    /// Takes a mutable reference to the controlled value, if currently owned
    /// by the `Lessor`.
    fn as_mut(&mut self) -> Option<&mut T> {
        if let LessorState::Owned(value) = &mut self.0 {
            Some(value)
        } else {
            None
        }
    }

    /// Consumes this `Lessor`, returning the raw owned value if it is currently
    /// owned. If the value is currently leased, this will return `None` and
    /// the value will be dropped when the current lease is dropped.
    fn take(self) -> Option<T> {
        self.0.take().ok()
    }

    /// Creates a [`Lease`] object providing exclusive access to the controlled
    /// value, if the value is currently owned by the `Lessor`. Returns `None`
    /// if the value is currently leased.
    ///
    /// Once called, [`Lessor::await_release()`] must be called for the `Lessor`
    /// to regain ownership of the controlled value.
    fn lease(&mut self) -> Option<Lease<T>> {
        match mem::replace(&mut self.0, LessorState::Processing).take() {
            Ok(value) => {
                let (tx, rx) = oneshot::channel::<T>();
                self.0 = LessorState::Leased(rx);

                Some(Lease {
                    value: ManuallyDrop::new(value),
                    tx,
                })
            },
            Err(state) => {
                self.0 = state;
                None
            }
        }
    }

    /// Waits for the active [`Lease`] object to be dropped, and retakes
    /// ownership of the controlled value.
    async fn await_release(&mut self) {
        self.0 = mem::replace(
            &mut self.0,
            LessorState::Processing
        ).await_release().await
    }
}

// TRAIT IMPLS *****************************************************************
impl<T> Deref for Lease<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.value.deref()
    }
}

impl<T> DerefMut for Lease<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.value.deref_mut()
    }
}

impl<T> Drop for Lease<T> {
    fn drop(&mut self) {
        unsafe {
            let value = ManuallyDrop::take(&mut self.value);

            if self.tx.send(value).is_err() {
                debug!("Lessor went out of scope while value lease was active");
            }
        }
    }
}

// PUBLIC HELPER FUNCTIONS *****************************************************

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
