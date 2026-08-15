//! The rule deciding when a reused buffer gives its capacity back.
//!
//! Parsers and emitters reuse their buffers across records, so a buffer grows
//! to the largest record it has ever seen and would otherwise stay there for
//! the life of the object. A document holding one outlier record would pin
//! that record's size forever, which matters for a parser kept alive across
//! many documents or parked in a pool.
//!
//! Reclaiming is a reallocation and, for the buffers here, a copy, so it is
//! only worth doing when the excess is large enough to pay for it. Callers
//! therefore hang [`reclaim`] on a path that already allocates or copies —
//! a read refill, a chunk feed, a drain — and never on the per-record path.

#[cfg(all(not(feature = "std"), not(test)))]
use alloc::vec::Vec;

/// Capacity, in elements, below which a buffer is left alone entirely.
///
/// Small buffers are not worth a reallocation to reclaim, and a floor stops
/// a workload whose records straddle the factor from churning on every call.
const FLOOR: usize = 8 * 1024;

/// How far capacity must exceed what is in use before it is given back.
///
/// A buffer that doubles as it grows sits at up to twice what it needs, so
/// reclaiming below that would fight the growth policy on ordinary workloads.
/// Four leaves that headroom untouched and catches only genuine outliers.
const EXCESS_FACTOR: usize = 4;

/// Whether `capacity` is far enough above `keep` to be worth reclaiming.
///
/// `keep` is what the caller wants to remain available without reallocating —
/// the live bytes plus whatever headroom its own growth policy wants. Nothing
/// is reclaimed unless capacity exceeds
/// `max(keep, FLOOR) * EXCESS_FACTOR`, which makes a run of ordinary records
/// free and subsumes a separate floor comparison.
///
/// This is separate from [`reclaim`] because a caller holding an invariant
/// over its buffer — the read window keeps its spare capacity initialized, so
/// reads land where they are parsed — has to re-establish it afterwards, and
/// must not pay for that on the calls where nothing is reclaimed.
#[inline]
pub(crate) fn should_reclaim(capacity: usize, keep: usize) -> bool {
    capacity > keep.max(FLOOR).saturating_mul(EXCESS_FACTOR)
}

/// Give `buffer` back the capacity an outlier record grew, if there is enough
/// of it to be worth a reallocation.
///
/// The test inlines into the caller and the reallocation does not, so a hot
/// path that calls this per chunk pays only a load and two comparisons.
#[inline]
pub(crate) fn reclaim<T>(buffer: &mut Vec<T>, keep: usize) {
    if should_reclaim(buffer.capacity(), keep) {
        shrink(buffer, keep);
    }
}

/// The reallocation itself, kept out of line so it never grows a hot caller.
#[cold]
#[inline(never)]
fn shrink<T>(buffer: &mut Vec<T>, keep: usize) {
    buffer.shrink_to(keep.max(FLOOR));
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use std::iter::repeat_n;

    use super::{EXCESS_FACTOR, FLOOR, reclaim, should_reclaim};

    #[test]
    fn a_small_buffer_is_left_alone() {
        let mut buffer: Vec<u8> = Vec::with_capacity(FLOOR);
        reclaim(&mut buffer, 0);
        assert_eq!(buffer.capacity(), FLOOR);
    }

    #[test]
    fn capacity_within_the_factor_is_left_alone() {
        // Twice the floor is the headroom a doubling growth policy leaves
        // behind on an ordinary workload, and must survive untouched.
        let mut buffer: Vec<u8> = Vec::with_capacity(FLOOR * 2);
        reclaim(&mut buffer, FLOOR);
        assert!(buffer.capacity() >= FLOOR * 2);
    }

    #[test]
    fn capacity_exactly_on_the_excess_boundary_is_left_alone() {
        let keep = FLOOR + 1;
        let threshold = keep * EXCESS_FACTOR;
        let mut buffer: Vec<u8> = Vec::with_capacity(threshold);

        assert!(!should_reclaim(buffer.capacity(), keep));
        reclaim(&mut buffer, keep);
        assert_eq!(buffer.capacity(), threshold);
        assert!(should_reclaim(threshold + 1, keep));
    }

    /// The floor and the factor are a tuning contract, not free parameters:
    /// callers hang [`reclaim`] on their refill paths on the strength of these
    /// numbers. Every other test here is written in terms of the constants, so
    /// none of them can detect a constant moving; pin the values once, in
    /// absolute terms, and check a size that only a mis-set floor would touch.
    #[test]
    fn the_tuning_constants_hold_their_documented_values() {
        assert_eq!(FLOOR, 8 * 1024);
        assert_eq!(EXCESS_FACTOR, 4);

        // Comfortably above any plausible smaller floor, still below the real
        // one, so there is nothing here worth a reallocation.
        assert!(!should_reclaim(5 * 1024, 0));
        assert!(should_reclaim(64 * 1024, 0));
    }

    #[test]
    fn an_outlier_is_reclaimed_down_to_what_is_kept() {
        let mut buffer: Vec<u8> = Vec::with_capacity(FLOOR * EXCESS_FACTOR * 8);
        reclaim(&mut buffer, FLOOR);
        assert_eq!(buffer.capacity(), FLOOR);
    }

    #[test]
    fn the_first_capacity_past_the_boundary_is_reclaimed_to_the_exact_keep() {
        let keep = FLOOR + 1;
        let threshold = keep * EXCESS_FACTOR;
        let mut buffer: Vec<u8> = Vec::with_capacity(threshold + 1);

        reclaim(&mut buffer, keep);

        assert_eq!(buffer.capacity(), keep);
        assert!(!should_reclaim(threshold + 1, keep + 1));
    }

    #[test]
    fn shrinking_below_the_floor_retains_the_exact_floor() {
        let mut buffer: Vec<u8> = Vec::with_capacity(FLOOR * EXCESS_FACTOR * 8);
        reclaim(&mut buffer, 0);
        assert_eq!(buffer.capacity(), FLOOR);
    }

    #[test]
    fn live_bytes_are_never_discarded() {
        let mut buffer: Vec<u8> = Vec::with_capacity(FLOOR * 64);
        buffer.extend(repeat_n(b'x', FLOOR * 3));
        let live = buffer.len();
        reclaim(&mut buffer, live);
        assert_eq!(buffer.len(), FLOOR * 3);
        assert!(buffer.capacity() >= FLOOR * 3);
    }
}
