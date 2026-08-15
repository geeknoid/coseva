//! Compile-time guarantees about the traits the public types implement.
//!
//! These assertions fail at compile time, so an accidental loss of `Send`,
//! `Sync`, or `Unpin` on a public type is caught before it reaches users.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(feature = "std")]

#[cfg(feature = "compact_str")]
use compact_str::CompactString;
use coseva::config::{EmitOptions, FormatOptions, Limits, ParseOptions};
use coseva::encoding::{CollectVisitor, EncodeField, EncodeVisitor};
use coseva::format::Csv;
use coseva::{
    ByteRecord, Column, Error, ErrorKind, FieldProjection, IoParser, Location, MatchKind,
    Predicate, PushParser, Record, SliceParser, TextRecord,
};
use coseva::{IntoInnerError, IoEmitter, VecEmitter};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

const fn assert_send_sync<T: Send + Sync>() {}
const fn assert_unpin<T: Unpin>() {}

#[test]
fn public_types_are_send_and_sync() {
    assert_send_sync::<ByteRecord>();
    assert_send_sync::<TextRecord>();
    assert_send_sync::<Record<'_>>();
    assert_send_sync::<Error>();
    assert_send_sync::<ErrorKind>();
    assert_send_sync::<Location>();
    assert_send_sync::<Column>();
    assert_send_sync::<MatchKind>();
    assert_send_sync::<Predicate>();
    assert_send_sync::<FieldProjection>();
    assert_send_sync::<FormatOptions>();
    assert_send_sync::<ParseOptions>();
    assert_send_sync::<EmitOptions>();
    assert_send_sync::<Limits>();
    assert_send_sync::<SliceParser<'_>>();
    assert_send_sync::<IoParser<std::io::Empty>>();
    assert_send_sync::<PushParser>();
    assert_send_sync::<VecEmitter>();
    assert_send_sync::<IoEmitter<Vec<u8>>>();
    assert_send_sync::<IntoInnerError<Vec<u8>>>();
}

#[test]
fn public_types_are_unpin() {
    assert_unpin::<ByteRecord>();
    assert_unpin::<TextRecord>();
    assert_unpin::<Error>();
    assert_unpin::<SliceParser<'_>>();
    assert_unpin::<IoParser<std::io::Empty>>();
    assert_unpin::<PushParser>();
}

#[test]
fn options_compare_by_value() {
    assert_eq!(ParseOptions::new(), ParseOptions::default());
    assert_ne!(
        ParseOptions::new(),
        ParseOptions::new().buffer_capacity(1024)
    );

    assert_eq!(EmitOptions::new(), EmitOptions::default());
    assert_ne!(EmitOptions::new(), EmitOptions::new().buffer_capacity(1024));
}

#[test]
fn record_equality_ignores_parser_provenance() {
    let mut parser =
        SliceParser::<Csv>::new(b"a,b\n1,2\n3,4\n", ParseOptions::new()).expect("parser");
    let mut parsed = ByteRecord::new();

    let mut line = parser.next_line().expect("parses").expect("record");
    line.read_byte_record_into(&mut parsed).expect("reads");

    let literal = ByteRecord::from(vec![b"1".to_vec(), b"2".to_vec()]);

    assert_eq!(parsed, literal, "identical fields compare equal");
    assert_ne!(
        parsed.byte_range(),
        literal.byte_range(),
        "the records really do carry different provenance"
    );

    assert_eq!(hash_of(&parsed), hash_of(&literal));
}

#[test]
fn records_collect_and_extend() {
    let mut bytes: ByteRecord = [b"a".as_slice(), b"bb".as_slice()].into_iter().collect();
    bytes.extend([b"ccc".as_slice()]);
    assert_eq!(bytes.len(), 3);
    assert_eq!(bytes.get(2), Some(&b"ccc"[..]));

    let mut text: TextRecord = ["a", "bb"].into_iter().collect();
    text.extend(["ccc"]);
    assert_eq!(text.len(), 3);
    assert_eq!(text.get(2), Some("ccc"));

    assert_eq!(ByteRecord::from(&text), bytes);
}

#[test]
fn location_displays_position() {
    let location = Location {
        byte: 7,
        line: 2,
        record: 1,
        field: 0,
    };

    assert_eq!(location.to_string(), "byte 7, line 2, record 1, field 0");
    assert_eq!(Location::UNKNOWN.to_string(), "unknown location");
}

#[test]
fn unterminated_record_errors_display_append_context() {
    assert_eq!(
        ErrorKind::UnterminatedRecord.to_string(),
        "cannot append: file does not end with a record terminator"
    );
}

#[test]
fn every_record_type_iterates_the_same_way() {
    let mut parser = SliceParser::<Csv>::new(b"a,b\n1,2\n", ParseOptions::new()).expect("parser");
    let mut line = parser.next_line().expect("parses").expect("record");
    let record = line.record().expect("decodes");

    let expected = [&b"1"[..], &b"2"[..]];
    assert_eq!(record.iter().collect::<Vec<_>>(), expected);
    assert_eq!((&record).into_iter().collect::<Vec<_>>(), expected);

    let bytes = ByteRecord::from(vec![b"1".to_vec(), b"2".to_vec()]);
    assert_eq!(bytes.iter().collect::<Vec<_>>(), expected);
    assert_eq!((&bytes).into_iter().collect::<Vec<_>>(), expected);

    let text = TextRecord::try_from(&bytes).expect("utf-8");
    assert_eq!(text.iter().collect::<Vec<_>>(), ["1", "2"]);
    assert_eq!((&text).into_iter().collect::<Vec<_>>(), ["1", "2"]);
}

#[derive(Debug, Eq, PartialEq)]
enum Visit {
    Field(usize, &'static str, Vec<u8>),
    Null(usize, &'static str),
}

#[derive(Default)]
struct InspectVisitor {
    visits: Vec<Visit>,
}

impl EncodeVisitor for InspectVisitor {
    fn visit_field(&mut self, index: usize, name: &'static str, bytes: &[u8]) -> Result<(), Error> {
        self.visits.push(Visit::Field(index, name, bytes.to_vec()));
        Ok(())
    }

    fn visit_null(&mut self, index: usize, name: &'static str) -> Result<(), Error> {
        self.visits.push(Visit::Null(index, name));
        Ok(())
    }
}

#[test]
fn every_direct_encoding_forwards_the_exact_field_identity() {
    let mut visitor = InspectVisitor::default();
    <[u8] as EncodeField>::encode_to(b"slice", 10, "slice", &mut visitor).expect("slice encodes");
    <&[u8] as EncodeField>::encode_to(&b"ref".as_slice(), 11, "ref", &mut visitor)
        .expect("slice reference encodes");
    Vec::<u8>::from(b"vec".as_slice())
        .encode_to(12, "vec", &mut visitor)
        .expect("vector encodes");
    <str as EncodeField>::encode_to("str", 13, "str", &mut visitor).expect("str encodes");
    <&str as EncodeField>::encode_to(&"borrowed", 14, "borrowed", &mut visitor)
        .expect("str reference encodes");
    String::from("owned")
        .encode_to(15, "owned", &mut visitor)
        .expect("string encodes");
    true.encode_to(16, "bool", &mut visitor)
        .expect("boolean encodes");
    Some(false)
        .encode_to(17, "some", &mut visitor)
        .expect("present option encodes");
    Option::<bool>::None
        .encode_to(18, "none", &mut visitor)
        .expect("absent option encodes");
    #[cfg(feature = "compact_str")]
    CompactString::from("compact")
        .encode_to(19, "compact", &mut visitor)
        .expect("compact string encodes");

    let expected = vec![
        Visit::Field(10, "slice", b"slice".to_vec()),
        Visit::Field(11, "ref", b"ref".to_vec()),
        Visit::Field(12, "vec", b"vec".to_vec()),
        Visit::Field(13, "str", b"str".to_vec()),
        Visit::Field(14, "borrowed", b"borrowed".to_vec()),
        Visit::Field(15, "owned", b"owned".to_vec()),
        Visit::Field(16, "bool", b"true".to_vec()),
        Visit::Field(17, "some", b"false".to_vec()),
        Visit::Null(18, "none"),
        #[cfg(feature = "compact_str")]
        Visit::Field(19, "compact", b"compact".to_vec()),
    ];
    assert_eq!(visitor.visits, expected);
}

#[derive(Default)]
struct DefaultNullVisitor {
    visit: Option<(usize, &'static str, Vec<u8>)>,
}

impl EncodeVisitor for DefaultNullVisitor {
    fn visit_field(&mut self, index: usize, name: &'static str, bytes: &[u8]) -> Result<(), Error> {
        self.visit = Some((index, name, bytes.to_vec()));
        Ok(())
    }
}

#[test]
fn default_null_forwarding_preserves_identity_and_uses_empty_bytes() {
    let mut visitor = DefaultNullVisitor::default();
    visitor.visit_null(7, "missing").expect("accepted");
    assert_eq!(visitor.visit, Some((7, "missing", Vec::new())));
}

#[test]
fn collect_visitor_new_is_empty() {
    const EMPTY: CollectVisitor = CollectVisitor::new();
    let visitor = EMPTY;
    assert!(visitor.is_empty());
    assert_eq!(visitor.len(), 0);
}

fn hash_of<T: Hash>(value: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}
