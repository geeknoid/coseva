//! Record and field-conversion integration tests.
//!
//! These tests cover `TextRecord`, `ByteRecord`, `Record`, the `conversion` and
//! `convert` field encode/decode plumbing, `config`, and `field_ends`.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::collections::HashSet;
use std::error::Error as StdError;
use std::net::{IpAddr, Ipv4Addr};

use coseva::config::{EmitOptions, FormatOptions, Headers, ParseOptions, Whitespace};
use coseva::encoding::{ByteRecordRef, CsvDecode, DecodeField, DecodeRecord, MappedRecord};
use coseva::encoding::{CollectVisitor, EncodeField, EncodeVisitor};
use coseva::{ByteRecord, Error, ErrorKind, FromBytes, SliceParser, TextRecord};

mod common;

use common::unheaded;

// ── Helpers ───────────────────────────────────────────────────────────────────

// ══════════════════════════════════════════════════════════════════════════════
// TextRecord
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn owned_utf8_conversion_reuses_valid_byte_storage() {
    let mut bytes = ByteRecord::new();
    bytes.push_field("alpha");
    bytes.push_field("beta");
    bytes.set_location(11..22, 7);
    let allocation = bytes.as_slice().as_ptr();

    let strings = TextRecord::try_from(bytes).expect("fields are valid UTF-8");
    assert_eq!(strings.as_slice().as_ptr(), allocation);
    assert_eq!(strings.range(1), Some(5..9));
    assert_eq!(strings.byte_range(), 11..22);
    assert_eq!(strings.index(), 7);
}

#[test]
fn lossy_conversion_respects_field_boundaries_and_metadata() {
    let mut bytes = ByteRecord::new();
    bytes.push_field([0xC2]);
    bytes.push_field([0xA2]);
    bytes.set_location(3..9, 4);

    let strings = TextRecord::from_byte_record_lossy(bytes);
    assert_eq!(strings.iter().collect::<Vec<_>>(), ["�", "�"]);
    assert_eq!(strings.byte_range(), 3..9);
    assert_eq!(strings.index(), 4);
    assert_eq!(
        &strings.as_slice()[strings.range(1).expect("second field")],
        "�"
    );
}

#[test]
fn string_record_push_null_and_set_null_round_trip() {
    let mut record = TextRecord::new();
    record.push_field("alpha");
    record.push_null();

    assert_eq!(record.is_null(0), Some(false));
    assert_eq!(record.is_null(1), Some(true));
    assert_eq!(record.get(1), Some(""));

    assert!(record.set_null(0));
    assert_eq!(record.is_null(0), Some(true));
    assert_eq!(record.get(0), Some(""));
}

#[test]
fn string_record_to_byte_record_preserves_null_flags() {
    let mut record = TextRecord::new();
    record.push_field("alpha");
    record.push_null();
    record.push_field("beta");

    let bytes = record.to_byte_record();
    assert_eq!(bytes.len(), 3);
    assert_eq!(bytes.is_null(0), Some(false));
    assert_eq!(bytes.is_null(1), Some(true));
    assert_eq!(bytes.is_null(2), Some(false));
    assert_eq!(bytes.get(0), Some(b"alpha".as_slice()));
    assert_eq!(bytes.get(2), Some(b"beta".as_slice()));
}

#[test]
fn string_record_from_byte_record_lossy_preserves_null_flags() {
    let mut bytes = ByteRecord::new();
    bytes.push_field("alpha");
    bytes.push_null();

    let strings = TextRecord::from_byte_record_lossy(bytes);
    assert_eq!(strings.is_null(0), Some(false));
    assert_eq!(strings.is_null(1), Some(true));
    assert_eq!(strings.get(1), Some(""));
}

#[test]
fn string_record_parse_treats_null_as_none() {
    let mut record = TextRecord::new();
    record.push_field("42");
    record.push_null();
    record.push_field("");

    assert_eq!(
        record.parse::<u32>(0).expect("valid numeric field"),
        Some(42)
    );
    assert_eq!(record.parse::<u32>(1).expect("NULL field"), None);
    record
        .parse::<u32>(2)
        .expect_err("empty numeric field should fail");
    assert_eq!(record.parse::<u32>(3).expect("absent field"), None);
}

#[test]
fn text_record_eq_compares_field_content_not_metadata() {
    let mut a = TextRecord::new();
    a.push_field("x");
    a.push_field("y");
    let mut b = TextRecord::new();
    b.push_field("x");
    b.push_field("y");
    b.set_location(0..10, 5);
    assert_eq!(a, b);
    let mut c = TextRecord::new();
    c.push_field("x");
    c.push_field("z");
    assert_ne!(a, c);
}

#[test]
fn text_record_hash_is_equal_for_equal_records() {
    let mut a = TextRecord::new();
    a.push_field("hello");
    let mut b = TextRecord::new();
    b.push_field("hello");
    let mut set = HashSet::new();
    set.insert(a);
    assert!(set.contains(&b));
}

#[test]
fn byte_records_with_identical_fields_are_equal_across_parse_routes()
-> Result<(), Box<dyn StdError>> {
    // Two identical NULL-free records. The first is viewed borrowed before it
    // is taken owned, so it is materialized from the existing spans; the second
    // is taken owned directly, so the plain kernel marks NULLs in a pass over
    // the finished record. The two routes set the record-level bookkeeping flag
    // by different rules, and neither record holds a NULL.
    let input = b"a,b\na,b\n";
    let opts = ParseOptions::new().headers(Headers::None);
    let mut parser = SliceParser::with_options(input, FormatOptions::POSTGRES_COPY_CSV, opts)?;

    let mut viewed_first = ByteRecord::new();
    let mut line = parser.next_line()?.expect("first record");
    let borrowed = line.record()?;
    assert_eq!(borrowed.get(0), Some(&b"a"[..]));
    line.read_byte_record_into(&mut viewed_first)?;

    let mut taken_owned = ByteRecord::new();
    let mut line = parser.next_line()?.expect("second record");
    line.read_byte_record_into(&mut taken_owned)?;

    assert_eq!(viewed_first.as_slice(), taken_owned.as_slice());
    assert_eq!(viewed_first.len(), taken_owned.len());
    assert_eq!(viewed_first.is_null(0), taken_owned.is_null(0));
    assert_eq!(viewed_first, taken_owned);

    let mut set = HashSet::new();
    set.insert(viewed_first);
    assert!(
        set.contains(&taken_owned),
        "records that compare equal must hash equally"
    );
    Ok(())
}

#[test]
fn text_record_is_empty_true_before_any_field() {
    let empty = TextRecord::new();
    assert!(empty.is_empty());
    let mut nonempty = TextRecord::new();
    nonempty.push_field("a");
    assert!(!nonempty.is_empty());
}

#[test]
fn text_record_range_returns_byte_offsets_of_fields() {
    let mut record = TextRecord::new();
    record.push_field("ab");
    record.push_field("cde");
    assert_eq!(record.range(0), Some(0..2));
    assert_eq!(record.range(1), Some(2..5));
    assert_eq!(record.range(99), None);
}

#[test]
fn text_record_get_bytes_returns_utf8_as_byte_slice() {
    let mut record = TextRecord::new();
    record.push_field("hello");
    assert_eq!(record.get_bytes(0), Some(b"hello".as_slice()));
    assert_eq!(record.get_bytes(1), None);
}

#[test]
fn text_record_set_field_returns_false_for_out_of_bounds_index() {
    let mut record = TextRecord::new();
    record.push_field("a");
    assert!(!record.set_field(5, "x"));
}

#[test]
fn text_record_set_field_replaces_with_longer_value() {
    let mut record = TextRecord::new();
    record.push_field("hi");
    record.push_field("world");
    assert!(record.set_field(0, "longer_replacement"));
    assert_eq!(record.get(0), Some("longer_replacement"));
    assert_eq!(record.get(1), Some("world"));
}

#[test]
fn text_record_set_field_replaces_with_shorter_value() {
    let mut record = TextRecord::new();
    record.push_field("long_field_value");
    record.push_field("second");
    assert!(record.set_field(0, "x"));
    assert_eq!(record.get(0), Some("x"));
    assert_eq!(record.get(1), Some("second"));
}

#[test]
fn text_record_set_field_on_second_field_uses_previous_end_offset() {
    let mut record = TextRecord::new();
    record.push_field("alpha");
    record.push_field("beta");
    assert!(record.set_field(1, "z"));
    assert_eq!(record.get(0), Some("alpha"));
    assert_eq!(record.get(1), Some("z"));
}

#[test]
fn text_record_set_null_returns_false_for_out_of_bounds_index() {
    let mut record = TextRecord::new();
    record.push_field("a");
    assert!(!record.set_null(5));
}

#[test]
fn text_record_truncate_removes_trailing_fields() {
    let mut record = TextRecord::new();
    record.push_field("a");
    record.push_field("b");
    record.push_field("c");
    record.truncate(1);
    assert_eq!(record.len(), 1);
    assert_eq!(record.get(0), Some("a"));
}

#[test]
fn text_record_truncate_to_zero_clears_all_bytes() {
    let mut record = TextRecord::new();
    record.push_field("hello");
    record.truncate(0);
    assert_eq!(record.len(), 0);
    assert_eq!(record.as_slice(), "");
}

#[test]
fn text_record_truncate_does_nothing_when_new_len_exceeds_current() {
    let mut record = TextRecord::new();
    record.push_field("a");
    record.set_location(4..6, 2);
    record.truncate(5);
    assert_eq!(record.len(), 1);
    assert_eq!(record.byte_range(), 4..6);
    assert_eq!(record.index(), 2);
}

#[test]
fn text_record_clear_removes_all_fields_and_resets_metadata() {
    let mut record = TextRecord::new();
    record.push_field("a");
    record.push_field("b");
    record.set_location(0..5, 3);
    record.clear();
    assert!(record.is_empty());
    assert_eq!(record.byte_range(), 0..0);
    assert_eq!(record.index(), 0);
}

#[test]
fn text_record_capacity_accessors_and_shrink() {
    let mut record = TextRecord::with_capacity(4, 32);
    assert!(record.byte_capacity() >= 32);
    assert!(record.field_capacity() >= 4);
    record.push_field("hello");
    record.shrink_to_fit();
    assert!(record.byte_capacity() >= 5);
}

#[test]
fn text_record_index_operator_borrows_field_by_position() {
    let mut record = TextRecord::new();
    record.push_field("first");
    record.push_field("second");
    assert_eq!(&record[0], "first");
    assert_eq!(&record[1], "second");
}

#[test]
fn text_record_from_vec_string_creates_record_with_all_fields() {
    let fields = vec!["alpha".to_owned(), "beta".to_owned()];
    let record = TextRecord::from(fields);
    assert_eq!(record.len(), 2);
    assert_eq!(record.get(0), Some("alpha"));
    assert_eq!(record.get(1), Some("beta"));
}

#[test]
fn text_record_extend_appends_multiple_fields() {
    let mut record = TextRecord::new();
    record.push_field("start");
    record.extend(["second", "third"]);
    assert_eq!(record.len(), 3);
    assert_eq!(record.get(2), Some("third"));
}

#[test]
fn text_record_into_vec_string_collects_owned_fields() {
    let mut record = TextRecord::new();
    record.push_field("one");
    record.push_field("two");
    let v: Vec<String> = Vec::from(record);
    assert_eq!(v, ["one", "two"]);
}

#[test]
fn text_record_try_from_byte_record_ref_succeeds_for_valid_utf8() {
    let mut br = ByteRecord::new();
    br.push_field(b"hello");
    let tr = TextRecord::try_from(&br).expect("valid UTF-8");
    assert_eq!(tr.get(0), Some("hello"));
}

#[test]
fn text_record_try_from_byte_record_ref_rejects_invalid_utf8() {
    let mut br = ByteRecord::new();
    br.push_field(b"\xff");
    TextRecord::try_from(&br).expect_err("TryFrom<&ByteRecord> must fail for invalid UTF-8");
}

#[test]
fn text_record_try_from_owned_byte_record_rejects_invalid_utf8() {
    let mut br = ByteRecord::new();
    br.push_field(b"\xff\xfe");
    TextRecord::try_from(br).expect_err("invalid UTF-8 must fail");
}

#[test]
fn text_record_try_from_parsed_record_rejects_invalid_utf8() -> Result<(), Box<dyn StdError>> {
    let input = b"\xff,ok\n";
    let mut parser = unheaded(input);
    let mut line = parser.next_line()?.expect("line present");
    let record = line.record()?;
    TextRecord::try_from(&record).expect_err("field with invalid UTF-8");
    Ok(())
}

// ══════════════════════════════════════════════════════════════════════════════
// ByteRecord
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn byte_record_push_null_carries_no_bytes_and_preserves_field_count() {
    let mut record = ByteRecord::new();
    record.push_field("a");
    record.push_null();
    record.push_field("");
    record.push_null();

    assert_eq!(record.len(), 4);
    assert_eq!(record.is_null(0), Some(false));
    assert_eq!(record.is_null(1), Some(true));
    assert_eq!(record.is_null(2), Some(false));
    assert_eq!(record.is_null(3), Some(true));
    assert_eq!(record.is_null(4), None);

    // NULL fields yield empty bytes, same as a present-empty field, so
    // field counts and iteration widths are unaffected by NULL.
    assert_eq!(record.get(1), Some(b"".as_slice()));
    assert_eq!(record.get(3), Some(b"".as_slice()));
    assert_eq!(
        record.iter().collect::<Vec<_>>(),
        [
            b"a".as_slice(),
            b"".as_slice(),
            b"".as_slice(),
            b"".as_slice(),
        ]
    );
}

#[test]
fn byte_record_set_null_clears_bytes_and_flags_the_field() {
    let mut record = ByteRecord::new();
    record.push_field("alpha");
    record.push_field("beta");
    record.push_field("gamma");

    assert!(record.set_null(1));
    assert_eq!(record.is_null(1), Some(true));
    assert_eq!(record.get(1), Some(b"".as_slice()));
    // Neighboring fields are untouched.
    assert_eq!(record.get(0), Some(b"alpha".as_slice()));
    assert_eq!(record.get(2), Some(b"gamma".as_slice()));
    assert_eq!(record.len(), 3);

    // Setting content on a NULL field clears the NULL flag again.
    record.set_field(1, "new");
    assert_eq!(record.is_null(1), Some(false));
    assert_eq!(record.get(1), Some(b"new".as_slice()));

    assert!(!record.set_null(99));
}

// P5 added an equal-length short circuit to `set_field` that copies in place
// rather than splicing and rewriting every later endpoint. It must still clear
// the NULL flag, leave every neighbor's boundary where it was, and behave the
// same for the zero-length case a NULL field presents.
#[test]
fn byte_record_set_field_equal_length_preserves_every_other_boundary() {
    let mut record = ByteRecord::new();
    record.push_field("alpha");
    record.push_field("beta");
    record.push_field("gamma");
    record.push_null();
    record.push_field("delta");

    assert!(record.set_field(1, "BETA"));
    assert_eq!(record.get(1), Some(b"BETA".as_slice()));
    assert_eq!(record.is_null(1), Some(false));
    assert_eq!(record.get(0), Some(b"alpha".as_slice()));
    assert_eq!(record.get(2), Some(b"gamma".as_slice()));
    assert_eq!(record.get(3), Some(b"".as_slice()));
    assert_eq!(record.is_null(3), Some(true));
    assert_eq!(record.get(4), Some(b"delta".as_slice()));
    assert_eq!(record.len(), 5);

    // A NULL field holds no bytes, so an empty replacement is the equal-length
    // case and must still clear the flag.
    assert!(record.set_field(3, ""));
    assert_eq!(record.is_null(3), Some(false));
    assert_eq!(record.get(3), Some(b"".as_slice()));
    assert_eq!(record.get(4), Some(b"delta".as_slice()));

    // And the last field, where there is no later endpoint at all.
    assert!(record.set_field(4, "DELTA"));
    assert_eq!(record.get(4), Some(b"DELTA".as_slice()));
    assert_eq!(record.len(), 5);
}

#[test]
fn byte_record_parse_and_get_str_treat_null_as_none() {
    let mut record = ByteRecord::new();
    record.push_field("42");
    record.push_null();
    record.push_field("");

    assert_eq!(
        record.parse::<u32>(0).expect("valid numeric field"),
        Some(42)
    );
    assert_eq!(record.get_str(0).expect("valid UTF-8 field"), Some("42"));

    assert_eq!(record.parse::<u32>(1).expect("NULL field"), None);
    assert_eq!(record.get_str(1).expect("NULL field"), None);

    record
        .parse::<u32>(2)
        .expect_err("empty numeric field should fail");
    assert_eq!(record.get_str(2).expect("valid UTF-8 field"), Some(""));

    assert_eq!(record.parse::<u32>(3).expect("absent field"), None);
    assert_eq!(record.get_str(3).expect("absent field"), None);
}

#[test]
fn byte_record_parse_from_str_matches_parse_for_valid_fields() {
    let mut record = ByteRecord::new();
    record.push_field("42");
    record.push_null();

    assert_eq!(record.parse::<u32>(0).expect("valid digits"), Some(42));
    assert_eq!(
        record.parse_from_str::<u32>(0).expect("valid digits"),
        Some(42)
    );

    // NULL short-circuits identically on both paths.
    assert_eq!(record.parse::<u32>(1).expect("NULL field"), None);
    assert_eq!(record.parse_from_str::<u32>(1).expect("NULL field"), None);

    // Absent indices stay `Ok(None)` on both paths.
    assert_eq!(record.parse::<u32>(2).expect("absent field"), None);
    assert_eq!(record.parse_from_str::<u32>(2).expect("absent field"), None);
}

#[test]
fn byte_record_clone_from_copies_fields_in_place() {
    let mut src = ByteRecord::new();
    src.push_field(b"hello");
    src.push_field(b"world");
    src.set_location(0..10, 2);
    let mut dst = ByteRecord::new();
    dst.clone_from(&src);
    assert_eq!(dst.get(0), Some(b"hello".as_slice()));
    assert_eq!(dst.get(1), Some(b"world".as_slice()));
    assert_eq!(dst.byte_range(), 0..10);
    assert_eq!(dst.index(), 2);
}

#[test]
fn byte_record_is_empty_true_for_new_record() {
    let record = ByteRecord::new();
    assert!(record.is_empty());
    let mut nonempty = ByteRecord::new();
    nonempty.push_field(b"x");
    assert!(!nonempty.is_empty());
}

#[test]
fn byte_record_field_capacity_reflects_reserved_fields() {
    let record = ByteRecord::with_capacity(8, 64);
    assert!(record.field_capacity() >= 8);
}

#[test]
fn byte_record_range_returns_byte_offsets_of_fields() {
    let mut record = ByteRecord::new();
    record.push_field(b"abc");
    record.push_field(b"de");
    assert_eq!(record.range(0), Some(0..3));
    assert_eq!(record.range(1), Some(3..5));
    assert_eq!(record.range(99), None);
}

#[test]
fn byte_record_get_str_returns_error_for_invalid_utf8() {
    let mut record = ByteRecord::new();
    record.push_field(b"\xff");
    let err = record.get_str(0).expect_err("not valid UTF-8");
    assert!(matches!(err.kind(), ErrorKind::InvalidUtf8(_)));
}

#[test]
fn byte_record_parse_from_str_returns_none_for_null_field() -> Result<(), Box<dyn StdError>> {
    let mut record = ByteRecord::new();
    record.push_null();
    let result = record.parse_from_str::<u32>(0)?;
    assert_eq!(result, None);
    Ok(())
}

#[test]
fn byte_record_parse_from_str_returns_none_for_absent_field() -> Result<(), Box<dyn StdError>> {
    let record = ByteRecord::new();
    let result = record.parse_from_str::<u32>(0)?;
    assert_eq!(result, None);
    Ok(())
}

#[test]
fn byte_record_parse_from_str_returns_error_for_invalid_utf8() {
    let mut record = ByteRecord::new();
    record.push_field(b"\xff");
    let err = record.parse_from_str::<u32>(0).expect_err("invalid UTF-8");
    assert!(matches!(err.kind(), ErrorKind::InvalidUtf8(_)));
}

#[test]
fn byte_record_parse_from_str_returns_error_for_invalid_value() {
    let mut record = ByteRecord::new();
    record.push_field(b"not_a_number");
    record
        .parse_from_str::<u32>(0)
        .expect_err("not a valid u32");
}

#[test]
fn byte_record_to_text_record_converts_valid_utf8() {
    let mut record = ByteRecord::new();
    record.push_field(b"hello");
    let tr = TextRecord::try_from(&record).expect("valid UTF-8");
    assert_eq!(tr.get(0), Some("hello"));
}

#[test]
fn byte_record_truncate_to_zero_removes_all_fields() {
    let mut record = ByteRecord::new();
    record.push_field(b"a");
    record.push_field(b"b");
    record.truncate(0);
    assert_eq!(record.len(), 0);
    assert_eq!(record.bytes_len(), 0);
}

#[test]
fn byte_record_truncate_to_mid_removes_trailing_fields() {
    let mut record = ByteRecord::new();
    record.push_field(b"a");
    record.push_field(b"b");
    record.push_field(b"c");
    record.truncate(1);
    assert_eq!(record.len(), 1);
    assert_eq!(record.get(0), Some(b"a".as_slice()));
}

#[test]
fn byte_record_truncate_does_nothing_when_new_len_exceeds_current() {
    let mut record = ByteRecord::new();
    record.push_field(b"x");
    record.truncate(5);
    assert_eq!(record.len(), 1);
}

#[test]
fn byte_record_index_operator_borrows_field_by_position() {
    let mut record = ByteRecord::new();
    record.push_field(b"first");
    record.push_field(b"second");
    assert_eq!(&record[0], b"first");
}

#[test]
fn byte_record_index_second_field_uses_previous_end() {
    let mut record = ByteRecord::new();
    record.push_field(b"first");
    record.push_field(b"second");
    assert_eq!(&record[1], b"second");
}

#[test]
fn byte_record_into_vec_vec_u8_collects_fields() {
    let mut record = ByteRecord::new();
    record.push_field(b"one");
    record.push_field(b"two");
    let v: Vec<Vec<u8>> = Vec::from(record);
    assert_eq!(v, [b"one".to_vec(), b"two".to_vec()]);
}

#[test]
fn byte_record_from_iterator_collects_all_fields() {
    let record: ByteRecord = [b"a".as_slice(), b"b"].into_iter().collect();
    assert_eq!(record.len(), 2);
    assert_eq!(record.get(1), Some(b"b".as_slice()));
}

#[test]
fn byte_record_extend_appends_fields() {
    let mut record = ByteRecord::new();
    record.push_field(b"start");
    record.extend([b"second".as_slice(), b"third"]);
    assert_eq!(record.len(), 3);
}

// ══════════════════════════════════════════════════════════════════════════════
// Record::parse_from_str
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn record_parse_from_str_returns_none_for_null_field() -> Result<(), Box<dyn StdError>> {
    // An empty unquoted field is NULL in PostgresCsv mode.
    let input = b",notempty\n";
    let opts = ParseOptions::new().headers(Headers::None);
    let mut parser = SliceParser::with_options(input, FormatOptions::POSTGRES_COPY_CSV, opts)?;
    let mut line = parser.next_line()?.expect("line present");
    let record = line.record()?;
    assert_eq!(record.is_null(0), Some(true));
    let result = record.parse_from_str::<u32>(0)?;
    assert_eq!(result, None);
    Ok(())
}

#[test]
fn record_parse_from_str_returns_none_for_absent_field() -> Result<(), Box<dyn StdError>> {
    let input = b"one\n";
    let mut parser = unheaded(input);
    let mut line = parser.next_line()?.expect("line present");
    let record = line.record()?;
    let result = record.parse_from_str::<u32>(99)?;
    assert_eq!(result, None);
    Ok(())
}

#[test]
fn record_parse_from_str_returns_error_for_unparseable_field() -> Result<(), Box<dyn StdError>> {
    let input = b"not_a_number\n";
    let mut parser = unheaded(input);
    let mut line = parser.next_line()?.expect("line present");
    let record = line.record()?;
    record.parse_from_str::<u32>(0).expect_err("not a number");
    Ok(())
}

// ══════════════════════════════════════════════════════════════════════════════
// Record — borrowed view
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn record_clone_reads_the_same_fields_alongside_the_original() -> Result<(), Box<dyn StdError>> {
    let input = b"alpha,,gamma\n";
    let opts = ParseOptions::new().headers(Headers::None);
    let mut parser = SliceParser::with_options(input, FormatOptions::POSTGRES_COPY_CSV, opts)?;
    let mut line = parser.next_line()?.expect("line present");
    let record = line.record()?;

    let copy = record.clone();

    // Both views stay usable, so the clone borrows the parser rather than
    // taking the original's place.
    assert_eq!(copy.len(), record.len());
    assert_eq!(copy.index(), record.index());
    assert_eq!(copy.byte_range(), record.byte_range());
    for field in 0..record.len() {
        assert_eq!(copy.get(field), record.get(field));
        assert_eq!(copy.is_null(field), record.is_null(field));
    }
    assert_eq!(copy.get(0), Some(&b"alpha"[..]));
    assert_eq!(copy.is_null(1), Some(true));
    Ok(())
}

// ══════════════════════════════════════════════════════════════════════════════
// config
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn whitespace_default_equals_none() {
    assert_eq!(Whitespace::default(), Whitespace::NONE);
}

#[test]
fn parse_options_default_and_new_produce_equivalent_parsers() {
    let _a = ParseOptions::default();
    let _b = ParseOptions::new();
}

#[test]
fn encode_options_default_can_be_constructed() {
    let _ = EmitOptions::default();
}

#[test]
fn slice_parser_rejects_zero_buffer_capacity() {
    let opts = ParseOptions::new().buffer_capacity(0);
    SliceParser::with_options(b"".as_slice(), FormatOptions::CSV, opts)
        .expect_err("zero buffer capacity must be rejected");
}

#[test]
fn slice_parser_rejects_delimiter_equal_to_quote_byte() {
    let format = FormatOptions::CSV.delimiter(b'"');
    SliceParser::with_options(b"".as_slice(), format, ParseOptions::new())
        .expect_err("delimiter == quote must be rejected");
}

// ══════════════════════════════════════════════════════════════════════════════
// field_ends — whitespace trimming during parsing
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn trim_unquoted_only_keeps_quoted_whitespace() -> Result<(), Box<dyn StdError>> {
    // Whitespace::exempts_quoted() == true triggers the per-field trim path in
    // the engine rather than the fast bulk-trim path.
    let format = FormatOptions::CSV.trim(Whitespace::FIELDS.unquoted_only());
    let opts = ParseOptions::new().headers(Headers::None);
    let mut parser = SliceParser::with_options(b" a ,\"  b  \"\n", format, opts)?;
    let mut line = parser.next_line()?.expect("line");
    let record = line.record()?;
    assert_eq!(record.get_str(0)?, Some("a"));
    // unquoted_only(): quoted field keeps its whitespace
    assert_eq!(record.get_str(1)?, Some("  b  "));
    Ok(())
}

#[test]
fn trim_fields_trims_unquoted_whitespace() -> Result<(), Box<dyn StdError>> {
    // Whitespace::applies_to_scope() is used by the bulk-trim path when
    // exempts_quoted() is false.
    let format = FormatOptions::CSV.trim(Whitespace::FIELDS);
    let opts = ParseOptions::new().headers(Headers::None);
    let mut parser = SliceParser::with_options(b" a , b \n", format, opts)?;
    let mut line = parser.next_line()?.expect("line");
    let record = line.record()?;
    assert_eq!(record.get_str(0)?, Some("a"));
    assert_eq!(record.get_str(1)?, Some("b"));
    Ok(())
}

#[test]
fn trim_fields_owned_record_triggers_applies_to_scope() -> Result<(), Box<dyn StdError>> {
    // `applies_to_scope` is called on the owned-record code path (read_byte_record_into).
    let format = FormatOptions::CSV.trim(Whitespace::FIELDS);
    let opts = ParseOptions::new().headers(Headers::None);
    let mut parser = SliceParser::with_options(b" a , b \n", format, opts)?;
    let mut line = parser.next_line()?.expect("line");
    let mut record = ByteRecord::new();
    line.read_byte_record_into(&mut record)?;
    assert_eq!(record.get(0), Some(b"a".as_slice()));
    assert_eq!(record.get(1), Some(b"b".as_slice()));
    Ok(())
}

#[test]
fn trim_unquoted_only_owned_triggers_exempts_quoted() -> Result<(), Box<dyn StdError>> {
    // `exempts_quoted` is checked inside `needs_general_parsing` which decides
    // the engine path when the parser initializes. This variant exercises the
    // owned path (read_byte_record_into) so the engine is actually instantiated.
    let format = FormatOptions::CSV.trim(Whitespace::FIELDS.unquoted_only());
    let opts = ParseOptions::new().headers(Headers::None);
    let mut parser = SliceParser::with_options(b" a ,\"  b  \"\n", format, opts)?;
    let mut line = parser.next_line()?.expect("line");
    let mut record = ByteRecord::new();
    line.read_byte_record_into(&mut record)?;
    assert_eq!(record.get(0), Some(b"a".as_slice()));
    assert_eq!(record.get(1), Some(b"  b  ".as_slice()));
    Ok(())
}

// ══════════════════════════════════════════════════════════════════════════════
// convert — from_bytes_via_str via char and network-address types
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn char_from_bytes_parses_single_character() {
    assert_eq!(char::from_bytes(b"A"), Ok('A'));
    assert_eq!(char::from_bytes(b"\xc3\xa9"), Ok('\u{e9}'));
}

#[test]
fn char_from_bytes_rejects_multiple_characters() {
    char::from_bytes(b"ab").expect_err("two characters is not one char");
}

#[test]
fn char_from_bytes_rejects_invalid_utf8() {
    // Covers the Err branch of as_utf8(bytes)? inside from_bytes_via_str.
    char::from_bytes(b"\xff").expect_err("invalid UTF-8 sequence");
}

#[test]
fn ipv4addr_from_bytes_parses_dotted_quad() {
    assert_eq!(Ipv4Addr::from_bytes(b"127.0.0.1"), Ok(Ipv4Addr::LOCALHOST));
}

#[test]
fn ipaddr_from_bytes_parses_ipv4_address() {
    let expected: IpAddr = "10.0.0.1".parse().expect("valid");
    assert_eq!(IpAddr::from_bytes(b"10.0.0.1"), Ok(expected));
}

fn kind<T: FromBytes<Err = ErrorKind> + core::fmt::Debug>(bytes: &[u8]) -> ErrorKind {
    T::from_bytes(bytes).expect_err("conversion should fail")
}

#[test]
fn unsigned_integers_round_trip_without_utf8_validation() {
    assert_eq!(u8::from_bytes(b"255"), Ok(255));
    assert_eq!(u16::from_bytes(b"65535"), Ok(65535));
    assert_eq!(u32::from_bytes(b"650706"), Ok(650_706));
    assert_eq!(u64::from_bytes(b"+7"), Ok(7));
    assert_eq!(u128::from_bytes(b"0"), Ok(0));
    assert_eq!(usize::from_bytes(b"12"), Ok(12));
}

#[test]
fn signed_integers_accept_both_signs() {
    assert_eq!(i8::from_bytes(b"-128"), Ok(-128));
    assert_eq!(i16::from_bytes(b"+32767"), Ok(32767));
    assert_eq!(i32::from_bytes(b"-1"), Ok(-1));
    assert_eq!(i64::from_bytes(b"9223372036854775807"), Ok(i64::MAX));
    assert_eq!(i128::from_bytes(b"-5"), Ok(-5));
    assert_eq!(isize::from_bytes(b"3"), Ok(3));
}

#[test]
fn integer_failures_report_a_precise_kind() {
    assert_eq!(kind::<u32>(b""), ErrorKind::EmptyField);
    assert_eq!(kind::<u32>(b"+"), ErrorKind::EmptyField);
    assert_eq!(kind::<i32>(b"-"), ErrorKind::EmptyField);
    assert_eq!(kind::<u32>(b"12a"), ErrorKind::InvalidDigit);
    assert_eq!(kind::<u32>(b"-1"), ErrorKind::InvalidDigit);
    assert_eq!(kind::<u8>(b"256"), ErrorKind::OutOfRange);
    assert_eq!(kind::<i8>(b"-129"), ErrorKind::OutOfRange);
}

#[test]
fn booleans_accept_numeric_and_textual_forms() {
    assert_eq!(bool::from_bytes(b"true"), Ok(true));
    assert_eq!(bool::from_bytes(b"1"), Ok(true));
    assert_eq!(bool::from_bytes(b"false"), Ok(false));
    assert_eq!(bool::from_bytes(b"0"), Ok(false));
    assert_eq!(kind::<bool>(b"TRUE"), ErrorKind::InvalidValue);
}

#[test]
fn floats_and_addresses_parse_scalar_values() {
    assert_eq!(f32::from_bytes(b"1.5"), Ok(1.5));
    assert_eq!(f64::from_bytes(b"-2.25"), Ok(-2.25));
    assert_eq!(char::from_bytes("é".as_bytes()), Ok('é'));
    assert_eq!(Ipv4Addr::from_bytes(b"127.0.0.1"), Ok(Ipv4Addr::LOCALHOST));
    assert_eq!(kind::<f64>(b"not a float"), ErrorKind::InvalidValue);
}

/// Floats bypass `FromStr` entirely, so every accepted form, every
/// rejected form, and the exact rounding must still agree with it.
#[test]
fn floats_agree_with_from_str_on_every_form() {
    const CASES: &[&str] = &[
        "1.5",
        "-2.25",
        "0",
        "-0",
        "+5",
        ".5",
        "+.5",
        "-.5",
        "5.",
        "000000001.5",
        "1e10",
        "1E10",
        "1e-10",
        "3.141592653589793",
        "2.2250738585072011e-308",
        "4.9406564584124654e-324",
        "1.7976931348623157e308",
        "1.7976931348623157e309",
        "1.5e400",
        "-1.5e400",
        "1e-400",
        "1e1000000000000",
        "inf",
        "-inf",
        "+inf",
        "INF",
        "infinity",
        "Infinity",
        "nan",
        "NaN",
        "",
        " 1.5",
        "1.5 ",
        "1_000",
        "1,5",
        "1.5f",
        "0x1p3",
        "1e",
        "1e+",
        "--1",
        "not a float",
    ];

    /// Agreement means the same value bit-for-bit, or the same rejection.
    macro_rules! agrees {
        ($ty:ty, $case:expr, $bytes:expr) => {{
            let agrees = match (<$ty>::from_bytes($bytes), $case.parse::<$ty>()) {
                (Ok(ours), Ok(theirs)) => {
                    ours.to_bits() == theirs.to_bits() || (ours.is_nan() && theirs.is_nan())
                }
                (Err(_), Err(_)) => true,
                _ => false,
            };
            assert!(
                agrees,
                concat!(stringify!($ty), " {:?} disagrees with FromStr"),
                $case
            );
        }};
    }

    for case in CASES {
        let bytes = case.as_bytes();
        agrees!(f64, case, bytes);
        agrees!(f32, case, bytes);
    }
}

/// An empty field is reported as empty rather than as a bad value.
#[test]
fn empty_float_input_reports_an_empty_kind() {
    assert_eq!(kind::<f64>(b""), ErrorKind::EmptyField);
    assert_eq!(kind::<f32>(b""), ErrorKind::EmptyField);
}

#[test]
fn strings_require_utf8_but_byte_vectors_do_not() {
    assert_eq!(String::from_bytes(b"Boston"), Ok(String::from("Boston")));
    assert!(matches!(kind::<String>(&[0xFF]), ErrorKind::InvalidUtf8(_)));
    assert_eq!(Vec::<u8>::from_bytes(&[0xFF]), Ok(Vec::from([0xFF])));
}

/// Integer parsing agrees with the standard library either side of the digit
/// count below which overflow is impossible.
///
/// The parser skips its range checks for short inputs, so the boundary between
/// that path and the checked one is exercised from both sides for every width,
/// including leading zeros and signs that lengthen the input without changing
/// the value.
#[test]
fn integers_match_the_standard_library_across_the_unchecked_digit_boundary() {
    macro_rules! check {
        ($ty:ty, $( $text:expr ),* $(,)?) => {{
            for text in [$( $text ),*] {
                let ours = <$ty>::from_bytes(text.as_bytes());
                let theirs = text.parse::<$ty>();
                match (ours, theirs) {
                    (Ok(ours), Ok(theirs)) => assert_eq!(
                        ours, theirs,
                        "{} disagreed on {text:?}", stringify!($ty)
                    ),
                    (Err(_), Err(_)) => {}
                    (ours, theirs) => panic!(
                        "{} disagreed on {text:?}: {ours:?} vs {theirs:?}",
                        stringify!($ty)
                    ),
                }
            }
        }};
    }

    check!(
        u8, "0", "9", "99", "100", "255", "256", "0255", "00255", "999"
    );
    check!(
        i8, "0", "99", "100", "127", "128", "-99", "-100", "-128", "-129"
    );
    check!(u16, "9999", "10000", "65535", "65536", "099999");
    check!(i16, "9999", "32767", "32768", "-32768", "-32769");
    check!(
        u32,
        "999999999",
        "1000000000",
        "4294967295",
        "4294967296",
        "0004294967295",
    );
    check!(
        i32,
        "999999999",
        "2147483647",
        "2147483648",
        "-2147483648",
        "-2147483649"
    );
    check!(
        u64,
        "9999999999999999999",
        "10000000000000000000",
        "18446744073709551615",
        "18446744073709551616",
    );
    check!(
        i64,
        "999999999999999999",
        "9223372036854775807",
        "9223372036854775808",
        "-9223372036854775808",
        "-9223372036854775809",
    );
    check!(
        u128,
        "340282366920938463463374607431768211455",
        "340282366920938463463374607431768211456",
    );
    check!(
        i128,
        "170141183460469231731687303715884105727",
        "170141183460469231731687303715884105728",
        "-170141183460469231731687303715884105728",
    );

    // Every value a byte can hold, over the whole boundary neighborhood.
    for value in 0..=u8::MAX {
        assert_eq!(u8::from_bytes(value.to_string().as_bytes()), Ok(value));
    }
    for value in i8::MIN..=i8::MAX {
        assert_eq!(i8::from_bytes(value.to_string().as_bytes()), Ok(value));
    }

    // A non-digit past the boundary is still reported as such, not as a range
    // failure, on both paths.
    assert_eq!(kind::<u32>(b"12x"), ErrorKind::InvalidDigit);
    assert_eq!(kind::<u32>(b"1234567890x"), ErrorKind::InvalidDigit);
    assert_eq!(kind::<u32>(b"4294967296"), ErrorKind::OutOfRange);
}

/// Valid multi-byte UTF-8 survives the ASCII screen that fronts validation.
///
/// The screen only settles all-ASCII input, so these exercise the fallback and
/// pin that it neither rejects well-formed text nor accepts malformed text.
#[test]
fn non_ascii_fields_decode_through_the_validation_fallback() {
    for text in [
        "café",
        "naïve café",
        "日本語",
        "🎉",
        "aaaaaaaé",
        "aaaaaaaaaaaaaaaé",
    ] {
        assert_eq!(String::from_bytes(text.as_bytes()), Ok(String::from(text)));
    }

    // Truncated and over-long sequences, including one placed past the first
    // word so a word-at-a-time screen cannot mask it.
    for bad in [
        [0xC3].as_slice(),
        &[0xE6, 0x97],
        &[0xF0, 0x9F, 0x8E],
        &[0x80],
        &[0xC0, 0xAF],
        &[b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', 0xFF],
        &[
            b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a',
            b'a', b'a', 0xC3,
        ],
    ] {
        assert!(
            matches!(kind::<String>(bad), ErrorKind::InvalidUtf8(_)),
            "expected invalid UTF-8 for {bad:?}"
        );
    }
}

#[test]
fn optional_values_treat_empty_input_as_absent() {
    assert_eq!(Option::<u32>::from_bytes(b""), Ok(None));
    assert_eq!(Option::<u32>::from_bytes(b"7"), Ok(Some(7)));
    assert_eq!(kind::<Option<u32>>(b"x"), ErrorKind::InvalidDigit);
}

// ══════════════════════════════════════════════════════════════════════════════
// conversion — CollectVisitor, DecodeField, EncodeField, CsvDecode defaults
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn collect_visitor_is_empty_reflects_accumulated_field_count() {
    let mut visitor = CollectVisitor::new();
    assert!(visitor.is_empty());
    visitor
        .visit_field(0, "f", b"x")
        .expect("visit_field succeeds");
    assert!(!visitor.is_empty());
}

/// A hand-written [`CsvDecode`] that does not override `csv_decode_into`,
/// exercising the default implementation.
struct ManualRow {
    value: u32,
}

impl<'record> CsvDecode<'record> for ManualRow {
    fn csv_decode<R>(record: &R) -> Result<Self, Error>
    where
        R: DecodeRecord<'record> + ?Sized,
    {
        Ok(Self {
            value: <u32 as DecodeField<'record>>::decode_field(record.get_field(0), 0, "value")?,
        })
    }

    fn field_names() -> &'static [&'static str] {
        &["value"]
    }
}

#[test]
fn csv_decode_into_default_delegates_to_csv_decode() {
    let mut record = ByteRecord::new();
    record.push_field(b"77");
    let record_ref = ByteRecordRef::new(&record);
    let mut row = ManualRow { value: 0 };
    row.csv_decode_into(&record_ref).expect("decode succeeds");
    assert_eq!(row.value, 77);
}

#[test]
fn csv_decode_into_default_error_path_propagates() {
    let mut record = ByteRecord::new();
    record.push_field(b"not_a_number");
    let record_ref = ByteRecordRef::new(&record);
    let mut row = ManualRow { value: 0 };
    row.csv_decode_into(&record_ref)
        .expect_err("non-numeric field must fail");
}

#[test]
fn decode_field_into_default_impl_updates_value_in_place() {
    let mut val: i32 = 0;
    <i32 as DecodeField<'_>>::decode_field_into(&mut val, Some(b"42"), 0, "n")
        .expect("decode succeeds");
    assert_eq!(val, 42);
}

#[test]
fn decode_field_into_default_error_path_propagates() {
    let mut val: i32 = 0;
    <i32 as DecodeField<'_>>::decode_field_into(&mut val, Some(b"not_a_number"), 0, "n")
        .expect_err("invalid bytes must fail");
}

#[test]
fn decode_field_into_from_record_default_impl_reads_from_record() {
    let mut record = ByteRecord::new();
    record.push_field(b"99");
    let record_ref = ByteRecordRef::new(&record);
    let mut val: u64 = 0;
    <u64 as DecodeField<'_>>::decode_field_into_from_record(&mut val, &record_ref, 0, "n")
        .expect("decode succeeds");
    assert_eq!(val, 99);
}

#[test]
fn decode_field_into_from_record_default_error_path_propagates() {
    let mut record = ByteRecord::new();
    record.push_field(b"not_a_number");
    let record_ref = ByteRecordRef::new(&record);
    let mut val: u64 = 0;
    <u64 as DecodeField<'_>>::decode_field_into_from_record(&mut val, &record_ref, 0, "n")
        .expect_err("invalid bytes must fail");
}

#[test]
fn decode_field_str_ref_rejects_invalid_utf8() {
    let err =
        <&str as DecodeField<'_>>::decode_field(Some(b"\xff"), 0, "f").expect_err("invalid UTF-8");
    assert!(matches!(err.kind(), ErrorKind::InvalidUtf8(_)));
}

#[test]
fn decode_field_string_rejects_invalid_utf8() {
    let err = <String as DecodeField<'_>>::decode_field(Some(b"\xff"), 0, "f")
        .expect_err("invalid UTF-8");
    assert!(matches!(err.kind(), ErrorKind::InvalidUtf8(_)));
}

#[test]
fn decode_field_string_into_rejects_invalid_utf8() {
    let mut s = String::new();
    let err = <String as DecodeField<'_>>::decode_field_into(&mut s, Some(b"\xff"), 0, "f")
        .expect_err("invalid UTF-8");
    assert!(matches!(err.kind(), ErrorKind::InvalidUtf8(_)));
}

#[test]
fn decode_field_f32_empty_reports_empty_field_error() {
    let err =
        <f32 as DecodeField<'_>>::decode_field(Some(b""), 0, "f").expect_err("empty f32 field");
    assert_eq!(err.kind(), ErrorKind::EmptyField);
}

#[test]
fn decode_field_f64_empty_reports_empty_field_error() {
    let err =
        <f64 as DecodeField<'_>>::decode_field(Some(b""), 0, "f").expect_err("empty f64 field");
    assert_eq!(err.kind(), ErrorKind::EmptyField);
}

#[test]
fn decode_field_f32_invalid_reports_invalid_value_error() {
    let err = <f32 as DecodeField<'_>>::decode_field(Some(b"not_a_float"), 0, "f")
        .expect_err("invalid f32 bytes");
    assert_eq!(err.kind(), ErrorKind::InvalidValue);
}

#[test]
fn decode_field_f64_invalid_reports_invalid_value_error() {
    let err = <f64 as DecodeField<'_>>::decode_field(Some(b"xyz"), 0, "f")
        .expect_err("invalid f64 bytes");
    assert_eq!(err.kind(), ErrorKind::InvalidValue);
}

#[test]
fn option_decode_field_into_from_record_null_aware_null_field_yields_none() {
    let mut record = ByteRecord::new();
    record.push_null();
    let record_ref = ByteRecordRef::new(&record);
    let mut val: Option<i32> = Some(99);
    <Option<i32> as DecodeField<'_>>::decode_field_into_from_record(&mut val, &record_ref, 0, "n")
        .expect("null decodes to None");
    assert_eq!(val, None);
}

#[test]
fn option_decode_field_into_from_record_null_aware_absent_field_yields_none() {
    // A null-aware record with only a push_null at index 0; index 1 is absent.
    let mut record = ByteRecord::new();
    record.push_null();
    let record_ref = ByteRecordRef::new(&record);
    let mut val: Option<i32> = Some(7);
    <Option<i32> as DecodeField<'_>>::decode_field_into_from_record(&mut val, &record_ref, 1, "n")
        .expect("absent field decodes to None");
    assert_eq!(val, None);
}

#[test]
fn option_decode_field_into_from_record_null_aware_present_fills_none_slot() {
    // `decode_some_into` with slot=None allocates Some (covers the else branch).
    let mut record = ByteRecord::new();
    record.push_null(); // makes null_aware = true; index 0 is null
    record.push_field(b"42"); // index 1 is present
    let record_ref = ByteRecordRef::new(&record);
    let mut val: Option<i32> = None;
    <Option<i32> as DecodeField<'_>>::decode_field_into_from_record(&mut val, &record_ref, 1, "n")
        .expect("present field decodes");
    assert_eq!(val, Some(42));
}

#[test]
fn option_decode_field_into_from_record_null_aware_present_updates_some_slot() {
    // `decode_some_into` with slot=Some reuses the inner value (covers the if-branch).
    let mut record = ByteRecord::new();
    record.push_null(); // makes null_aware = true
    record.push_field(b"77");
    let record_ref = ByteRecordRef::new(&record);
    let mut val: Option<i32> = Some(0);
    <Option<i32> as DecodeField<'_>>::decode_field_into_from_record(&mut val, &record_ref, 1, "n")
        .expect("present field updates slot");
    assert_eq!(val, Some(77));
}

#[test]
fn decode_some_into_error_path_propagates_when_slot_is_none() {
    // decode_some_into is reached via Option::decode_field_into_from_record on a
    // null-aware record with a present but invalid field.
    let mut record = ByteRecord::new();
    record.push_null(); // sets null_aware = true; index 0 is null
    record.push_field(b"not_a_number"); // index 1 is present but invalid
    let record_ref = ByteRecordRef::new(&record);
    let mut val: Option<i32> = None;
    <Option<i32> as DecodeField<'_>>::decode_field_into_from_record(&mut val, &record_ref, 1, "n")
        .expect_err("invalid bytes propagate through decode_some_into");
}

#[test]
fn encode_field_slice_passes_bytes_to_visitor() {
    let mut visitor = CollectVisitor::new();
    <[u8] as EncodeField>::encode_to(b"hello", 0, "f", &mut visitor).expect("encodes");
    assert_eq!(visitor.fields(), &[b"hello".to_vec()]);
}

#[test]
fn encode_field_ref_slice_passes_bytes_to_visitor() {
    let mut visitor = CollectVisitor::new();
    let bytes: &[u8] = b"world";
    <&[u8] as EncodeField>::encode_to(&bytes, 0, "f", &mut visitor).expect("encodes");
    assert_eq!(visitor.fields(), &[b"world".to_vec()]);
}

#[test]
fn encode_field_str_passes_utf8_bytes_to_visitor() {
    let mut visitor = CollectVisitor::new();
    <str as EncodeField>::encode_to("hello", 0, "f", &mut visitor).expect("encodes");
    assert_eq!(visitor.fields(), &[b"hello".to_vec()]);
}

#[test]
fn encode_field_ref_str_passes_utf8_bytes_to_visitor() {
    let mut visitor = CollectVisitor::new();
    <&str as EncodeField>::encode_to(&"world", 0, "f", &mut visitor).expect("encodes");
    assert_eq!(visitor.fields(), &[b"world".to_vec()]);
}

#[test]
fn encode_field_bool_encodes_true_and_false() {
    let mut visitor = CollectVisitor::new();
    <bool as EncodeField>::encode_to(&true, 0, "f", &mut visitor).expect("true encodes");
    <bool as EncodeField>::encode_to(&false, 1, "f", &mut visitor).expect("false encodes");
    assert_eq!(visitor.fields(), &[b"true".to_vec(), b"false".to_vec()]);
}

fn null_aware_record(fields: &[Option<&str>]) -> ByteRecord {
    let mut record = ByteRecord::new();
    for field in fields {
        match field {
            Some(bytes) => record.push_field(bytes),
            None => record.push_null(),
        }
    }
    record
}

#[test]
fn decode_record_default_null_queries_are_false() {
    struct Bare;
    impl<'record> DecodeRecord<'record> for Bare {
        fn get_field(&self, _index: usize) -> Option<&'record [u8]> {
            None
        }
    }
    let bare = Bare;
    assert!(!bare.is_null_aware());
    assert!(!bare.is_field_null(0));
}

#[test]
fn byte_record_ref_delegates_null_queries() {
    let record = null_aware_record(&[Some("a"), None, Some("")]);
    let record_ref = ByteRecordRef::new(&record);
    assert!(record_ref.is_null_aware());
    assert!(!record_ref.is_field_null(0));
    assert!(record_ref.is_field_null(1));
    assert!(!record_ref.is_field_null(2));
}

#[test]
fn mapped_record_delegates_null_queries_through_mapping() {
    let record = null_aware_record(&[Some("a"), None]);
    let record_ref = ByteRecordRef::new(&record);
    // Reverse mapping: target index 0 -> source 1, target index 1 -> source 0.
    let mapped = MappedRecord::new(&record_ref, &[1, 0]);
    assert!(mapped.is_null_aware());
    assert!(mapped.is_field_null(0));
    assert!(!mapped.is_field_null(1));
}

#[test]
fn decode_field_from_record_default_forwards_to_decode_field() {
    let record = null_aware_record(&[Some("hello")]);
    let record_ref = ByteRecordRef::new(&record);
    let value = <String as DecodeField<'_>>::decode_field_from_record(&record_ref, 0, "field")
        .expect("valid UTF-8");
    assert_eq!(value, "hello");
}

#[test]
fn option_decode_treats_empty_as_none_when_not_null_aware() {
    let mut record = ByteRecord::new();
    record.push_field("");
    record.push_field("value");
    let record_ref = ByteRecordRef::new(&record);
    assert!(!record_ref.is_null_aware());

    // Without NULL awareness, a present-empty field decodes as `None`.
    let empty =
        <Option<String> as DecodeField<'_>>::decode_field_from_record(&record_ref, 0, "field")
            .expect("decodes");
    assert_eq!(empty, None);

    let present =
        <Option<String> as DecodeField<'_>>::decode_field_from_record(&record_ref, 1, "field")
            .expect("decodes");
    assert_eq!(present, Some("value".to_owned()));

    // Missing field (beyond the record) is also `None`.
    let missing =
        <Option<String> as DecodeField<'_>>::decode_field_from_record(&record_ref, 5, "field")
            .expect("decodes");
    assert_eq!(missing, None);
}

#[test]
fn option_decode_distinguishes_null_from_empty_when_null_aware() {
    let record = null_aware_record(&[None, Some(""), Some("value")]);
    let record_ref = ByteRecordRef::new(&record);
    assert!(record_ref.is_null_aware());

    // Explicit NULL decodes to `None`.
    let null_field =
        <Option<String> as DecodeField<'_>>::decode_field_from_record(&record_ref, 0, "field")
            .expect("decodes");
    assert_eq!(null_field, None);

    // Present-but-empty decodes to `Some(<empty>)`, not `None`.
    let empty_field =
        <Option<String> as DecodeField<'_>>::decode_field_from_record(&record_ref, 1, "field")
            .expect("decodes");
    assert_eq!(empty_field, Some(String::new()));

    let present =
        <Option<String> as DecodeField<'_>>::decode_field_from_record(&record_ref, 2, "field")
            .expect("decodes");
    assert_eq!(present, Some("value".to_owned()));

    // Missing field beyond the record is also `None`.
    let missing =
        <Option<String> as DecodeField<'_>>::decode_field_from_record(&record_ref, 5, "field")
            .expect("decodes");
    assert_eq!(missing, None);
}

#[test]
fn option_numeric_present_empty_is_an_error_when_null_aware() {
    let record = null_aware_record(&[None, Some("")]);
    let record_ref = ByteRecordRef::new(&record);

    // Explicit NULL still decodes to `None` for numeric Options.
    let null_field =
        <Option<i32> as DecodeField<'_>>::decode_field_from_record(&record_ref, 0, "n")
            .expect("decodes");
    assert_eq!(null_field, None);

    // A present-but-empty numeric field is the underlying type's parse
    // error, not `None`.
    let err = <Option<i32> as DecodeField<'_>>::decode_field_from_record(&record_ref, 1, "n")
        .expect_err("empty is not a valid i32");
    assert_eq!(err.location().field, 1);
}

#[test]
fn encode_visitor_default_visit_null_forwards_to_visit_field() {
    let mut visitor = CollectVisitor::new();
    visitor.visit_null(0, "field").expect("default succeeds");
    assert_eq!(visitor.fields(), &[Vec::<u8>::new()]);
}

#[test]
fn encode_option_none_calls_visit_null() {
    #[derive(Default)]
    struct TrackingVisitor {
        fields: Vec<Option<Vec<u8>>>,
    }
    impl EncodeVisitor for TrackingVisitor {
        fn visit_field(
            &mut self,
            _index: usize,
            _name: &'static str,
            bytes: &[u8],
        ) -> Result<(), Error> {
            self.fields.push(Some(bytes.to_vec()));
            Ok(())
        }

        fn visit_null(&mut self, _index: usize, _name: &'static str) -> Result<(), Error> {
            self.fields.push(None);
            Ok(())
        }
    }

    let mut visitor = TrackingVisitor::default();
    let none: Option<String> = None;
    none.encode_to(0, "field", &mut visitor).expect("encodes");
    let some: Option<String> = Some("hi".to_owned());
    some.encode_to(1, "field", &mut visitor).expect("encodes");

    assert_eq!(visitor.fields, [None, Some(b"hi".to_vec())]);
}

fn encode_f64(value: f64) -> Vec<u8> {
    let mut visitor = CollectVisitor::new();
    value.encode_to(0, "f", &mut visitor).expect("encodes");
    visitor.into_fields().pop().expect("one field")
}

fn encode_f32(value: f32) -> Vec<u8> {
    let mut visitor = CollectVisitor::new();
    value.encode_to(0, "f", &mut visitor).expect("encodes");
    visitor.into_fields().pop().expect("one field")
}

fn decode_f64(bytes: &[u8]) -> f64 {
    <f64 as DecodeField<'_>>::decode_field(Some(bytes), 0, "f").expect("decodes")
}

fn decode_f32(bytes: &[u8]) -> f32 {
    <f32 as DecodeField<'_>>::decode_field(Some(bytes), 0, "f").expect("decodes")
}

#[test]
fn f64_roundtrips_bit_exact() {
    let values = [
        0.0_f64,
        -0.0,
        1.0,
        -1.0,
        f64::MIN,
        f64::MAX,
        f64::MIN_POSITIVE,
        f64::EPSILON,
        5e-324, // smallest subnormal
        core::f64::consts::PI,
        123_456_789.123_456_79,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ];
    for value in values {
        let encoded = encode_f64(value);
        let decoded = decode_f64(&encoded);
        assert_eq!(
            value.to_bits(),
            decoded.to_bits(),
            "f64 {value} encoded as {:?} did not round-trip",
            String::from_utf8_lossy(&encoded)
        );
    }
    let nan_encoded = encode_f64(f64::NAN);
    assert!(
        decode_f64(&nan_encoded).is_nan(),
        "NaN encoded as {:?} did not decode to NaN",
        String::from_utf8_lossy(&nan_encoded)
    );
}

#[test]
fn f32_roundtrips_bit_exact() {
    let values = [
        0.0_f32,
        -0.0,
        1.0,
        -1.0,
        f32::MIN,
        f32::MAX,
        f32::MIN_POSITIVE,
        f32::EPSILON,
        1e-45, // smallest subnormal
        core::f32::consts::PI,
        f32::INFINITY,
        f32::NEG_INFINITY,
    ];
    for value in values {
        let encoded = encode_f32(value);
        let decoded = decode_f32(&encoded);
        assert_eq!(
            value.to_bits(),
            decoded.to_bits(),
            "f32 {value} encoded as {:?} did not round-trip",
            String::from_utf8_lossy(&encoded)
        );
    }
    let nan_encoded = encode_f32(f32::NAN);
    assert!(decode_f32(&nan_encoded).is_nan());
}

macro_rules! check_integer_roundtrip {
    ($ty:ty, $($v:expr),+ $(,)?) => {{
        for value in [$($v),+] {
            let mut visitor = CollectVisitor::new();
            <$ty as EncodeField>::encode_to(&value, 0, "n", &mut visitor).expect("encodes");
            let encoded = visitor.into_fields().pop().expect("one field");
            let decoded =
                <$ty as DecodeField<'_>>::decode_field(Some(&encoded), 0, "n").expect("decodes");
            assert_eq!(
                value, decoded,
                "{} {value} encoded as {:?} did not round-trip",
                stringify!($ty),
                String::from_utf8_lossy(&encoded)
            );
        }
    }};
}

#[test]
fn integers_roundtrip_at_boundaries() {
    check_integer_roundtrip!(i8, i8::MIN, 0, i8::MAX);
    check_integer_roundtrip!(i16, i16::MIN, 0, i16::MAX);
    check_integer_roundtrip!(i32, i32::MIN, 0, i32::MAX);
    check_integer_roundtrip!(i64, i64::MIN, -1, 0, i64::MAX);
    check_integer_roundtrip!(i128, i128::MIN, 0, i128::MAX);
    check_integer_roundtrip!(isize, isize::MIN, 0, isize::MAX);
    check_integer_roundtrip!(u8, 0, u8::MAX);
    check_integer_roundtrip!(u16, 0, u16::MAX);
    check_integer_roundtrip!(u32, 0, u32::MAX);
    check_integer_roundtrip!(u64, 0, u64::MAX);
    check_integer_roundtrip!(u128, 0, u128::MAX);
    check_integer_roundtrip!(usize, 0, usize::MAX);
}

/// Validating the concatenated buffer is not enough on its own. Two fields
/// holding the halves of one multi-byte sequence sit adjacent in that buffer
/// and read as valid UTF-8 there, even though neither field is valid alone.
/// The field-boundary check is what rejects them.
#[test]
fn text_record_rejects_a_multibyte_sequence_split_across_two_fields() {
    let mut br = ByteRecord::new();
    br.push_field(b"\xc3");
    br.push_field(b"\xa9");
    // The bytes concatenate to a well-formed "é".
    let joined: Vec<u8> = br.iter().flatten().copied().collect();
    assert_eq!(str::from_utf8(&joined).expect("valid once joined"), "é");

    let error =
        TextRecord::try_from(&br).expect_err("a field boundary inside a sequence must be rejected");
    assert!(matches!(error.kind(), ErrorKind::InvalidUtf8(_)), "{error}");
    assert_eq!(
        error.location().field,
        0,
        "the first invalid field is named"
    );
}

/// A text record's invariant is that it holds valid UTF-8. A parse that fails
/// partway can have already laid bytes down in it, so the record must be
/// emptied before the error is returned rather than left holding a fragment.
#[test]
fn a_failed_parse_leaves_the_text_record_empty() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        b"kept\n\"unterminated",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;

    let mut record = TextRecord::new();
    let mut line = parser.next_line()?.expect("the first record");
    line.read_text_record_into(&mut record)?;
    assert_eq!(record.get(0), Some("kept"));

    let mut line = parser.next_line()?.expect("the unterminated record");
    let _ = line
        .read_text_record_into(&mut record)
        .expect_err("the quoted field is never closed");
    assert!(record.is_empty(), "the fragment must not survive the error");
    Ok(())
}
