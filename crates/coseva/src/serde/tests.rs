use serde::de::SeqAccess as _;
use serde::{Deserialize, Serialize};

use super::deserializer::{HeaderView, SeqDeserializer};
use super::struct_cache::StructCache;
use super::{
    deserialize_byte_record, deserialize_byte_record_owned, deserialize_record, serialize_to_record,
};
use crate::byte_record::ByteRecord;
use crate::record::Record;
use crate::span::{Source, Span, SpanSet};

#[derive(Debug, Deserialize, PartialEq)]
struct TestRecord {
    name: Option<String>,
    age: Option<i32>,
}

fn null_aware_byte_record(fields: &[Option<&str>]) -> ByteRecord {
    let mut record = ByteRecord::new();
    for field in fields {
        match field {
            Some(bytes) => record.push_field(bytes),
            None => record.push_null(),
        }
    }
    record
}

/// Build a one-field record over `b"1"`, optionally NULL-aware.
fn single_field_record<'a>(
    input: &'a [u8],
    scratch: &'a [u8],
    spans: &'a SpanSet,
    null_aware: bool,
) -> Record<'a> {
    Record::new(spans.resolved(input, scratch), 0..input.len(), 0).with_null_aware(null_aware)
}

#[derive(Debug, Deserialize, PartialEq)]
struct Kept {
    x: i32,
}

#[test]
fn byte_record_deserialize_treats_explicit_null_as_none() {
    let record = null_aware_byte_record(&[None, None]);
    let decoded: TestRecord = deserialize_byte_record_owned(&record, None).expect("decodes");
    assert_eq!(
        decoded,
        TestRecord {
            name: None,
            age: None,
        }
    );
}

#[test]
fn byte_record_deserialize_treats_present_empty_as_some_empty() {
    #[derive(Debug, Deserialize, PartialEq)]
    struct TextValue {
        name: Option<String>,
    }

    let record = null_aware_byte_record(&[Some(""), Some("")]);
    // "age" is Option<i32>, so a present-but-empty numeric field is a
    // parse error, not `None`, once the record is NULL-aware.
    let decoded: Result<TestRecord, _> = deserialize_byte_record_owned(&record, None);
    decoded.expect_err("present empty integer should fail");

    let record = null_aware_byte_record(&[Some("")]);
    let decoded: TextValue = deserialize_byte_record_owned(&record, None).expect("decodes");
    assert_eq!(
        decoded,
        TextValue {
            name: Some(String::new()),
        }
    );
}

#[test]
fn byte_record_deserialize_preserves_legacy_behavior_when_not_null_aware() {
    #[derive(Debug, Deserialize, PartialEq)]
    struct TextValue {
        name: Option<String>,
    }
    // Ordinary (non-NULL-aware) CSV: a present field is `Some(..)` even
    // when empty; absent-field-as-`None` legacy behavior is already
    // covered by the `tests/serde.rs` integration suite.
    let mut record = ByteRecord::new();
    record.push_field("");
    assert!(!record.null_aware());

    let decoded: TextValue = deserialize_byte_record_owned(&record, None).expect("decodes");
    assert_eq!(
        decoded,
        TextValue {
            name: Some(String::new()),
        }
    );
}

#[test]
fn record_view_deserialize_observes_null_awareness() {
    #[derive(Debug, Deserialize, PartialEq)]
    struct TextValue<'a> {
        #[serde(borrow)]
        name: Option<&'a str>,
    }

    let scratch = Vec::new();
    let spans = SpanSet::from([Span::from_valid_null(Source::Input, 0)]);
    let input = b"";
    let record = Record::new(spans.resolved(input, &scratch), 0..0, 0).with_null_aware(true);

    let decoded: TextValue<'_> = deserialize_record(&record).expect("decodes");
    assert_eq!(decoded, TextValue { name: None });
}

#[derive(Debug, Serialize)]
struct OutputRecord {
    name: Option<String>,
    age: Option<i32>,
}

#[test]
fn serialize_option_none_marks_explicit_null() {
    let value = OutputRecord {
        name: None,
        age: Some(7),
    };
    let mut record = ByteRecord::new();
    serialize_to_record(&value, &mut record, false).expect("serializes");

    assert_eq!(record.is_null(0), Some(true));
    assert_eq!(record.get(0), Some(b"".as_slice()));
    assert_eq!(record.is_null(1), Some(false));
    assert_eq!(record.get(1), Some(b"7".as_slice()));
}

#[test]
fn serialize_option_some_empty_string_is_not_null() {
    let value = OutputRecord {
        name: Some(String::new()),
        age: None,
    };
    let mut record = ByteRecord::new();
    serialize_to_record(&value, &mut record, false).expect("serializes");

    assert_eq!(record.is_null(0), Some(false));
    assert_eq!(record.get(0), Some(b"".as_slice()));
    assert_eq!(record.is_null(1), Some(true));
}

#[test]
fn sync_revalidates_after_invalid_header_becomes_valid() {
    // A header column that is not valid UTF-8 records an `invalid` marker.
    let mut cache = StructCache::new();
    let mut headers = ByteRecord::new();
    headers.push_field([0xffu8]);
    headers.push_field("b");
    cache.sync(Some(&headers));
    assert!(cache.invalid.is_some());

    // The header record is replaced by one with the same column count whose
    // first column is now valid UTF-8. The cache must rebuild rather than
    // treating the (empty) validated prefix as an unchanged match.
    let mut valid = ByteRecord::new();
    valid.push_field("a");
    valid.push_field("b");
    cache.sync(Some(&valid));
    assert!(
        cache.invalid.is_none(),
        "stale invalid marker survived a header change"
    );
    assert_eq!(cache.names.len(), 2);
}

#[test]
fn learned_skip_set_is_not_reused_across_distinct_structs() {
    // Two structs that share the same field-name list but differ in how
    // they treat unknown columns.
    #[derive(Debug, Deserialize)]
    struct Lax {
        x: i32,
    }
    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[expect(
        dead_code,
        reason = "the strict decode is expected to fail, so `x` is never read"
    )]
    struct Strict {
        x: i32,
    }

    let mut headers = ByteRecord::new();
    headers.push_field("x");
    headers.push_field("y");

    let mut record = ByteRecord::new();
    record.push_field("1");
    record.push_field("2");

    let mut cache = StructCache::new();
    cache.sync(Some(&headers));

    // Train the cache on the lax struct: column `y` is ignored, then the
    // record is committed exactly as the parser does on success.
    let lax: Lax =
        deserialize_byte_record(&record, Some(HeaderView::Full(&cache))).expect("lax decodes");
    assert_eq!(lax.x, 1);
    cache.commit();

    // The strict struct must still reject the unknown `y` column; the lax
    // struct's learned skip-set must not be applied to it.
    let strict: Result<Strict, _> =
        deserialize_byte_record(&record, Some(HeaderView::Full(&cache)));
    strict.expect_err("deny_unknown_fields struct must reject unknown column y");
}

/// `SeqDeserializer::size_hint` must report the fields still to be yielded.
#[test]
fn seq_size_hint_counts_the_remaining_fields() {
    let fields: Vec<(&[u8], bool)> = vec![(&b"a"[..], false), (&b"b"[..], false)];
    let seq = SeqDeserializer::new(fields.into_iter());
    assert_eq!(seq.size_hint(), Some(2));
}

/// A self-describing value asks the record deserializer for
/// `deserialize_any`, which presents the record as a sequence of fields.
#[derive(Debug, PartialEq)]
struct AnyFields(Vec<String>);

impl<'de> Deserialize<'de> for AnyFields {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldsVisitor;

        impl<'de> serde::de::Visitor<'de> for FieldsVisitor {
            type Value = AnyFields;

            fn expecting(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                formatter.write_str("a CSV record")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut fields = Vec::new();
                while let Some(field) = seq.next_element::<String>()? {
                    fields.push(field);
                }
                Ok(AnyFields(fields))
            }
        }

        deserializer.deserialize_any(FieldsVisitor)
    }
}

#[test]
fn deserialize_any_presents_the_record_as_a_sequence() {
    let input = b"ab";
    let scratch: Vec<u8> = Vec::new();
    let spans = SpanSet::from([
        Span::from_valid_range(Source::Input, 0..1, false),
        Span::from_valid_range(Source::Input, 1..2, false),
    ]);
    let record = single_field_record(input, &scratch, &spans, false);

    let any: AnyFields = deserialize_record(&record).expect("any deserializes as a sequence");
    assert_eq!(any, AnyFields(vec!["a".to_owned(), "b".to_owned()]));

    let fields: Vec<String> = deserialize_record(&record).expect("sequence deserializes");
    assert_eq!(fields, vec!["a".to_owned(), "b".to_owned()]);
}

/// Once a leading column has been learned as ignored, the map walk must
/// consume its field outright rather than offering it to the visitor.
#[test]
fn a_learned_leading_ignored_column_is_consumed_by_the_map_walk() {
    let mut headers = ByteRecord::new();
    headers.push_field("extra");
    headers.push_field("x");

    let mut data = ByteRecord::new();
    data.push_field("skipped");
    data.push_field("1");

    let mut cache = StructCache::new();
    cache.sync(Some(&headers));

    // The first record observes that column 0 is never read.
    let observed: Kept =
        deserialize_byte_record(&data, Some(HeaderView::Full(&cache))).expect("Kept decodes");
    assert_eq!(observed, Kept { x: 1 });
    cache.commit();

    // The second record skips it before any key is reported.
    let skipped: Kept =
        deserialize_byte_record(&data, Some(HeaderView::Full(&cache))).expect("Kept decodes");
    assert_eq!(skipped, Kept { x: 1 });
}

/// A top-level `Option` is `None` only for a record with no fields at all;
/// a record with fields deserializes as `Some`.
#[test]
fn a_top_level_option_distinguishes_an_empty_record() {
    let scratch: Vec<u8> = Vec::new();

    let empty_spans = SpanSet::new();
    let empty = Record::new(empty_spans.resolved(b"", &scratch), 0..0, 0);
    let none: Option<Vec<String>> = deserialize_record(&empty).expect("an empty record");
    assert_eq!(none, None);

    let input = b"a";
    let spans = SpanSet::from([Span::from_valid_range(Source::Input, 0..1, false)]);
    let present = single_field_record(input, &scratch, &spans, false);
    let some: Option<Vec<String>> = deserialize_record(&present).expect("a present record");
    assert_eq!(some, Some(vec!["a".to_owned()]));
}

/// A header wide enough to need the wide bitset, named `c000..cNNN` the way the
/// width benchmarks name their columns.
fn wide_headers(columns: usize) -> ByteRecord {
    let mut headers = ByteRecord::new();
    for column in 0..columns {
        headers.push_field(format!("c{column:03}"));
    }
    headers
}

/// The wide bitset is allocated only once a header passes the single 64-bit
/// word, so an ordinary header keeps the allocation-free fast path.
#[test]
fn the_wide_bitset_is_allocated_only_past_the_single_word_edge() {
    let mut cache = StructCache::new();

    // Exactly 64 columns fit the single word and allocate no wide storage.
    cache.sync(Some(&wide_headers(64)));
    assert!(
        cache.wide_ignored().is_empty(),
        "64 columns must stay on the single-word fast path"
    );

    // One more column needs exactly one wide word for column 64.
    cache.sync(Some(&wide_headers(65)));
    assert_eq!(
        cache.wide_ignored().len(),
        1,
        "column 64 needs a single wide word"
    );

    // Columns 64..200 need three words: ceil(136 / 64).
    cache.sync(Some(&wide_headers(200)));
    assert_eq!(
        cache.wide_ignored().len(),
        3,
        "columns 64..200 span three wide words"
    );

    // Dropping back to a narrow header releases the wide storage entirely.
    cache.sync(Some(&wide_headers(10)));
    assert!(
        cache.wide_ignored().is_empty(),
        "a narrow header must not retain wide storage"
    );
}

/// Ignored columns must be learned at every index of a wide header, including
/// the last column below the word edge, the first two above it, and the far
/// end of the widest supported header.
#[test]
fn ignored_columns_are_learned_at_every_index_of_a_wide_header() {
    use super::struct_cache::{mask_contains, wide_contains};
    use std::sync::atomic::Ordering::Relaxed;

    let mut cache = StructCache::new();
    cache.sync(Some(&wide_headers(200)));

    // A struct identity the cache has not seen: begin_struct records it and
    // reports it is not yet learned, so the record is observed. The same
    // `name` and `fields` values are reused below so the cache recognises the
    // identity by address rather than resetting.
    let name = "Wide";
    let fields: &'static [&'static str] = &["c063", "c064", "c065", "c199"];
    assert!(
        !cache.begin_struct(name, fields),
        "an unseen struct must be observed, not skipped"
    );

    // The visitor keeps the four boundary columns and ignores every other one.
    let kept = [63_usize, 64, 65, 199];
    for column in 0..200 {
        if !kept.contains(&column) {
            cache.note_ignored(column);
        }
    }
    cache.commit();

    // Every ignored column is now learned and every kept column is not, across
    // the single-word edge (63/64), just past it (65), and the far end (199).
    let mask = cache.ignored_mask();
    let wide = cache.wide_ignored();
    for column in 0..200 {
        let learned = mask_contains(mask, column) || wide_contains(wide, column);
        assert_eq!(
            learned,
            !kept.contains(&column),
            "column {column} learned state is wrong"
        );
    }

    // A second begin_struct with the same identity now reports the learned set
    // is ready, so later records skip straight to the kept columns.
    assert!(
        cache.begin_struct(name, fields),
        "a learned struct must report its skip-set is ready"
    );

    // Switching to a different struct identity clears the learned set so the
    // new struct is observed from scratch rather than reusing wide skips.
    let other = "Other";
    assert!(
        !cache.begin_struct(other, fields),
        "a distinct struct must relearn its own ignored columns"
    );
    let mask = cache.ignored_mask();
    let wide = cache.wide_ignored();
    assert_eq!(mask, 0, "the low word must be cleared for the new struct");
    assert!(
        wide.iter().all(|word| word.load(Relaxed) == 0),
        "the wide words must be cleared for the new struct"
    );
}

#[test]
fn test_field_deserializer_all_types_and_errors() {
    #[derive(Debug, Deserialize, PartialEq)]
    struct AllTypes<'a> {
        #[serde(borrow)]
        bytes_borrowed: &'a [u8],
        str_val: String,
        ch: char,
        flag1: bool,
        flag2: bool,
    }

    let mut record = ByteRecord::new();
    record.push_field(b"foo");
    record.push_field("bar");
    record.push_field("z");
    record.push_field("true");
    record.push_field("0");

    let val: AllTypes<'_> = deserialize_byte_record(&record, None).unwrap();
    assert_eq!(val.bytes_borrowed, b"foo");
    assert_eq!(val.str_val, "bar");
    assert_eq!(val.ch, 'z');
    assert!(val.flag1);
    assert!(!val.flag2);

    // Bool error
    #[derive(Debug, Deserialize)]
    struct BadBool {
        _b: bool,
    }
    let mut rec_bad_bool = ByteRecord::new();
    rec_bad_bool.push_field("not_bool");
    assert!(deserialize_byte_record_owned::<BadBool>(&rec_bad_bool, None).is_err());

    // Char errors (empty and multiple)
    #[derive(Debug, Deserialize)]
    struct BadChar {
        _c: char,
    }
    let mut rec_empty = ByteRecord::new();
    rec_empty.push_field("");
    assert!(deserialize_byte_record_owned::<BadChar>(&rec_empty, None).is_err());

    let mut rec_multi = ByteRecord::new();
    rec_multi.push_field("abc");
    assert!(deserialize_byte_record_owned::<BadChar>(&rec_multi, None).is_err());

    // Single field unsupported structures
    #[derive(Debug, Deserialize)]
    struct NestedSeq {
        _seq: Vec<i32>,
    }
    let mut rec_field = ByteRecord::new();
    rec_field.push_field("1,2,3");
    assert!(deserialize_byte_record_owned::<NestedSeq>(&rec_field, None).is_err());

    #[derive(Debug, Deserialize)]
    struct NestedTuple {
        _tup: (i32, i32),
    }
    assert!(deserialize_byte_record_owned::<NestedTuple>(&rec_field, None).is_err());

    #[derive(Debug, Deserialize)]
    struct NestedTupleStruct(#[expect(dead_code, reason = "test struct")] (i32, i32));
    assert!(deserialize_byte_record_owned::<NestedTupleStruct>(&rec_field, None).is_err());

    #[derive(Debug, Deserialize)]
    struct NestedMap {
        _map: std::collections::HashMap<String, String>,
    }
    assert!(deserialize_byte_record_owned::<NestedMap>(&rec_field, None).is_err());

    #[derive(Debug, Deserialize)]
    struct Inner {
        _a: i32,
    }
    #[derive(Debug, Deserialize)]
    struct Outer {
        _inner: Inner,
    }
    assert!(deserialize_byte_record_owned::<Outer>(&rec_field, None).is_err());

    // Enum variants (newtype, tuple, struct variants return error)
    #[derive(Debug, Deserialize)]
    #[expect(dead_code, reason = "test enum variants")]
    enum ComplexEnum {
        Newtype(i32),
        Tuple(i32, i32),
        Struct { x: i32 },
    }
    #[derive(Debug, Deserialize)]
    struct EnumHolder {
        _e: ComplexEnum,
    }
    let mut rec_enum = ByteRecord::new();
    rec_enum.push_field("Newtype");
    assert!(deserialize_byte_record_owned::<EnumHolder>(&rec_enum, None).is_err());
    let mut rec_enum2 = ByteRecord::new();
    rec_enum2.push_field("Tuple");
    assert!(deserialize_byte_record_owned::<EnumHolder>(&rec_enum2, None).is_err());
    let mut rec_enum3 = ByteRecord::new();
    rec_enum3.push_field("Struct");
    assert!(deserialize_byte_record_owned::<EnumHolder>(&rec_enum3, None).is_err());

    // Unit struct and newtype struct on record level
    #[derive(Debug, Deserialize)]
    struct UnitType;
    let _unit: UnitType = deserialize_byte_record_owned(&rec_empty, None).unwrap();

    #[derive(Debug, Deserialize)]
    struct NewtypeRecord(String);
    let mut rec_one = ByteRecord::new();
    rec_one.push_field("hello");
    let nt: NewtypeRecord = deserialize_byte_record_owned(&rec_one, None).unwrap();
    assert_eq!(nt.0, "hello");

    // MapDeserializer without headers error
    assert!(
        deserialize_byte_record_owned::<std::collections::HashMap<String, String>>(&rec_one, None)
            .is_err()
    );
}
