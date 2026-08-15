//! Exhaustive byte-order-mark handling matrix across every front end, scanning
//! path, read policy, and buffer/chunk seam.
//!
//! `ReadBom::Reject` must report [`ErrorKind::RejectedBom`] consistently from
//! [`SliceParser`], [`IoParser`], and [`PushParser`]. The eager general path
//! must report the mark before downstream syntax errors, including when a
//! one-byte buffer splits the mark across three refills.
//!
//! For every case the three front ends must produce the *same* [`Outcome`]:
//! `Detect` strips the mark and parses cleanly, `Preserve` keeps it as leading
//! field bytes, and `Reject` refuses it identically. The [`SliceParser`] result
//! with a full buffer is the oracle every streaming configuration must match.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::io::Cursor;

use coseva::config::{
    BlankRecords, FormatOptions, Headers, ParseOptions, ReadBom, Recovery, Syntax, Whitespace,
};
use coseva::{ByteRecord, Error, ErrorKind, IoParser, PushParser, SliceParser};

const BOM: &[u8] = b"\xEF\xBB\xBF";

/// Records reduced to owned byte fields, so results compare across front ends.
type Rows = Vec<Vec<Vec<u8>>>;

/// A parse outcome reduced to what every front end must agree on: the recovered
/// rows, or the error kind. The kind alone is compared because a rejected mark
/// is refused at construction by [`SliceParser`] but at read time by the
/// streaming parsers — the same kind, a different stage.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Parsed(Rows),
    Failed(ErrorKind),
}

impl Outcome {
    fn of(result: Result<Rows, Error>) -> Self {
        match result {
            Ok(rows) => Self::Parsed(rows),
            Err(error) => Self::Failed(error.kind()),
        }
    }
}

fn headerless() -> ParseOptions {
    ParseOptions::new().headers(Headers::None)
}

fn slice_rows(input: &[u8], format: FormatOptions) -> Result<Rows, Error> {
    let mut parser = SliceParser::with_options(input, format, headerless())?;
    let mut rows = Rows::new();
    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;
        rows.push(record.iter().map(<[u8]>::to_vec).collect());
    }
    Ok(rows)
}

fn io_rows(input: &[u8], format: FormatOptions, buffer: usize) -> Result<Rows, Error> {
    let options = headerless().buffer_capacity(buffer);
    let mut parser = IoParser::with_options(Cursor::new(input.to_vec()), format, options)?;
    let mut rows = Rows::new();
    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;
        rows.push(record.iter().map(<[u8]>::to_vec).collect());
    }
    Ok(rows)
}

fn push_rows(input: &[u8], format: FormatOptions, chunk: usize) -> Result<Rows, Error> {
    let mut parser = PushParser::with_options(format, headerless())?;
    let mut record = ByteRecord::new();
    let mut rows = Rows::new();
    let mut offset = 0;
    while offset < input.len() {
        let end = input.len().min(offset + chunk.max(1));
        let mut lent = parser.chunk(&input[offset..end]);
        while let Some(mut line) = lent.next_line()? {
            line.read_byte_record_into(&mut record)?;
            rows.push(record.iter().map(<[u8]>::to_vec).collect());
        }
        offset = end;
    }
    parser.finish();
    let mut lent = parser.chunk(b"");
    while let Some(mut line) = lent.next_line()? {
        line.read_byte_record_into(&mut record)?;
        rows.push(record.iter().map(<[u8]>::to_vec).collect());
    }
    Ok(rows)
}

/// The scanning paths under test. The eager path (comment byte or
/// `BlankRecords::Skip`) is the one the fix targets; the others are lazy.
fn path_formats() -> Vec<(&'static str, FormatOptions)> {
    vec![
        ("fast", FormatOptions::CSV),
        ("eager-comment", FormatOptions::CSV.comment(Some(b'#'))),
        (
            "eager-blankskip",
            FormatOptions::CSV.blank_records(BlankRecords::Skip),
        ),
        (
            "eager-comment-blankskip",
            FormatOptions::CSV
                .comment(Some(b'#'))
                .blank_records(BlankRecords::Skip),
        ),
        ("trim", FormatOptions::CSV.trim(Whitespace::FIELDS)),
        (
            "permissive",
            FormatOptions::CSV.syntax(Syntax::Compatible(Recovery::PERMISSIVE)),
        ),
    ]
}

/// Buffer/chunk seams, including one-byte reads that split the three-byte mark
/// across refills and a size that holds it whole.
const SEAMS: &[usize] = &[1, 2, 3, 4, 5, 64];

/// Documents built by prefixing a BOM onto a body, plus a couple of shapes that
/// place structural bytes immediately after the mark.
fn documents() -> Vec<(&'static str, Vec<u8>)> {
    let with_bom = |body: &[u8]| {
        let mut doc = BOM.to_vec();
        doc.extend_from_slice(body);
        doc
    };
    vec![
        ("plain", with_bom(b"a,b\nc,d\n")),
        ("quote-after-bom", with_bom(b"\"a\",b\nc,d\n")),
        ("comment-after-bom", with_bom(b"#note\na,b\n")),
        ("blank-after-bom", with_bom(b"\na,b\n")),
        ("bom-only", with_bom(b"")),
        ("bom-then-field", with_bom(b"solo\n")),
    ]
}

/// Every front end, path, buffer, and policy agrees with the slice oracle.
#[test]
fn every_front_end_agrees_on_bom_handling() {
    for (doc_name, document) in documents() {
        for (path_name, path) in path_formats() {
            for policy in [ReadBom::Detect, ReadBom::Preserve, ReadBom::Reject] {
                let format = path.read_bom(policy);
                let oracle = Outcome::of(slice_rows(&document, format));

                for &seam in SEAMS {
                    let io = Outcome::of(io_rows(&document, format, seam));
                    assert_eq!(
                        io, oracle,
                        "io disagreed with slice: doc={doc_name} path={path_name} \
                         policy={policy:?} buffer={seam}"
                    );
                    let push = Outcome::of(push_rows(&document, format, seam));
                    assert_eq!(
                        push, oracle,
                        "push disagreed with slice: doc={doc_name} path={path_name} \
                         policy={policy:?} chunk={seam}"
                    );
                }
            }
        }
    }
}

/// `Reject` refuses a leading mark with `RejectedBom` for every front end, path,
/// buffer, and document — never a downstream syntax error. This is the direct
/// regression guard for the eager-path defect (`quote-after-bom` on the
/// `eager-*` paths) and the front-end-kind defect (`SliceParser`).
#[test]
fn reject_reports_rejected_bom_everywhere() {
    for (doc_name, document) in documents() {
        for (path_name, path) in path_formats() {
            let format = path.read_bom(ReadBom::Reject);
            assert_eq!(
                Outcome::of(slice_rows(&document, format)),
                Outcome::Failed(ErrorKind::RejectedBom),
                "slice: doc={doc_name} path={path_name}"
            );
            for &seam in SEAMS {
                assert_eq!(
                    Outcome::of(io_rows(&document, format, seam)),
                    Outcome::Failed(ErrorKind::RejectedBom),
                    "io: doc={doc_name} path={path_name} buffer={seam}"
                );
                assert_eq!(
                    Outcome::of(push_rows(&document, format, seam)),
                    Outcome::Failed(ErrorKind::RejectedBom),
                    "push: doc={doc_name} path={path_name} chunk={seam}"
                );
            }
        }
    }
}

/// The rejected-mark kind and location are identical across front ends, proving
/// the unified error contract rather than only a shared kind.
#[test]
fn rejected_bom_error_is_identical_across_front_ends() {
    let document = {
        let mut doc = BOM.to_vec();
        doc.extend_from_slice(b"a,b\n");
        doc
    };
    let format = FormatOptions::CSV.read_bom(ReadBom::Reject);

    let slice_error = SliceParser::with_options(&document, format, headerless())
        .expect_err("slice rejects the mark");

    let io_error = {
        let mut parser =
            IoParser::with_options(Cursor::new(document.clone()), format, headerless())
                .expect("io constructs; the mark is read lazily");
        drive_streaming_error(parser.next_line().and_then(|line| {
            let mut line = line.expect("positioned on the mark's record");
            line.read_byte_record_into(&mut ByteRecord::new())
        }))
    };

    let push_error = {
        let mut parser = PushParser::with_options(format, headerless()).expect("push constructs");
        let mut lent = parser.chunk(&document);
        drive_streaming_error(lent.next_line().and_then(|line| {
            let mut line = line.expect("positioned on the mark's record");
            line.read_byte_record_into(&mut ByteRecord::new())
        }))
    };

    for (label, error) in [("io", &io_error), ("push", &push_error)] {
        assert_eq!(error.kind(), slice_error.kind(), "{label} kind differs");
        assert_eq!(
            error.location().byte,
            slice_error.location().byte,
            "{label} byte location differs"
        );
    }
    assert_eq!(slice_error.kind(), ErrorKind::RejectedBom);
}

fn drive_streaming_error(result: Result<(), Error>) -> Error {
    result.expect_err("a rejected mark must fail the read")
}

/// `Detect` strips a leading mark on every path and seam: no field ever begins
/// with the mark and the rows equal the BOM-free document's rows.
#[test]
fn detect_strips_the_leading_bom_on_every_path() {
    let body: &[u8] = b"a,b\nc,d\n";
    let mut with_bom = BOM.to_vec();
    with_bom.extend_from_slice(body);

    for (path_name, path) in path_formats() {
        let format = path.read_bom(ReadBom::Detect);
        let expected = slice_rows(body, format).expect("BOM-free body parses");
        for &seam in SEAMS {
            let stripped = io_rows(&with_bom, format, seam).expect("io parses");
            assert_eq!(
                stripped, expected,
                "io Detect path={path_name} buffer={seam}"
            );
            let pushed = push_rows(&with_bom, format, seam).expect("push parses");
            assert_eq!(
                pushed, expected,
                "push Detect path={path_name} chunk={seam}"
            );
            assert!(
                !stripped[0][0].starts_with(BOM),
                "Detect left a mark path={path_name} buffer={seam}"
            );
        }
    }
}

/// `Preserve` keeps a leading mark as the first field's opening bytes on every
/// path and seam.
#[test]
fn preserve_keeps_the_leading_bom_on_every_path() {
    let mut with_bom = BOM.to_vec();
    with_bom.extend_from_slice(b"a,b\nc,d\n");

    for (path_name, path) in path_formats() {
        let format = path.read_bom(ReadBom::Preserve);
        let oracle = slice_rows(&with_bom, format).expect("slice parses");
        assert!(
            oracle[0][0].starts_with(BOM),
            "Preserve dropped the mark path={path_name}"
        );
        for &seam in SEAMS {
            assert_eq!(
                io_rows(&with_bom, format, seam).expect("io parses"),
                oracle,
                "io Preserve path={path_name} buffer={seam}"
            );
            assert_eq!(
                push_rows(&with_bom, format, seam).expect("push parses"),
                oracle,
                "push Preserve path={path_name} chunk={seam}"
            );
        }
    }
}
