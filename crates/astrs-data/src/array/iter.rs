//! The shared null-aware iterator every array family reuses.
//!
//! Eleven array types need the same iterator: yield `Option<T>`, `None` for a
//! null slot, exact size, double-ended. Rather than write it eleven times,
//! each array implements [`ArrayAccessor`] on its *reference* type — which is
//! what lets `StringArray` yield `&'a str` without copying — and gets
//! [`ArrayIter`] for free.
//!
//! ```
//! use astrs_data::array::{Array, StringArray};
//!
//! let names = StringArray::from_opt_iter([Some("lidar"), None, Some("imu")]);
//! let collected: Vec<Option<&str>> = names.iter().collect();
//! assert_eq!(collected, vec![Some("lidar"), None, Some("imu")]);
//! assert_eq!(names.iter().rev().flatten().collect::<Vec<_>>(), vec!["imu", "lidar"]);
//! ```

/// Random access to an array's logical values, null-aware.
///
/// Implemented on `&'a ConcreteArray` rather than on `ConcreteArray` so that
/// borrowing value types (`&str`, `&[u8]`, [`crate::ArrayRef`]) can be
/// returned without a copy.
pub trait ArrayAccessor {
    /// The logical value type, e.g. `i32`, `&'a str` or `&'a [u8]`.
    type Item;

    /// Number of logical slots.
    fn accessor_len(&self) -> usize;

    /// The value at `index`, or `None` when the slot is null or out of range.
    fn accessor_get(&self, index: usize) -> Option<Self::Item>;
}

/// Double-ended, exact-size iterator over an array's logical values.
///
/// Yields `None` for null slots, so the item type is `Option<A::Item>`.
#[derive(Debug, Clone)]
pub struct ArrayIter<A: ArrayAccessor> {
    /// The borrowed array.
    array: A,
    /// Next index from the front.
    front: usize,
    /// One past the next index from the back.
    back: usize,
}

impl<A: ArrayAccessor> ArrayIter<A> {
    /// Starts an iteration over the whole array.
    #[must_use]
    pub fn new(array: A) -> Self {
        let back = array.accessor_len();
        Self {
            array,
            front: 0,
            back,
        }
    }
}

impl<A: ArrayAccessor> Iterator for ArrayIter<A> {
    type Item = Option<A::Item>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.front >= self.back {
            return None;
        }
        let value = self.array.accessor_get(self.front);
        self.front += 1;
        Some(value)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.back - self.front;
        (remaining, Some(remaining))
    }

    #[inline]
    fn count(self) -> usize {
        self.back - self.front
    }

    #[inline]
    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        self.front = self.front.saturating_add(n);
        self.next()
    }
}

impl<A: ArrayAccessor> DoubleEndedIterator for ArrayIter<A> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.front >= self.back {
            return None;
        }
        self.back -= 1;
        Some(self.array.accessor_get(self.back))
    }
}

impl<A: ArrayAccessor> ExactSizeIterator for ArrayIter<A> {}

impl<A: ArrayAccessor> std::iter::FusedIterator for ArrayIter<A> {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use crate::array::{Int32Array, StringArray};

    #[test]
    fn iterates_forwards_and_backwards() {
        let values = Int32Array::from_opt_iter([Some(1), None, Some(3), Some(4)]);
        let mut iter = values.iter();
        assert_eq!(iter.len(), 4);
        assert_eq!(iter.next(), Some(Some(1)));
        assert_eq!(iter.next_back(), Some(Some(4)));
        assert_eq!(iter.len(), 2);
        assert_eq!(iter.collect::<Vec<_>>(), vec![None, Some(3)]);
    }

    #[test]
    fn exhausted_iterators_stay_exhausted() {
        let values = Int32Array::from_values([1]);
        let mut iter = values.iter();
        assert_eq!(iter.next(), Some(Some(1)));
        assert_eq!(iter.next(), None);
        assert_eq!(iter.next(), None);
        assert_eq!(iter.next_back(), None);
    }

    #[test]
    fn empty_array_iterates_zero_times() {
        let values = Int32Array::from_values([] as [i32; 0]);
        assert_eq!(values.iter().count(), 0);
        assert_eq!(values.iter().len(), 0);
        assert_eq!(values.iter().next(), None);
    }

    #[test]
    fn nth_skips() {
        let values = Int32Array::from_values([0, 1, 2, 3, 4]);
        let mut iter = values.iter();
        assert_eq!(iter.nth(2), Some(Some(2)));
        assert_eq!(iter.next(), Some(Some(3)));
        assert_eq!(iter.nth(99), None);
    }

    #[test]
    fn borrowing_items_keep_the_array_lifetime() {
        let names = StringArray::from_opt_iter([Some("a"), None, Some("ccc")]);
        let lengths: Vec<Option<usize>> = names.iter().map(|s| s.map(str::len)).collect();
        assert_eq!(lengths, vec![Some(1), None, Some(3)]);
        assert_eq!(names.iter().flatten().count(), 2);
    }

    #[test]
    fn count_short_circuits() {
        let values = Int32Array::from_values([1, 2, 3]);
        assert_eq!(values.iter().count(), 3);
        let mut iter = values.iter();
        let _ = iter.next();
        assert_eq!(iter.count(), 2);
    }
}
