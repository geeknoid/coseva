//! Errors: what went wrong, and exactly where.
//!
//! Every error carries a [`Location`] — byte offset, line, record, and field —
//! because "invalid CSV" is not an actionable bug report for a 4 GB file. This
//! example walks the error kinds you are most likely to hit, shows how to
//! report them, and shows how to accept input that a strict parser rejects.
//!
//! Run with: `cargo run --example errors`

use coseva::config::{FormatOptions, ParseOptions, Recovery, Syntax};
use coseva::format::Csv;
use coseva::{ErrorKind, SliceParser};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    locating()?;
    kinds()?;
    fatal()?;
    recovery()?;
    Ok(())
}

/// Reporting an error usefully.
fn locating() -> Result<(), Box<dyn std::error::Error>> {
    println!("== locating an error ==");

    // A syntax error is raised by the parser, so it carries the full
    // location: byte offset, line, record, and field.
    let input = "city,population\nBoston,650706\nBris\"tol,467099\n";
    let mut parser =
        SliceParser::<Csv>::new(input.as_bytes(), ParseOptions::new()).expect("parser");
    let mut failure = None;
    while failure.is_none() {
        match parser.next_line() {
            Ok(Some(mut line)) => failure = line.record().err(),
            Ok(None) => break,
            Err(error) => failure = Some(error),
        }
    }
    let error = failure.expect("the third record is malformed");
    let location = error.location();
    println!(
        "  byte {}, line {}, record {}, field {:?}: {:?}",
        location.byte,
        location.line,
        location.record,
        location.field,
        error.kind()
    );

    // A conversion error raised by `Record::parse` knows only the field index
    // it was given; the surrounding position is not available to it. Decoding
    // through the parser (`Line::decoded`, `decoded_records`) routes the same
    // error back out through the parser, which fills the rest in.
    let mut parser = SliceParser::<Csv>::new(
        "city,population\nBristol,not-a-number\n".as_bytes(),
        ParseOptions::new(),
    )
    .expect("parser");
    let mut line = parser.next_line()?.expect("one record");
    let record = line.record()?;
    let error = record.parse::<u64>(1).expect_err("not a number");
    println!(
        "  field-only location: field {:?}: {:?}",
        error.location().field,
        error.kind()
    );
    println!();
    Ok(())
}

/// Matching on the kind, when the response depends on the failure.
fn kinds() -> Result<(), Box<dyn std::error::Error>> {
    println!("== error kinds ==");

    let cases: [(&str, &[u8]); 3] = [
        ("stray quote in an unquoted field", b"a,b\"c\n"),
        ("text after a closing quote", b"\"a\"x,b\n"),
        ("unterminated quoted field", b"\"a,b\n"),
    ];

    for (label, input) in cases {
        let mut parser = SliceParser::with_options(
            input,
            FormatOptions::CSV,
            ParseOptions::new().headers(coseva::config::Headers::None),
        )?;
        let outcome = parser.next_line().and_then(|line| {
            line.expect("one record")
                .record()
                .map(|record| record.len())
        });
        match outcome {
            Ok(count) => println!("  {label:>34}: accepted, {count} fields"),
            Err(error) => match error.kind() {
                ErrorKind::UnexpectedQuote => println!("  {label:>34}: UnexpectedQuote"),
                ErrorKind::UnexpectedByteAfterQuote(byte) => {
                    println!(
                        "  {label:>34}: UnexpectedByteAfterQuote({:?})",
                        byte as char
                    );
                }
                ErrorKind::UnterminatedRecord => println!("  {label:>34}: UnterminatedRecord"),
                other => println!("  {label:>34}: {other:?}"),
            },
        }
    }
    println!();
    Ok(())
}

/// A syntax error is not something to retry past.
fn fatal() -> Result<(), Box<dyn std::error::Error>> {
    println!("== syntax errors are terminal ==");
    let mut parser = SliceParser::with_options(
        b"good,record\nbad\"record\nanother,record\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(coseva::config::Headers::None),
    )?;

    parser.next_line()?.expect("first record").record()?;
    let first = parser
        .next_line()
        .and_then(|line| line.expect("second record").record().map(|_| ()))
        .expect_err("the second record is malformed");
    println!("  first failure:  {:?}", first.kind());

    // Once the byte stream stops making sense, later offsets are guesses.
    // The parser stays failed rather than resynchronizing on a delimiter that
    // may be inside a field.
    let again = parser.next_line().err();
    println!("  parser stays failed: {}", again.is_some());
    println!();
    Ok(())
}

/// Accepting input that strict parsing rejects.
fn recovery() -> Result<(), Box<dyn std::error::Error>> {
    println!("== permissive recovery ==");
    let input: &[u8] = b"a,b\"c,d\n\"e\" ,f\n";

    // `Recovery::PERMISSIVE` turns the common real-world mistakes into
    // accepted input instead of errors.
    let format = FormatOptions::CSV.syntax(Syntax::Compatible(Recovery::PERMISSIVE));
    let mut parser = SliceParser::with_options(
        input,
        format,
        ParseOptions::new().headers(coseva::config::Headers::None),
    )?;
    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;
        let fields: Vec<String> = record
            .iter()
            .map(|field| String::from_utf8_lossy(field).into_owned())
            .collect();
        println!("  {fields:?}");
    }

    // Each relaxation is separately selectable, so you can accept exactly the
    // deviation your source produces and keep everything else strict.
    let recovery = Recovery::NONE.unquoted_quotes(true);
    let format = FormatOptions::CSV.syntax(Syntax::Compatible(recovery));
    let mut parser = SliceParser::with_options(
        b"a,b\"c\n",
        format,
        ParseOptions::new().headers(coseva::config::Headers::None),
    )?;
    let mut line = parser.next_line()?.expect("one record");
    println!("  targeted relaxation: {:?}", line.record()?.get_str(1)?);
    println!();
    Ok(())
}
