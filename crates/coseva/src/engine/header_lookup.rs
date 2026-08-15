//! The header-name lookup and the pieces that keep it cheap to build.
//!
//! The map is built lazily, because the paths that read it — looking a column
//! up by name — are the exception. Typed decode and the Serde path both
//! resolve their columns by scanning the header record directly, so for the
//! common case of parsing a file into a struct the map is never built at all.
//!
//! When it is built, the two costs that matter are the hash and the per-entry
//! allocations. `HeaderHasher` addresses the first and `HeaderSlots` the
//! second.
//!
//! The keys are the names' hashes rather than the names, so building the map
//! copies nothing. Storing the names would mean copying every one onto the
//! heap, because the map outlives the window its header record was parsed from
//! and so cannot borrow into it — but the engine owns that record for exactly
//! as long as it owns the map, so the map can key on a hash and compare
//! against the record when it is asked. That is worth 19% of the cost of
//! building a six-column lookup and 27% of a two-hundred-column one.
//!
//! Two different names hashing alike would otherwise silently resolve to each
//! other's columns, so a name is only accepted from the map once its bytes have
//! been checked against the record. A name that loses its hash to a different
//! earlier one is filed one key along instead, and both building and reading
//! walk that chain until the bytes match or a key is free. No header row
//! without a 64-bit collision ever takes a second step.

#[cfg(not(feature = "std"))]
use alloc::collections::BTreeMap;
use alloc::vec;
#[cfg(all(not(feature = "std"), not(test)))]
use alloc::vec::Vec;
#[cfg(feature = "std")]
use core::hash::BuildHasherDefault;
use core::hash::Hasher;
use core::slice;
#[cfg(feature = "std")]
use std::collections::HashMap;

use crate::ByteRecord;

#[cfg(not(feature = "std"))]
type Table = BTreeMap<u64, HeaderSlots>;

#[cfg(feature = "std")]
type Table = HashMap<u64, HeaderSlots, HeaderHashBuilder>;

/// Maps a header name to the columns carrying it.
///
/// Keyed by the hash of the name rather than the name, so building it copies
/// nothing; see the module docs for why that is sound and what it is worth.
#[derive(Debug, Default)]
pub(super) struct HeaderLookup(Table);

impl HeaderLookup {
    /// Forget every entry, keeping the allocation for the next header row.
    pub(super) fn clear(&mut self) {
        self.0.clear();
    }

    /// Build the lookup over `headers`, replacing whatever it held.
    pub(super) fn rebuild(&mut self, headers: &ByteRecord) {
        self.clear();
        for (index, name) in headers.iter().enumerate() {
            self.insert_at(headers, hash_name(name), name, index);
        }
    }

    /// File `index` under `name`, starting the collision walk at `key`.
    fn insert_at(&mut self, headers: &ByteRecord, key: u64, name: &[u8], index: usize) {
        let mut key = key;
        // Walk to this name's own key: the first that is either free or
        // already holds it. Anything else on the way is a collision.
        loop {
            match self.0.get_mut(&key) {
                Some(slots) if headers.get(slots.first()) == Some(name) => {
                    slots.push(index);
                    return;
                }
                Some(_) => {
                    // gamma::skip(stmt.delete_assign, reason = "a colliding key would stop advancing and loop forever")
                    // gamma::skip(assign_value.default, reason = "a colliding key would stop advancing and loop forever")
                    // gamma::skip(literal.int_decrement, reason = "a zero collision step would loop forever")
                    key = key.wrapping_add(1);
                }
                None => {
                    self.0.insert(key, HeaderSlots::One(index));
                    return;
                }
            }
        }
    }

    /// The columns named `name`, or `None` if no column carries it.
    ///
    /// `headers` must be the record the lookup was built over.
    pub(super) fn get(&self, headers: &ByteRecord, name: &[u8]) -> Option<&HeaderSlots> {
        self.get_at(headers, hash_name(name), name)
    }

    /// Find `name`, starting the collision walk at `key`.
    fn get_at(&self, headers: &ByteRecord, key: u64, name: &[u8]) -> Option<&HeaderSlots> {
        let mut key = key;
        loop {
            let slots = self.0.get(&key)?;
            if headers.get(slots.first()) == Some(name) {
                return Some(slots);
            }
            // gamma::skip(stmt.delete_assign, reason = "a colliding key would stop advancing and loop forever")
            // gamma::skip(assign_value.default, reason = "a colliding key would reset and loop forever")
            // gamma::skip(literal.int_decrement, reason = "a zero collision step would loop forever")
            key = key.wrapping_add(1);
        }
    }
}

/// The key a name is filed under, before any collision walk.
///
/// Exposed to the crate so the typed-mapping and projection paths can build a
/// temporary lookup with the same hashing rather than rolling their own.
pub(crate) fn hash_name(name: &[u8]) -> u64 {
    let mut hasher = HeaderHasher(0);
    hasher.write(name);
    hasher.0
}

/// The column indices sharing one header name.
///
/// A duplicate header name is legal and the lookup has to report every column
/// carrying it, which is why this is a sequence rather than an index. It is
/// also rare, so the single-column case is stored inline and only a genuine
/// duplicate allocates. That is one allocation saved per column of every
/// header ever looked up.
#[derive(Clone, Debug)]
pub(super) enum HeaderSlots {
    /// The name is unique, and this is its column.
    One(usize),
    /// The name repeats. Never empty, and always in ascending column order.
    Many(Vec<usize>),
}

impl HeaderSlots {
    /// Record another column carrying this name.
    pub(super) fn push(&mut self, index: usize) {
        match self {
            Self::One(first) => *self = Self::Many(vec![*first, index]),
            Self::Many(indices) => indices.push(index),
        }
    }

    /// Every column carrying this name, in ascending order.
    pub(super) fn as_slice(&self) -> &[usize] {
        match self {
            Self::One(index) => slice::from_ref(index),
            Self::Many(indices) => indices,
        }
    }

    /// The first column carrying this name.
    pub(super) fn first(&self) -> usize {
        match self {
            Self::One(index) => *index,
            // `Many` is only ever built with two elements and only grows.
            Self::Many(indices) => indices[0],
        }
    }
}

/// The hasher the header lookup is built with.
///
/// The default `RandomState` is SipHash-1-3, chosen to make collision attacks
/// against a long-lived, attacker-fed map impractical. Neither half of that
/// applies here: the keys are derived from the header names of the file being
/// parsed, the map lives and dies with one parser, and a caller who controls
/// the header row controls the whole input anyway. This hasher roughly halves
/// the cost of building a wide-header lookup.
///
/// The mix is the familiar rotate-xor-multiply used by `rustc` itself. It is
/// not a strong hash and is not used as one.
///
/// The table's keys are already the output of that mix, so hashing one again
/// would only spend a multiply to move bits that are distributed already.
/// `write_u64` therefore passes the key straight through, and the name itself
/// is hashed through `write`.
#[cfg(feature = "std")]
type HeaderHashBuilder = BuildHasherDefault<HeaderHasher>;

/// Multiplier for the mix. An odd constant with a well-distributed bit pattern,
/// which is all the mix requires of it.
const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

#[derive(Clone, Copy, Debug, Default)]
struct HeaderHasher(u64);

impl HeaderHasher {
    /// Fold one machine word into the state.
    fn mix(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for HeaderHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        while let Some((chunk, tail)) = rest.split_first_chunk::<8>() {
            self.mix(u64::from_le_bytes(*chunk));
            // gamma::skip(stmt.delete_assign, reason = "an eight-byte chunk would be processed forever")
            rest = tail;
        }
        // The tail is folded in two more fixed-width steps rather than
        // byte-by-byte, so a name of any length costs at most three mixes after
        // its whole words.
        if let Some((chunk, tail)) = rest.split_first_chunk::<4>() {
            self.mix(u64::from(u32::from_le_bytes(*chunk)));
            rest = tail;
        }
        let mut remainder = 0_u64;
        for &byte in rest {
            remainder = (remainder << 8) | u64::from(byte);
        }
        // Unconditional, so that trailing bytes cannot alias a shorter name
        // whose whole words are identical.
        self.mix(remainder ^ bytes.len() as u64);
    }

    /// A key is already a mixed hash, so it is taken as the state unchanged.
    fn write_u64(&mut self, i: u64) {
        self.0 = i;
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod header_lookup_tests {
    #[cfg(feature = "std")]
    use core::hash::BuildHasher;
    use core::hash::Hasher;

    #[cfg(feature = "std")]
    use super::HeaderHashBuilder;
    use super::{HeaderHasher, HeaderLookup, HeaderSlots, hash_name};
    use crate::ByteRecord;

    /// Every name filed under one key, which is what a 64-bit collision between
    /// all of them would produce. A real one cannot be constructed, so the walk
    /// is driven through the seam the hashing normally supplies.
    const COLLIDING: u64 = 0;

    fn record(names: &[&str]) -> ByteRecord {
        names.iter().map(|name| name.as_bytes()).collect()
    }

    #[test]
    fn colliding_names_keep_their_own_columns() {
        let headers = record(&["alpha", "beta", "gamma"]);
        let mut lookup = HeaderLookup::default();
        for (index, name) in headers.iter().enumerate() {
            lookup.insert_at(&headers, COLLIDING, name, index);
        }

        for (index, name) in headers.iter().enumerate() {
            let slots = lookup
                .get_at(&headers, COLLIDING, name)
                .expect("a name that was inserted must be found");
            assert_eq!(slots.as_slice(), &[index]);
        }
    }

    #[test]
    fn a_duplicate_joins_its_own_name_rather_than_the_one_it_collided_with() {
        let headers = record(&["alpha", "beta", "alpha"]);
        let mut lookup = HeaderLookup::default();
        for (index, name) in headers.iter().enumerate() {
            lookup.insert_at(&headers, COLLIDING, name, index);
        }

        assert_eq!(
            lookup
                .get_at(&headers, COLLIDING, b"alpha")
                .map(HeaderSlots::as_slice),
            Some(&[0, 2][..])
        );
        assert_eq!(
            lookup
                .get_at(&headers, COLLIDING, b"beta")
                .map(HeaderSlots::as_slice),
            Some(&[1][..])
        );
    }

    /// A miss must end the walk at the first free key rather than run on.
    #[test]
    fn an_absent_name_is_not_found_through_a_chain_of_collisions() {
        let headers = record(&["alpha", "beta"]);
        let mut lookup = HeaderLookup::default();
        for (index, name) in headers.iter().enumerate() {
            lookup.insert_at(&headers, COLLIDING, name, index);
        }

        assert!(lookup.get_at(&headers, COLLIDING, b"gamma").is_none());
    }

    /// The ordinary path, with the hashing supplying the keys.
    #[test]
    fn names_resolve_to_their_columns() {
        let headers = record(&["alpha", "beta", "alpha"]);
        let mut lookup = HeaderLookup::default();
        lookup.rebuild(&headers);

        assert_eq!(
            lookup.get(&headers, b"alpha").map(HeaderSlots::as_slice),
            Some(&[0, 2][..])
        );
        assert_eq!(
            lookup.get(&headers, b"beta").map(HeaderSlots::as_slice),
            Some(&[1][..])
        );
        assert!(lookup.get(&headers, b"gamma").is_none());
    }

    #[test]
    fn clear_and_rebuild_remove_every_stale_name() {
        let first = record(&["alpha", "beta"]);
        let second = record(&["gamma"]);
        let mut lookup = HeaderLookup::default();

        lookup.rebuild(&first);
        lookup.clear();
        assert!(lookup.0.is_empty());
        assert!(lookup.get(&first, b"alpha").is_none());

        lookup.rebuild(&first);
        lookup.rebuild(&second);
        assert_eq!(lookup.0.len(), 1);
        assert!(lookup.get(&second, b"alpha").is_none());
        assert_eq!(
            lookup.get(&second, b"gamma").map(HeaderSlots::as_slice),
            Some(&[0][..])
        );
    }

    #[test]
    fn header_hash_covers_every_chunk_and_tail_width() {
        let cases = [
            (&b""[..], 0x0000_0000_0000_0000),
            (&b"a"[..], 0x8ec8_a4ae_acc3_f7e0),
            (&b"abc"[..], 0xb756_958f_b746_01e0),
            (&b"abcde"[..], 0x8c33_229a_137f_aaea),
            (&b"abcdefgh"[..], 0xd434_aa35_efa3_16c4),
            (&b"abcdefghi"[..], 0x8fb2_eed4_0cf4_be0c),
            (&b"abcdefghijkl"[..], 0xde13_704a_bf63_1b57),
            (&b"abcdefghijklm"[..], 0xc7b8_a053_071c_43a3),
            (&b"abcdefghijklmnopq"[..], 0x375f_a916_6a94_194f),
        ];

        for (name, expected) in cases {
            assert_eq!(hash_name(name), expected, "wrong hash for {name:?}");
        }
    }

    #[test]
    fn prehashed_table_keys_replace_the_hasher_state() {
        let mut hasher = HeaderHasher(17);
        hasher.write_u64(0xfeed_face_cafe_beef);
        assert_eq!(hasher.finish(), 0xfeed_face_cafe_beef);
    }

    #[cfg(feature = "std")]
    #[test]
    fn table_hashers_start_from_the_documented_zero_state() {
        assert_eq!(HeaderHashBuilder::default().build_hasher().finish(), 0);
    }
}
