//! Tests for reusable field projections.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::cell::Cell;
use std::error::Error as StdError;
use std::rc::Rc;

use coseva::config::{FormatOptions, Headers, ParseOptions};
use coseva::{ByteRecord, ErrorKind, FieldProjection, SliceParser, TextRecord};

#[test]
fn positional_and_named_projections_preserve_order() -> Result<(), Box<dyn StdError>> {
    let headers = ByteRecord::from(vec![
        b"city".to_vec(),
        b"state".to_vec(),
        b"population".to_vec(),
    ]);
    let projection = FieldProjection::from_headers(&headers, ["population", "city"])?;
    assert_eq!(projection.indices(), [2, 0]);
    assert_eq!(projection.len(), 2);
    assert!(!projection.is_empty());

    let record = ByteRecord::from(vec![b"Boston".to_vec(), b"MA".to_vec(), b"650706".to_vec()]);
    assert_eq!(
        record.project(&projection).collect::<Vec<_>>(),
        [Some(b"650706".as_slice()), Some(b"Boston".as_slice())],
    );

    let mut reader = SliceParser::with_options(
        b"Boston,MA,650706\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing borrowed row");
    let borrowed = line.record()?;
    assert_eq!(
        borrowed.project(&projection).collect::<Vec<_>>(),
        [Some(b"650706".as_slice()), Some(b"Boston".as_slice())],
    );

    let positional = FieldProjection::new(vec![1, 1]);
    assert_eq!(
        record.project(&positional).collect::<Vec<_>>(),
        [Some(b"MA".as_slice()); 2],
        "positional projections may repeat a source field",
    );
    Ok(())
}

#[test]
fn named_resolution_rejects_missing_and_duplicate_headers() {
    let duplicates = ByteRecord::from(vec![b"name".to_vec(), b"name".to_vec()]);
    let error = FieldProjection::from_headers(&duplicates, ["name"])
        .expect_err("a repeated header is ambiguous");
    assert_eq!(error.kind(), ErrorKind::DuplicateHeader);
    assert_eq!(error.to_string(), r#"duplicate projected header "name""#);

    let headers = ByteRecord::from(vec![b"city".to_vec()]);
    let error =
        FieldProjection::from_headers(&headers, ["state"]).expect_err("the header is absent");
    assert_eq!(error.kind(), ErrorKind::MissingHeader);
    assert_eq!(error.to_string(), r#"missing projected header "state""#);
}

#[test]
fn sparse_resolution_preserves_requested_name_error_order() {
    let headers = ByteRecord::from(vec![b"dup".to_vec(), b"dup".to_vec()]);
    let error = FieldProjection::from_headers(&headers, ["missing", "dup"])
        .expect_err("the first requested name is missing");
    assert_eq!(error.kind(), ErrorKind::MissingHeader);
    assert_eq!(error.to_string(), r#"missing projected header "missing""#);
}

#[test]
fn headers_may_come_from_any_byte_sequence() -> Result<(), Box<dyn StdError>> {
    let text = TextRecord::from(vec!["city".to_owned(), "population".to_owned()]);
    let projection = FieldProjection::from_headers(&text, ["population"])?;
    assert_eq!(projection.indices(), [1]);

    let record = TextRecord::from(vec!["Boston".to_owned(), "650706".to_owned()]);
    assert_eq!(
        record.project(&projection).collect::<Vec<_>>(),
        [Some("650706")]
    );

    let from_names = FieldProjection::from_headers(["city", "population"], [b"city"])?;
    assert_eq!(from_names.indices(), [0]);
    Ok(())
}

struct UnknownSize<I>(I);

impl<I: Iterator> Iterator for UnknownSize<I> {
    type Item = I::Item;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

struct Counted<I> {
    inner: I,
    yielded: Rc<Cell<usize>>,
}

impl<I: Iterator> Iterator for Counted<I> {
    type Item = I::Item;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.inner.next();
        if item.is_some() {
            self.yielded.set(self.yielded.get() + 1);
        }
        item
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

#[test]
fn bounded_sparse_resolution_handles_zero_through_three_names() -> Result<(), Box<dyn StdError>> {
    let headers = ["a", "b", "c", "d"];
    assert!(FieldProjection::from_headers(headers, [] as [&str; 0])?.is_empty());
    assert_eq!(
        FieldProjection::from_headers(headers, ["d"])?.indices(),
        [3]
    );
    assert_eq!(
        FieldProjection::from_headers(headers, ["d", "a"])?.indices(),
        [3, 0]
    );
    assert_eq!(
        FieldProjection::from_headers(headers, ["d", "b", "a"])?.indices(),
        [3, 1, 0]
    );

    let record = ByteRecord::from(headers.map(|header| header.as_bytes().to_vec()).to_vec());
    assert_eq!(
        FieldProjection::from_headers(&record, ["c", "a"])?.indices(),
        [2, 0]
    );
    Ok(())
}

#[test]
fn sparse_resolution_preserves_fallback_errors_and_consumes_successful_scans() {
    let headers = ["a", "b", "c"];
    let unknown = || UnknownSize(headers.into_iter());
    assert_eq!(
        FieldProjection::from_headers(unknown(), ["c"])
            .expect("unknown-size headers remain resolvable")
            .indices(),
        [2]
    );
    assert_eq!(
        FieldProjection::from_headers(unknown(), ["c", "a"])
            .expect("unknown-size headers remain resolvable")
            .indices(),
        [2, 0]
    );
    assert_eq!(
        FieldProjection::from_headers(unknown(), UnknownSize(["c", "b", "a"].into_iter()))
            .expect("unknown-size names remain resolvable")
            .indices(),
        [2, 1, 0]
    );

    for names in [["missing", "dup"], ["dup", "missing"]] {
        let error = FieldProjection::from_headers(["dup", "dup"], UnknownSize(names.into_iter()))
            .expect_err("fallback resolution must retain requested-name error order");
        assert_eq!(
            error.kind(),
            if names[0] == "missing" {
                ErrorKind::MissingHeader
            } else {
                ErrorKind::DuplicateHeader
            }
        );
    }

    let yielded = Rc::new(Cell::new(0));
    let counted = Counted {
        inner: headers.into_iter(),
        yielded: Rc::clone(&yielded),
    };
    assert_eq!(
        FieldProjection::from_headers(counted, ["c", "a"])
            .expect("bounded scan succeeds")
            .indices(),
        [2, 0]
    );
    assert_eq!(yielded.get(), headers.len());

    let yielded_names = Rc::new(Cell::new(0));
    let counted_names = Counted {
        inner: ["c", "a"].into_iter(),
        yielded: Rc::clone(&yielded_names),
    };
    assert_eq!(
        FieldProjection::from_headers(headers, counted_names)
            .expect("bounded names are consumed once")
            .indices(),
        [2, 0]
    );
    assert_eq!(yielded_names.get(), 2);

    assert_eq!(
        FieldProjection::from_headers(["a", "b"], ["a", "a"])
            .expect("repeated requested names may select one unique header twice")
            .indices(),
        [0, 0]
    );
    assert_eq!(
        FieldProjection::from_headers(["a", "a", "b"], ["a", "b"])
            .expect_err("a duplicated header remains ambiguous")
            .kind(),
        ErrorKind::DuplicateHeader
    );
    assert_eq!(
        FieldProjection::from_headers(["a", "b"], ["a", "missing"])
            .expect_err("a missing header remains an error")
            .kind(),
        ErrorKind::MissingHeader
    );
}

#[test]
fn short_records_yield_none_without_shifting_later_fields() {
    let projection = FieldProjection::new(vec![2, 0]);
    let record = ByteRecord::from(vec![b"only".to_vec()]);
    assert_eq!(
        record.project(&projection).collect::<Vec<_>>(),
        [None, Some(b"only".as_slice())],
    );

    let text = TextRecord::from(vec!["only".to_owned()]);
    assert_eq!(
        text.project(&projection).collect::<Vec<_>>(),
        [None, Some("only")]
    );
}

#[test]
fn projected_fields_report_an_exact_size_and_iterate_from_both_ends() {
    let projection = FieldProjection::new(vec![0, 1, 2]);
    let record = ByteRecord::from(vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);

    let mut fields = record.project(&projection);
    assert_eq!(fields.len(), 3);
    assert_eq!(fields.next(), Some(Some(b"a".as_slice())));
    assert_eq!(fields.next_back(), Some(Some(b"c".as_slice())));
    assert_eq!(fields.len(), 1);
    assert_eq!(fields.next(), Some(Some(b"b".as_slice())));
    assert_eq!(fields.next(), None);

    let empty = FieldProjection::default();
    assert!(empty.is_empty());
    assert_eq!(record.project(&empty).count(), 0);
}

#[test]
fn projected_lending_fields_outlive_the_record_view() -> Result<(), Box<dyn StdError>> {
    let projection = FieldProjection::new(vec![1]);
    let mut reader = SliceParser::with_options(
        b"Boston,\"M\"\"A\"\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing row");
    let fields: Vec<_> = {
        let record = line.record()?;
        // The record view is dropped here, but the fields borrow the parsed
        // input, so they stay usable.
        record.project(&projection).collect()
    };
    assert_eq!(fields, [Some(b"M\"A".as_slice())]);
    Ok(())
}

// ── constructing projections from headers ───────────────────────────────────────

/// Building a projection from a header record that repeats a target name is
/// ambiguous and must be rejected.
#[test]
fn field_projection_from_headers_rejects_duplicate() {
    let headers = ByteRecord::from(vec![b"city".to_vec(), b"city".to_vec(), b"pop".to_vec()]);
    let err = FieldProjection::from_headers(&headers, ["city"])
        .expect_err("duplicate header should fail");
    assert_eq!(err.kind(), ErrorKind::DuplicateHeader);
}

// ── double-ended iteration and size hints ───────────────────────────────────────

/// `ProjectedFields` supports `DoubleEndedIterator`, walking from the back of
/// the projection as well as the front.
#[test]
fn projected_fields_double_ended_iterator() -> Result<(), Box<dyn StdError>> {
    let headers = ByteRecord::from(vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    let projection = FieldProjection::from_headers(&headers, ["a", "b", "c"])?;
    let record = ByteRecord::from(vec![b"1".to_vec(), b"2".to_vec(), b"3".to_vec()]);
    let fields: Vec<_> = record.project(&projection).collect();
    assert_eq!(
        fields,
        [
            Some(b"1".as_slice()),
            Some(b"2".as_slice()),
            Some(b"3".as_slice())
        ]
    );

    let mut iter = record.project(&projection);
    assert_eq!(iter.next_back(), Some(Some(b"3".as_slice())));
    assert_eq!(iter.next_back(), Some(Some(b"2".as_slice())));
    assert_eq!(iter.next_back(), Some(Some(b"1".as_slice())));
    assert_eq!(iter.next_back(), None);
    Ok(())
}

/// `ProjectedTextFields` reports an exact size hint and also supports
/// `DoubleEndedIterator`.
#[test]
fn projected_text_fields_size_hint_and_double_ended() -> Result<(), Box<dyn StdError>> {
    let headers = ByteRecord::from(vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    let projection = FieldProjection::from_headers(&headers, ["a", "b", "c"])?;

    let mut text = TextRecord::new();
    text.push_field("one");
    text.push_field("two");
    text.push_field("three");

    let iter = text.project(&projection);
    let (low, high) = iter.size_hint();
    assert_eq!(low, 3);
    assert_eq!(high, Some(3));

    let mut iter = text.project(&projection);
    assert_eq!(iter.next_back(), Some(Some("three")));
    assert_eq!(iter.next_back(), Some(Some("two")));
    assert_eq!(iter.next_back(), Some(Some("one")));
    assert_eq!(iter.next_back(), None);
    Ok(())
}

/// `ProjectedFields` reports an exact size hint matching the projection width.
#[test]
fn projected_fields_size_hint() -> Result<(), Box<dyn StdError>> {
    let headers = ByteRecord::from(vec![b"a".to_vec(), b"b".to_vec()]);
    let projection = FieldProjection::from_headers(&headers, ["a", "b"])?;
    let record = ByteRecord::from(vec![b"1".to_vec(), b"2".to_vec()]);
    let iter = record.project(&projection);
    let (low, high) = iter.size_hint();
    assert_eq!(low, 2);
    assert_eq!(high, Some(2));
    Ok(())
}

/// A projection index beyond the end of the record yields `None` for that
/// slot rather than panicking or shifting later fields.
#[test]
fn projected_fields_out_of_bounds_yields_none() -> Result<(), Box<dyn StdError>> {
    let input = b"a,b\n1,2\n";
    let mut parser = SliceParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = parser.next_line()?.expect("record");
    let record = line.record()?;
    // Request field index 5 on a 2-field record.
    let proj = FieldProjection::new(Box::new([5usize]) as Box<[usize]>);
    let mut fields = record.project(&proj);
    assert_eq!(fields.next(), Some(None));
    Ok(())
}

// ── wide indexed resolution ─────────────────────────────────────────────────────

/// A header record over `cNNN` names, the fixed-width column naming the width
/// benchmarks use, so a column keeps its name at every width.
fn wide_header(width: usize) -> ByteRecord {
    ByteRecord::from(
        (0..width)
            .map(|column| format!("c{column:03}").into_bytes())
            .collect::<Vec<_>>(),
    )
}

/// A projection that names every column of a wide header must resolve through
/// the indexed path — since `width × width` passes the scan threshold — and
/// still map each name to its own column, whatever order the names arrive in.
#[test]
fn wide_named_projection_resolves_every_column() -> Result<(), Box<dyn StdError>> {
    for width in [64_usize, 100, 200] {
        let headers = wide_header(width);
        let names: Vec<String> = (0..width).map(|column| format!("c{column:03}")).collect();

        // Header order maps each name to its own position.
        let identity = FieldProjection::from_headers(&headers, names.iter().map(String::as_str))?;
        assert_eq!(
            identity.indices(),
            (0..width).collect::<Vec<_>>().as_slice()
        );

        // Reversed order maps each name to a different position, so nothing
        // resolves by coincidence of index.
        let reversed: Vec<&str> = names.iter().rev().map(String::as_str).collect();
        let reversed = FieldProjection::from_headers(&headers, reversed)?;
        assert_eq!(
            reversed.indices(),
            (0..width).rev().collect::<Vec<_>>().as_slice()
        );

        // The first and last columns alone, the sparse shape that stays on the
        // scan even against a wide header.
        let ends = [names[0].as_str(), names[width - 1].as_str()];
        let ends = FieldProjection::from_headers(&headers, ends)?;
        assert_eq!(ends.indices(), [0, width - 1]);
    }
    Ok(())
}

/// The wide indexed path rejects duplicate and missing headers exactly as the
/// narrow scan does, so widening a header changes no error.
#[test]
fn wide_named_projection_rejects_duplicate_and_missing() {
    // A duplicated header makes the name that reaches it ambiguous. Column 50
    // is renamed to repeat column 49's name, and every column is requested, so
    // the indexed path (100 × 100) resolves `c049` to two columns.
    let mut header_names: Vec<String> = (0..100).map(|column| format!("c{column:03}")).collect();
    header_names[50] = "c049".to_owned();
    let dup_headers = ByteRecord::from(
        header_names
            .iter()
            .map(|name| name.clone().into_bytes())
            .collect::<Vec<_>>(),
    );
    let all: Vec<String> = (0..100).map(|column| format!("c{column:03}")).collect();
    let error = FieldProjection::from_headers(&dup_headers, all.iter().map(String::as_str))
        .expect_err("a duplicate header is ambiguous on the wide path");
    assert_eq!(error.kind(), ErrorKind::DuplicateHeader);

    // A name no column carries is missing on the indexed path too.
    let clean = wide_header(100);
    let mut names: Vec<String> = (0..100).map(|column| format!("c{column:03}")).collect();
    names[10] = "absent".to_owned();
    let error = FieldProjection::from_headers(&clean, names.iter().map(String::as_str))
        .expect_err("an absent name is missing on the wide path");
    assert_eq!(error.kind(), ErrorKind::MissingHeader);
}
