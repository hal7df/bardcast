///! Useful utilities for handling raw responses from the libpulse API and
///! collecting them into types that can be returned through a result wrapper.

extern crate libpulse_binding as libpulse;

use std::sync::{Mutex, Weak};

use log::error;
use libpulse::callbacks::ListResult;

use crate::snd::pulse::owned::IntoOwnedInfo;

// TYPE DEFINITIONS ************************************************************

/// Basic trait providing convenience functions for collecting data into a
/// shared result during an operation callback.
pub trait CollectResult<T> {
    /// Core access function to the underlying data. All other Collect traits
    /// defined in this module have default implementations based on this
    /// method.
    fn with(&self, op: impl FnOnce(&mut T));

    /// Stores the given value in the underlying result, Overwriting any
    /// previous value.
    fn store(&self, value: T) {
        self.with(|result| *result = value);
    }
}

/// Extension trait for implementations of [`CollectResult`] that wrap an
/// [`Option`].
pub trait CollectOptionalResult<T>: CollectResult<Option<T>> {
    /// Only stores the given value if no other value has been set thus far.
    fn first(&self, value: T) {
        self.with(|result| if result.is_none() {
            *result = Some(value);
        });
    }

    /// Wraps the given value in an `Option` and stores it, overwriting any
    /// previous value.
    fn last(&self, value: T) {
        self.store(Some(value));
    }
    
    /// Attempts to unwrap an info object from a [`ListResult`], storing it in
    /// the result if and only if the result has a valid info object, and no
    /// other info objects have already been stored in the result.
    fn first_info_in_list<I: IntoOwnedInfo<Owned = T>>(
        &self,
        info: ListResult<&I>
    ) {
        if let ListResult::Item(info) = info {
            self.first(info.into_owned());
        }
    }
}

/// Extension trait for implementations of [`CollectResult`] that wrap a [`Vec`].
pub trait CollectListResult<T>: CollectResult<Vec<T>> {
    /// Appends the given value to the underlying `Vec`.
    fn push(&self, value: T) {
        self.with(|result_list| result_list.push(value));
    }
    
    /// Converts the given info object into its owned equivalent before pushing
    /// to the underyling list.
    fn push_info<I: IntoOwnedInfo<Owned = T>>(&self, info: &I) {
        self.push(info.into_owned());
    }

    /// Attempts to unwrap an info object from a [`ListResult`], appending it to
    /// the underlying `Vec` if and only if the result has a valid info object.
    fn push_info_from_list<I: IntoOwnedInfo<Owned = T>>(
        &self,
        info: ListResult<&I>
    ) {
        if let ListResult::Item(info) = info {
            self.push_info(info);
        }
    }
}

// TYPE IMPLS ******************************************************************
impl<T> CollectResult<T> for Weak<Mutex<T>> {
    fn with(&self, op: impl FnOnce(&mut T)) {
        if let Some(result) = self.upgrade() {
            op(&mut result.lock().unwrap());
        } else {
            error!("Attempted to collect into dropped result reference");
        }
    }
}

impl<T> CollectOptionalResult<T> for Weak<Mutex<Option<T>>> {}

impl<T> CollectListResult<T> for Weak<Mutex<Vec<T>>> {}

// PUBLIC HELPER FUNCTIONS *****************************************************

/// Converts a [`ListResult`] to an [`Option`] when the [`ListResult`] is an
/// `Item`.
///
/// [`ListResult`] technically has additional states to notify the consumer when
/// the end of the list is reached or an error occurs; however, considering that
/// [`crate::snd::pulse::context::PulseContextWrapper`] automatically detects
/// when an operation ceases execution, client code rarely needs to handle this
/// case.
pub fn with_list<'a, T>(list_result: ListResult<&'a T>) -> Option<&'a T> {
    if let ListResult::Item(t) = list_result { Some(t) } else { None }
}

/// Produces `Some` with the contained list element iff the list result is an
/// `Item` and the contained item matches the given `predicate`.
pub fn filter_list<'a, T, P: Fn(&'a T) -> bool>(
    list_result: ListResult<&'a T>,
    predicate: P,
) -> Option<&'a T> {
    with_list(list_result).and_then(move |t| if predicate(t) {
        Some(t)
    } else {
        None
    })
}
