///! Useful utilities for handling raw responses from the libpulse API and
///! collecting them into types that can be returned through a
///! [`super::PulseFuture`].

extern crate libpulse_binding as libpulse;

use libpulse::callbacks::ListResult;

use super::PulseResultRef;
use crate::snd::pulse::owned::IntoOwnedInfo;

/// Stores the given value in the result container.
pub fn store<T>(result: &PulseResultRef<T>, value: T) {
    *result.lock().unwrap() = value;
}

/// Increments the value in the result container.
pub fn increment(result: &PulseResultRef<usize>) {
    *result.lock().unwrap() += 1;
}

/// Stores the given value in the result container, if there is not already any
/// value stored in it.
pub fn first<T>(result: &PulseResultRef<Option<T>>, value: T) {
    let mut result = result.lock().unwrap();

    if result.is_none() {
        *result = Some(value);
    }
}

/// Stores the given value in the result container, overriding any value that
/// may have already been present.
///
/// This function is similar to [`store`], but it handles a wrapping `Option`
/// in the underlying result container.
pub fn last<T>(result: &PulseResultRef<Option<T>>, value: T) {
    store(result, Some(value));
}

/// Converts a [`ListResult`] to an [`Option`] when the [`ListResult`] is an
/// `Item`.
///
/// [`ListResult`] technically has additional states to notify the consumer when
/// the end of the list is reached or an error occurs; however, considering that
/// [`super::PulseFuture`] always watches the state of the associated
/// [`libpulse::operation::Operation`], client code rarely needs to handle this
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

/// Converts the given entity info into an owned equivalent and stores it in
/// the result container, if there is not already any value stored in it.
///
/// This accepts the raw info object as a `ListResult` as most underlying
/// libpulse introspection functions will return the info object as a list, even
/// if only one was requested.
pub fn first_info<T: IntoOwnedInfo>(
    result: &PulseResultRef<Option<T::Owned>>,
    list_result: ListResult<&T>
) {
    if let ListResult::Item(value) = list_result {
        first(result, value.into_owned());
    }
}

/// Appends the given value to the `Vec` stored in the result container.
pub fn collect<T>(result: &PulseResultRef<Vec<T>>, value: T) {
    (*result.lock().unwrap()).push(value);
}

/// Converts the given entity info into an owned equivalent and appends it to
/// the `Vec` stored in the result container.
///
/// This accepts the raw info object as a `ListResult` as most underlying
/// libpulse introspection functions will return the info object as a list, even
/// if only one was requested.
pub fn collect_info<T: IntoOwnedInfo>(
    result: &PulseResultRef<Vec<T::Owned>>,
    list_result: ListResult<&T>
) {
    if let ListResult::Item(value) = list_result {
        collect(result, value.into_owned());
    }
}
