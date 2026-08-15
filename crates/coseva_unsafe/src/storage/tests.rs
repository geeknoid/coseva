use core::hash::{Hash, Hasher};

use super::{
    MAX_FIELD_OFFSET, RecordStorage, Utf8RecordError, Utf8RecordStorage, end_is_null, end_offset,
};

#[test]
fn record_storage_comprehensive() {
    let mut storage = RecordStorage::with_capacity(10, 100);
    assert_eq!(storage.len(), 0);
    assert!(storage.is_empty());
    assert_eq!(storage.bytes_len(), 0);
    assert!(storage.byte_capacity() >= 100);
    assert!(storage.field_capacity() >= 10);
    assert!(!storage.is_unallocated());
    assert!(!storage.null_aware());

    storage.set_null_aware(true);
    assert!(storage.null_aware());

    storage.set_location(10..20, 5);
    assert_eq!(storage.byte_range(), 10..20);
    assert_eq!(storage.index(), 5);

    storage.invalidate_source_metadata();
    assert_eq!(storage.byte_range(), 0..0);
    assert_eq!(storage.index(), 0);

    storage.push_field(b"foo", MAX_FIELD_OFFSET);
    storage.push_field(b"bar", MAX_FIELD_OFFSET);
    storage.push_null(MAX_FIELD_OFFSET);
    assert_eq!(storage.len(), 3);
    assert_eq!(storage.get(0), Some(b"foo".as_slice()));
    assert_eq!(storage.get(1), Some(b"bar".as_slice()));
    assert_eq!(storage.get(2), Some(b"".as_slice()));
    assert_eq!(storage.is_null(2), Some(true));
    assert_eq!(storage.range(0), Some(0..3));

    // Hash
    use core::hash::Hash;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    storage.hash(&mut hasher);

    // unequal length set_field (shorter, longer, out of bounds)
    assert!(storage.set_field(0, b"short", MAX_FIELD_OFFSET));
    assert_eq!(storage.get(0), Some(b"short".as_slice()));
    assert!(storage.set_field(0, b"a_very_long_field_replacement", MAX_FIELD_OFFSET));
    assert_eq!(
        storage.get(0),
        Some(b"a_very_long_field_replacement".as_slice())
    );
    assert!(!storage.set_field(100, b"out_of_bounds", MAX_FIELD_OFFSET));

    // equal length set_field
    assert!(storage.set_field(0, b"baz", MAX_FIELD_OFFSET));
    assert_eq!(storage.get(0), Some(b"baz".as_slice()));
    assert_eq!(storage.range(1), Some(3..6));
    assert_eq!(storage.is_null(1), Some(false));
    assert_eq!(storage.get(1), Some(b"bar".as_slice()));
    assert_eq!(storage.try_set_field_equal(1, b"bar"), Some(true));
    assert!(storage.set_field(1, b"quux", MAX_FIELD_OFFSET));
    assert_eq!(storage.get(1), Some(b"quux".as_slice()));
    assert_eq!(storage.try_set_field_equal(0, b"qux"), Some(true));
    assert_eq!(storage.try_set_field_equal(0, b"longer"), Some(false));

    // set_null
    assert!(storage.set_null(0, MAX_FIELD_OFFSET));
    assert_eq!(storage.is_null(0), Some(true));

    // truncate
    storage.truncate(100); // len >= self.len() no-op
    storage.truncate(2);
    assert_eq!(storage.len(), 2);

    // reclaim with excess capacity
    storage.reserve(20_000, 100_000);
    storage.reclaim();

    // shrink_to_fit
    storage.shrink_to_fit();

    // clone_from
    let mut other = RecordStorage::new();
    other.clone_from(&storage);
    assert_eq!(storage, other);

    // Additional helper methods
    let mut helpers = RecordStorage::new();
    helpers.reserve_storage(10, 100);
    helpers.extend_bytes(b"hello");
    helpers.append_field(b"world");
    helpers.append_short_field(b"hi");
    helpers.append_empty_fields(2);
    helpers.append_null_field();
    helpers.push_byte(b'x');
    helpers.finish_field();
    assert_eq!(helpers.as_slice(), helpers.bytes());
    assert_eq!(helpers.iter().count(), helpers.len());
    assert_eq!(helpers.ends().len(), helpers.len());

    let mut markable = RecordStorage::new();
    markable.append_field(b"hi");
    markable.append_field(b"world");
    markable.mark_null_fields(|f| f == b"hi");
    assert_eq!(markable.is_null(0), Some(true));
    assert_eq!(markable.is_null(1), Some(false));

    let mut trimmable = RecordStorage::new();
    trimmable.append_field(b"  trimmed  ");
    trimmable.append_field(b"   ");
    trimmable.append_field(b"leading_only  ");
    trimmable.append_field(b"  trailing_only");
    trimmable.mark_null_fields(|b| b == b"   ");
    trimmable.trim_fields_ascii();
    assert_eq!(trimmable.get(0), Some(b"trimmed".as_slice()));
    assert_eq!(trimmable.is_null(1), Some(true));

    let mut empty_trim = RecordStorage::new();
    empty_trim.trim_fields_ascii();
    assert_eq!(empty_trim.len(), 0);

    helpers.clear_fields();
    assert_eq!(helpers.len(), 0);
    helpers.truncate_storage(0, 0);
    assert_eq!(helpers.bytes_len(), 0);
}

#[test]
#[expect(
    clippy::assertions_on_result_states,
    reason = "verifying conversion failure"
)]
fn utf8_record_storage_comprehensive() {
    let default_utf8 = Utf8RecordStorage::new();
    assert_eq!(default_utf8.len(), 0);

    let mut storage = Utf8RecordStorage::with_capacity(5, 50);
    assert_eq!(storage.len(), 0);
    assert!(storage.is_empty());
    assert!(storage.byte_capacity() >= 50);
    assert!(storage.field_capacity() >= 5);
    assert!(!storage.null_aware());

    storage.set_null_aware(true);
    assert!(storage.null_aware());

    storage.set_location(5..15, 2);
    assert_eq!(storage.byte_range(), 5..15);
    assert_eq!(storage.index(), 2);

    storage.push_field("hello", MAX_FIELD_OFFSET);
    storage.push_field("world", MAX_FIELD_OFFSET);
    storage.push_null(MAX_FIELD_OFFSET);

    assert_eq!(storage.get(0), Some("hello"));
    assert_eq!(storage.get(1), Some("world"));
    assert_eq!(storage.get(100), None);
    assert_eq!(storage.range(100), None);
    assert_eq!(storage.is_null(100), None);
    assert_eq!(storage.as_str(), "helloworld");

    // Split UTF-8 code point across field boundaries
    let mut split_raw = RecordStorage::new();
    split_raw.append_field(&[0xF0, 0x9F]);
    split_raw.append_field(&[0x98, 0x80]);
    let split_res = Utf8RecordStorage::try_from_storage(split_raw);
    assert!(split_res.is_err());
    assert_eq!(storage.bytes(), b"helloworld");
    assert_eq!(storage.ends().len(), 3);
    assert_eq!(storage.range(0), Some(0..5));
    assert_eq!(storage.is_null(2), Some(true));

    assert!(storage.set_field(0, "there", MAX_FIELD_OFFSET));
    assert_eq!(storage.try_set_field_equal(0, "where"), Some(true));
    assert!(storage.set_null(0, MAX_FIELD_OFFSET));

    storage.reserve(10, 50);
    storage.shrink_to_fit();
    storage.truncate(1);
    assert_eq!(storage.len(), 1);

    storage.clear();
    assert_eq!(storage.len(), 0);

    let mut raw = RecordStorage::new();
    raw.push_field(b"valid", MAX_FIELD_OFFSET);
    let mut utf8_storage = Utf8RecordStorage::try_from_storage(raw).expect("valid UTF-8");

    let mut raw_non_ascii = RecordStorage::new();
    raw_non_ascii.push_field("🦀".as_bytes(), MAX_FIELD_OFFSET);
    let _ = Utf8RecordStorage::try_from_storage(raw_non_ascii).expect("valid UTF-8");

    let mut raw_invalid_boundary = RecordStorage::new();
    raw_invalid_boundary.push_field(&"🦀".as_bytes()[..2], MAX_FIELD_OFFSET);
    raw_invalid_boundary.push_field(&"🦀".as_bytes()[2..], MAX_FIELD_OFFSET);
    assert!(Utf8RecordStorage::try_from_storage(raw_invalid_boundary).is_err());

    let mut raw_invalid_utf8 = RecordStorage::new();
    raw_invalid_utf8.push_field(&[0xff, 0xfe], MAX_FIELD_OFFSET);
    assert!(Utf8RecordStorage::try_from_storage(raw_invalid_utf8).is_err());

    // invalid utf8 in a field
    let mut raw_trailing_invalid = RecordStorage::new();
    raw_trailing_invalid.push_field(b"ab\xff", MAX_FIELD_OFFSET);
    assert!(Utf8RecordStorage::try_from_storage(raw_trailing_invalid).is_err());

    let _ = utf8_storage.refill_with(|s| {
        s.push_field(b"ascii", MAX_FIELD_OFFSET);
        Ok::<(), ()>(())
    });
    let _ = utf8_storage.refill_with(|s| {
        s.push_field("🦀".as_bytes(), MAX_FIELD_OFFSET);
        Ok::<(), ()>(())
    });
    let _ = utf8_storage.refill_with(|_s| Err::<(), &'static str>("fail"));
    let _ = utf8_storage.refill_with(|s| {
        s.push_field(&[0xff, 0xfe], MAX_FIELD_OFFSET);
        Ok::<(), ()>(())
    });
    assert!(!utf8_storage.set_field(100, "out_of_bounds", MAX_FIELD_OFFSET));
    assert!(!utf8_storage.set_null(100, MAX_FIELD_OFFSET));
    assert_eq!(utf8_storage.try_set_field_equal(100, "out_of_bounds"), None);
    assert_eq!(utf8_storage.index(), 0);
    assert!(utf8_storage.byte_capacity() >= utf8_storage.bytes().len());
    assert!(utf8_storage.field_capacity() >= utf8_storage.len());
    let _ = utf8_storage.into_storage();
}

fn hash_of(storage: &RecordStorage) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    storage.hash(&mut hasher);
    hasher.finish()
}

#[test]
fn record_storage_hash_and_default_metadata_cover_both_components() {
    let storage = RecordStorage::new();
    assert_eq!(storage.byte_range(), 0..0);
    assert_eq!(storage.index(), 0);
    assert_eq!(storage.len(), 0);

    let mut one_field = RecordStorage::new();
    one_field.append_field(b"ab");
    let mut two_fields = RecordStorage::new();
    two_fields.append_field(b"a");
    two_fields.append_field(b"b");
    assert_eq!(one_field.bytes(), two_fields.bytes());
    assert_ne!(one_field.ends(), two_fields.ends());
    assert_ne!(hash_of(&one_field), hash_of(&two_fields));

    let mut different_bytes = RecordStorage::new();
    different_bytes.append_field(b"cd");
    assert_eq!(one_field.ends(), different_bytes.ends());
    assert_ne!(hash_of(&one_field), hash_of(&different_bytes));
}

#[test]
fn record_storage_offsets_and_metadata_remain_aligned_after_edits() {
    let mut storage = RecordStorage::new();
    storage.push_field(b"aa", 5);
    storage.push_field(b"b", 5);
    storage.push_null(5);
    storage.set_location(10..13, 7);

    assert!(storage.set_field(0, b"xxxx", 5));
    assert_eq!(storage.bytes(), b"xxxxb");
    assert_eq!(
        storage
            .ends()
            .iter()
            .map(|&raw| end_offset(raw))
            .collect::<alloc::vec::Vec<_>>(),
        [4, 5, 5]
    );
    assert!(!end_is_null(storage.ends()[0]));
    assert!(end_is_null(storage.ends()[2]));

    assert!(storage.set_field(0, b"z", 2));
    assert_eq!(storage.bytes(), b"zb");
    assert_eq!(storage.range(0), Some(0..1));
    assert_eq!(storage.range(1), Some(1..2));
    assert_eq!(storage.range(2), Some(2..2));
    assert!(storage.is_null(2).expect("third field exists"));

    storage.truncate(2);
    assert_eq!(storage.bytes(), b"zb");
    assert_eq!(storage.ends(), &[1, 2]);
    storage.clear();
    assert_eq!(storage.byte_range(), 0..0);
    assert_eq!(storage.index(), 0);
    assert!(!storage.null_aware());
}

#[test]
fn record_storage_equal_length_updates_copy_and_clear_null_flags() {
    let mut storage = RecordStorage::new();
    storage.append_field(b"abc");
    storage.append_null_field();
    assert!(storage.set_field(0, b"xyz", MAX_FIELD_OFFSET));
    assert_eq!(storage.get(0), Some(b"xyz".as_slice()));
    assert_eq!(storage.try_set_field_equal(0, b"xyz"), Some(true));
    assert_eq!(storage.get(0), Some(b"xyz".as_slice()));
    assert_eq!(storage.try_set_field_equal(0, b"xy"), Some(false));
    assert_eq!(storage.try_set_field_equal(9, b""), None);
    assert!(storage.set_field(1, b"", MAX_FIELD_OFFSET));
    assert_eq!(storage.is_null(1), Some(false));
}

#[test]
fn record_storage_null_and_truncation_boundaries_are_exact() {
    let mut storage = RecordStorage::new();
    storage.append_field(b"abc");
    storage.append_field(b"de");
    storage.append_field(b"f");

    storage.truncate_storage(2, 5);
    assert_eq!(storage.bytes(), b"abcde");
    assert_eq!(storage.ends(), &[3, 5]);
    storage.truncate(2);
    assert_eq!(storage.bytes(), b"abcde");
    storage.truncate(1);
    assert_eq!(storage.bytes(), b"abc");
    assert_eq!(storage.ends(), &[3]);

    assert!(storage.set_null(0, MAX_FIELD_OFFSET));
    assert!(storage.null_aware());
    assert_eq!(storage.get(0), Some(b"".as_slice()));
    assert_eq!(storage.is_null(0), Some(true));
    assert!(!storage.set_null(1, MAX_FIELD_OFFSET));

    storage.clear_fields();
    storage.push_null(0);
    assert!(storage.null_aware());
    assert_eq!(storage.ends().len(), 1);
    assert!(end_is_null(storage.ends()[0]));
}

#[test]
fn record_storage_rejects_offset_overflow_before_mutating() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let mut push = RecordStorage::new();
    push.push_field(b"a", 1);
    assert!(catch_unwind(AssertUnwindSafe(|| push.push_field(b"b", 1))).is_err());
    assert_eq!(push.bytes(), b"a");
    assert_eq!(push.ends(), &[1]);

    let mut set = RecordStorage::new();
    set.append_field(b"aa");
    set.append_field(b"b");
    let before = set.clone();
    assert!(catch_unwind(AssertUnwindSafe(|| set.set_field(0, b"xxxx", 4))).is_err());
    assert_eq!(set, before);
    assert!(set.set_field(0, b"xxx", 4));
    assert_eq!(set.bytes(), b"xxxb");

    let mut null = RecordStorage::new();
    null.append_field(b"a");
    assert!(catch_unwind(AssertUnwindSafe(|| null.push_null(0))).is_err());
    assert_eq!(null.len(), 1);
}

#[test]
fn record_storage_mark_and_trim_keep_endpoints_and_nulls_consistent() {
    let mut storage = RecordStorage::new();
    for field in [
        b"  first  ".as_slice(),
        b"NULL",
        b" middle ",
        b"   ",
        b"last",
    ] {
        storage.append_field(field);
    }
    storage.mark_null_fields(|field| field == b"NULL" || field == b"   ");
    assert!(storage.null_aware());
    assert_eq!(
        storage.iter().collect::<alloc::vec::Vec<_>>(),
        [b"  first  ".as_slice(), b"", b" middle ", b"", b"last"]
    );
    assert_eq!(
        (0..storage.len())
            .map(|index| storage.is_null(index).expect("index is in range"))
            .collect::<alloc::vec::Vec<_>>(),
        [false, true, false, true, false]
    );

    storage.trim_fields_ascii();
    assert_eq!(storage.bytes(), b"firstmiddlelast");
    assert_eq!(
        storage.iter().collect::<alloc::vec::Vec<_>>(),
        [b"first".as_slice(), b"", b"middle", b"", b"last"]
    );
    assert_eq!(
        storage
            .ends()
            .iter()
            .map(|&raw| end_offset(raw))
            .collect::<alloc::vec::Vec<_>>(),
        [5, 5, 11, 11, 15]
    );
    assert!(end_is_null(storage.ends()[1]));
    assert!(end_is_null(storage.ends()[3]));
    assert!(!end_is_null(storage.ends()[0]));
    assert!(!end_is_null(storage.ends()[2]));
    assert!(!end_is_null(storage.ends()[4]));
}

#[test]
fn record_storage_append_helpers_preserve_lengths_flags_and_bytes() {
    let mut storage = RecordStorage::new();
    storage.extend_bytes(b"raw");
    assert_eq!(storage.bytes(), b"raw");

    storage.clear_fields();
    storage.append_short_field(b"abc");
    storage.append_short_field(b"wxyz");
    storage.append_empty_fields(2);
    storage.append_null_field();
    storage.push_byte(b'!');
    storage.finish_field();

    assert_eq!(storage.bytes(), b"abcwxyz!");
    assert_eq!(
        storage.iter().collect::<alloc::vec::Vec<_>>(),
        [b"abc".as_slice(), b"wxyz", b"", b"", b"", b"!"]
    );
    assert_eq!(
        storage
            .ends()
            .iter()
            .map(|&raw| end_offset(raw))
            .collect::<alloc::vec::Vec<_>>(),
        [3, 7, 7, 7, 7, 8]
    );
    assert_eq!(storage.is_null(0), Some(false));
    assert_eq!(storage.is_null(1), Some(false));
    assert!(storage.is_null(4).expect("NULL field exists"));
    assert!(!storage.is_null(5).expect("final field exists"));
}

#[test]
fn clone_from_copies_null_and_source_metadata() {
    let mut source = RecordStorage::new();
    source.append_null_field();
    source.set_location(4..9, 12);

    let mut destination = RecordStorage::new();
    destination.append_field(b"stale");
    destination.clone_from(&source);
    assert_eq!(destination, source);
    assert!(destination.null_aware());
    assert_eq!(destination.byte_range(), 4..9);
    assert_eq!(destination.index(), 12);
}

#[test]
fn utf8_validation_reports_the_exact_malformed_field_and_rolls_back() {
    let mut malformed = RecordStorage::new();
    malformed.append_field(b"ok");
    malformed.append_field(&[0xff]);
    malformed.append_field(b"later");
    let error = Utf8RecordStorage::try_from_storage(malformed)
        .expect_err("the second field is malformed")
        .1;
    assert!(matches!(
        error,
        Utf8RecordError::InvalidField { index: 1, .. }
    ));

    let crab = "🦀".as_bytes();
    let mut split = RecordStorage::new();
    split.append_field(&crab[..2]);
    split.append_field(&crab[2..]);
    let error = Utf8RecordStorage::try_from_storage(split)
        .expect_err("a field endpoint splits a code point")
        .1;
    assert!(matches!(
        error,
        Utf8RecordError::InvalidField { index: 0, .. }
    ));

    let mut storage = Utf8RecordStorage::new();
    storage.push_field("old", MAX_FIELD_OFFSET);
    let application_error = storage
        .refill_with(|raw| {
            raw.append_field(b"temporary");
            Err::<(), _>("stop")
        })
        .expect("application errors are nested");
    assert_eq!(application_error, Err("stop"));
    assert!(storage.is_empty());
    assert_eq!(storage.as_str(), "");

    let validation_error = storage.refill_with(|raw| {
        raw.append_field(&[0xff]);
        Ok::<(), ()>(())
    });
    assert!(matches!(
        validation_error,
        Err(Utf8RecordError::InvalidField { index: 0, .. })
    ));
    assert!(storage.is_empty());
}

#[test]
fn utf8_wrapper_forwards_capacity_and_offset_boundaries() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let mut storage = Utf8RecordStorage::with_capacity(64, 256);
    let old_fields = storage.field_capacity();
    let old_bytes = storage.byte_capacity();
    storage.push_field("a", 1);
    storage.shrink_to_fit();
    assert!(storage.field_capacity() < old_fields);
    assert!(storage.byte_capacity() < old_bytes);

    storage.clear();
    storage.reserve(17, 19);
    assert!(storage.field_capacity() >= 17);
    assert!(storage.byte_capacity() >= 19);

    storage.push_field("a", 1);
    assert!(catch_unwind(AssertUnwindSafe(|| storage.push_field("b", 1))).is_err());
    assert_eq!(storage.get(0), Some("a"));

    storage.clear();
    storage.push_field("aa", 4);
    storage.push_field("b", 4);
    assert!(storage.set_field(0, "xxx", 4));
    assert_eq!(storage.get(0), Some("xxx"));
    assert_eq!(storage.get(1), Some("b"));
    assert_eq!(storage.try_set_field_equal(0, "yyy"), Some(true));
    assert_eq!(storage.get(0), Some("yyy"));
    assert!(storage.set_null(1, 4));
    assert_eq!(storage.is_null(1), Some(true));
    storage.truncate(1);
    assert_eq!(storage.len(), 1);
}

#[test]
fn null_setters_forward_exact_offset_limits() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let mut exact = RecordStorage::new();
    exact.append_field(b"a");
    exact.append_field(b"b");
    assert!(exact.set_null(0, 1));
    assert_eq!(exact.bytes(), b"b");
    assert_eq!(exact.is_null(0), Some(true));

    let mut too_small = RecordStorage::new();
    too_small.append_field(b"a");
    too_small.append_field(b"b");
    assert!(catch_unwind(AssertUnwindSafe(|| too_small.set_null(0, 0))).is_err());
    assert_eq!(too_small.bytes(), b"ab");

    let mut utf8_exact = Utf8RecordStorage::new();
    utf8_exact.push_field("a", 1);
    utf8_exact.push_null(1);
    assert!(utf8_exact.is_null(1).expect("NULL field exists"));

    let mut utf8_push_too_small = Utf8RecordStorage::new();
    utf8_push_too_small.push_field("a", 1);
    assert!(catch_unwind(AssertUnwindSafe(|| utf8_push_too_small.push_null(0))).is_err());

    let mut utf8_set = Utf8RecordStorage::new();
    utf8_set.push_field("a", 2);
    utf8_set.push_field("b", 2);
    assert!(utf8_set.set_field(0, "x", 2));
    assert!(utf8_set.set_null(0, 1));
    assert_eq!(utf8_set.get(1), Some("b"));

    let mut utf8_field_too_small = Utf8RecordStorage::new();
    utf8_field_too_small.push_field("a", 2);
    utf8_field_too_small.push_field("b", 2);
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            utf8_field_too_small.set_field(0, "xx", 2)
        }))
        .is_err()
    );

    let mut utf8_set_too_small = Utf8RecordStorage::new();
    utf8_set_too_small.push_field("a", 2);
    utf8_set_too_small.push_field("b", 2);
    assert!(catch_unwind(AssertUnwindSafe(|| utf8_set_too_small.set_null(0, 0))).is_err());
}

#[test]
fn equality_truncation_and_null_clearing_are_exact() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let mut one_field = RecordStorage::new();
    one_field.append_field(b"ab");
    let mut two_fields = RecordStorage::new();
    two_fields.append_field(b"a");
    two_fields.append_field(b"b");
    assert_ne!(one_field, two_fields);

    let mut different_bytes = RecordStorage::new();
    different_bytes.append_field(b"cd");
    assert_ne!(one_field, different_bytes);

    let mut invalid_cut = RecordStorage::new();
    invalid_cut.append_field(b"abc");
    invalid_cut.append_field(b"de");
    invalid_cut.append_field(b"f");
    assert!(catch_unwind(AssertUnwindSafe(|| invalid_cut.truncate_storage(2, 4))).is_err());

    let mut null = RecordStorage::new();
    null.append_null_field();
    assert!(null.set_field(0, b"x", MAX_FIELD_OFFSET));
    assert_eq!(null.get(0), Some(b"x".as_slice()));
    assert_eq!(null.is_null(0), Some(false));

    let mut equal_null = RecordStorage::new();
    equal_null.append_null_field();
    assert_eq!(equal_null.try_set_field_equal(0, b""), Some(true));
    assert_eq!(equal_null.is_null(0), Some(false));

    one_field.truncate(0);
    assert!(one_field.is_empty());
    assert!(one_field.bytes().is_empty());
}

#[test]
fn append_mark_and_trim_mutations_change_observable_storage() {
    let mut appended = RecordStorage::new();
    appended.append_field(b"a");
    appended.append_empty_fields(3);
    assert_eq!(appended.len(), 4);
    assert!((0..appended.len()).all(|index| appended.is_null(index) == Some(false)));

    let mut marked = RecordStorage::new();
    for field in [b"A".as_slice(), b"NULL", b"NULL", b"B"] {
        marked.append_field(field);
    }
    marked.mark_null_fields(|field| field == b"NULL");
    assert_eq!(marked.bytes(), b"AB");
    assert_eq!(
        marked.iter().collect::<alloc::vec::Vec<_>>(),
        [b"A".as_slice(), b"", b"", b"B"]
    );

    let mut whitespace = RecordStorage::new();
    whitespace.append_field(b"plain");
    whitespace.append_field(b" ");
    whitespace.trim_fields_ascii();
    assert_eq!(whitespace.bytes(), b"plain");
    assert_eq!(whitespace.ends(), &[5, 5]);
}

#[test]
fn utf8_success_retains_data_and_malformed_trailing_bytes_are_reported() {
    let mut storage = Utf8RecordStorage::new();
    storage.push_field("old", MAX_FIELD_OFFSET);
    let result = storage
        .refill_with(|raw| {
            assert!(raw.is_empty());
            raw.append_field(b"new");
            Ok::<(), ()>(())
        })
        .expect("new bytes are valid UTF-8");
    assert_eq!(result, Ok(()));
    assert_eq!(storage.get(0), Some("new"));

    let raw = storage.into_storage();
    assert_eq!(raw.get(0), Some(b"new".as_slice()));

    let mut malformed = RecordStorage::new();
    malformed.append_field(b"ok");
    malformed.parts_mut().0.push(0xff);
    let error = Utf8RecordStorage::try_from_storage(malformed)
        .expect_err("trailing bytes outside the endpoints are malformed")
        .1;
    assert!(matches!(
        error,
        Utf8RecordError::InvalidField { index: 1, .. }
    ));
}
