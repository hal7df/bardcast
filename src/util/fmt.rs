///! Utilities for determining the displayed length of strings and types that
///! implement [`std::fmt::Display`].

use std::cmp;

/// Trait for types that can compute their display length when printed to the
/// console.
pub trait DisplayLength {
    fn display_length(&self) -> usize;
}

impl DisplayLength for str {
    fn display_length(&self) -> usize {
        self.len()
    }
}

impl<T: ToString> DisplayLength for T {
    fn display_length(&self) -> usize {
        self.to_string().len()
    }
}

/// Macro that computes the maximum display length of any number of values
/// implementing [`DisplayLength`].
macro_rules! display_length {
    ( $first:expr, $( $x:expr ),* ) => {
        {
            let mut max_size = $crate::util::fmt::DisplayLength::display_length($first);
            $(
                max_size = ::std::cmp::max(
                    max_size,
                    $crate::util::fmt::DisplayLength::display_length($x)
                );
            )*
            max_size
        }
    };
}

/// Computes the maximum display length for a sequence of items implementing
/// [`DisplayLength`].
pub fn max_display_length<'a>(items: impl IntoIterator<Item = impl DisplayLength>) -> usize {
    items.into_iter().fold(
        0,
        |cur_max, item| cmp::max(cur_max, item.display_length())
    )
}

pub(crate) use display_length;
