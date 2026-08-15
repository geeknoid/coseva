//! Field-endpoint encoding shared by the owned record types.
//!
//! Owned records store field boundaries as cumulative end offsets: the start
//! of field `i` is the end of field `i - 1`, or `0`. An explicit NULL field is
//! encoded allocation-free by setting the top bit of its end offset. NULL
//! fields always have zero length, so the bit never needs to survive offset
//! arithmetic beyond a plain copy.

#[cfg(test)]
use core::cell::Cell;
#[cfg(feature = "serde")]
use core::slice;

pub(crate) const MAX_FIELD_OFFSET: usize = coseva_unsafe::storage::MAX_FIELD_OFFSET;

// Test-only override of `MAX_FIELD_OFFSET`, so a unit test can exercise the
// `encode_end` guard below without building a real 2 GiB record to reach it.
// Production code always compares against `MAX_FIELD_OFFSET` directly; this
// seam only exists under `#[cfg(test)]`, and mirrors the `TEST_MAX_OFFSET`
// seam guarding the structurally identical packing in `Span`
// (`src/engine/framing.rs`).
#[cfg(test)]
std::thread_local! {
    pub(crate) static TEST_MAX_FIELD_OFFSET: Cell<Option<usize>> =
        const { Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn max_field_offset() -> usize {
    TEST_MAX_FIELD_OFFSET
        .with(Cell::get)
        .unwrap_or(MAX_FIELD_OFFSET)
}

#[cfg(not(test))]
pub(crate) const fn max_field_offset() -> usize {
    MAX_FIELD_OFFSET
}

pub(crate) const fn end_offset(raw: usize) -> usize {
    coseva_unsafe::storage::end_offset(raw)
}

pub(crate) const fn end_is_null(raw: usize) -> bool {
    coseva_unsafe::storage::end_is_null(raw)
}

/// Encode one cumulative field endpoint, packing its explicit-NULL bit into
/// the top bit of the offset.
///
/// # Panics
///
/// Panics if `offset` exceeds [`MAX_FIELD_OFFSET`]. Such an offset would
/// alias the NULL flag, so the record would silently misreport both that
/// field's NULL state and every subsequent field boundary; refusing to build
/// the record is the only outcome that does not corrupt it. On 64-bit targets
/// the bound is beyond any possible allocation and the check never fires. On
/// 32-bit targets it is `2 GiB - 1`, and the parser can never reach it —
/// `Engine::parse_positioned_record` already rejects any window longer than
/// `Span::MAX_OFFSET`, which is half this bound, and an owned record's bytes
/// never exceed the window it was parsed from. Only a record grown past 2 GiB
/// through the owning mutation APIs can trip it.
#[cfg(test)]
pub(crate) fn encode_end(offset: usize, is_null: bool) -> usize {
    coseva_unsafe::storage::encode_end_bounded(offset, is_null, max_field_offset())
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod bound_tests {
    use super::{MAX_FIELD_OFFSET, TEST_MAX_FIELD_OFFSET, encode_end, end_is_null, end_offset};
    use crate::byte_record::ByteRecord;

    /// Shrinks the endpoint bound for the duration of one test, restoring it
    /// on drop so a panicking test cannot leak the override into the next one
    /// running on the same thread.
    struct MaxFieldOffsetGuard(Option<usize>);

    impl MaxFieldOffsetGuard {
        fn shrink_to(bound: usize) -> Self {
            let previous = TEST_MAX_FIELD_OFFSET.with(|cell| cell.replace(Some(bound)));
            Self(previous)
        }
    }

    impl Drop for MaxFieldOffsetGuard {
        fn drop(&mut self) {
            TEST_MAX_FIELD_OFFSET.with(|cell| cell.set(self.0));
        }
    }

    /// An endpoint that fits the bound must round-trip both its offset and
    /// its NULL bit. This is the negative control for the panic test below:
    /// without it, a guard that rejected everything would also pass.
    #[test]
    fn an_endpoint_within_the_bound_round_trips() {
        let _guard = MaxFieldOffsetGuard::shrink_to(8);

        let plain = encode_end(8, false);
        assert_eq!(end_offset(plain), 8);
        assert!(!end_is_null(plain));

        let null = encode_end(8, true);
        assert_eq!(end_offset(null), 8);
        assert!(end_is_null(null));
    }

    /// Growing a record past the endpoint bound must abort the mutation
    /// rather than fold the excess into the NULL flag, which would make the
    /// record misreport that field as NULL and place every later field
    /// boundary `NULL_END_FLAG` bytes too low. A real 2 GiB record is
    /// unreachable in a unit test on a 64-bit host, so the bound is shrunk
    /// instead — the same seam the structurally identical `Span` packing uses
    /// in `engine::framing`.
    #[test]
    #[should_panic(expected = "field offset overflows into the NULL flag bit")]
    fn pushing_a_field_past_the_endpoint_bound_panics_instead_of_misdecoding() {
        let _guard = MaxFieldOffsetGuard::shrink_to(4);

        let mut record = ByteRecord::new();
        record.push_field(b"abcd");
        record.push_field(b"e");
    }

    /// Without a shrunk bound the guard must be invisible: `MAX_FIELD_OFFSET`
    /// itself is a legal endpoint, so the check rejects only what genuinely
    /// overflows. This pins the comparison against widening to `>=`.
    #[test]
    fn the_largest_representable_offset_is_accepted() {
        assert_eq!(
            end_offset(encode_end(MAX_FIELD_OFFSET, false)),
            MAX_FIELD_OFFSET
        );
        assert!(end_is_null(encode_end(MAX_FIELD_OFFSET, true)));
    }
}

/// Iterator over whether each field of a [`ByteRecord`] or [`TextRecord`]
/// is an explicit NULL.
#[cfg(feature = "serde")]
#[derive(Clone, Debug)]
pub(crate) struct EndpointNullFlags<'record> {
    pub(crate) ends: slice::Iter<'record, usize>,
}

#[cfg(feature = "serde")]
impl Iterator for EndpointNullFlags<'_> {
    type Item = bool;

    fn next(&mut self) -> Option<Self::Item> {
        self.ends.next().map(|&raw| end_is_null(raw))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.ends.size_hint()
    }
}

#[cfg(feature = "serde")]
impl ExactSizeIterator for EndpointNullFlags<'_> {}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(all(test, feature = "serde"))]
mod tests {
    use crate::byte_record::ByteRecord;

    #[test]
    fn endpoint_null_flags_report_an_exact_size_hint() {
        // `EndpointNullFlags` is an `ExactSizeIterator`, so its `size_hint`
        // must stay exact as the iterator is drained.
        let mut record = ByteRecord::new();
        record.push_field(b"a");
        record.push_field(b"b");
        record.push_field(b"c");

        let mut flags = record.null_flags();
        assert_eq!(flags.size_hint(), (3, Some(3)));
        assert_eq!(flags.len(), 3);
        assert_eq!(flags.next(), Some(false));
        assert_eq!(flags.size_hint(), (2, Some(2)));
        assert_eq!(flags.count(), 2);
    }
}
