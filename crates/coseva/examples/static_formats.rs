//! Declaring your CSV format at compile time.
//!
//! A parser normally reads its delimiter, quote, escape kind and record ending
//! out of the options it was built with, checking them once per field. When the
//! format is known while compiling, those become immediates instead and the
//! branches that cannot be taken disappear.
//!
//! This is worth doing for quote-heavy data, where the per-field branching is a
//! real cost. On plain unquoted data the scan is dominated by a SIMD search that
//! already keeps the format bytes in registers, and specializing buys close to
//! nothing.
//!
//! Nothing here changes what the parser accepts: a specialized parser and a
//! run-time-configured one over the same format agree byte for byte, including
//! on malformed input, and the crate's test suite asserts exactly that.
//!
//! Run with: `cargo run --example static_formats`

use coseva::config::{FormatOptions, Headers, ParseOptions};
use coseva::format::{Csv, StaticFormat, Tsv};
use coseva::{Error, IoParser, SliceParser, csv_format};

csv_format! {
    /// The pipe-delimited, single-quoted export our upstream system produces.
    ///
    /// Declaring it here means an unusable combination -- a delimiter that is
    /// also the quote byte, say -- fails to compile rather than failing when
    /// the first parser is built.
    pub Upstream = FormatOptions::CSV.delimiter(b'|').quote(b'\'');
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    built_in_format()?;
    custom_format()?;
    streaming()?;
    format_chosen_at_run_time()?;
    generic_over_format()?;
    Ok(())
}

/// The common case: a built-in format named at the type level.
fn built_in_format() -> Result<(), Error> {
    let mut parser = SliceParser::<Csv>::new(
        b"city,country,pop\n\"Boston, MA\",US,650706\nLondon,UK,8982000\n",
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;

    println!("== built-in format ==");
    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;
        println!(
            "{} ({})",
            String::from_utf8_lossy(record.get(0).unwrap_or_default()),
            String::from_utf8_lossy(record.get(1).unwrap_or_default()),
        );
    }
    Ok(())
}

/// A format the crate does not ship, declared with `csv_format!`.
fn custom_format() -> Result<(), Error> {
    let mut parser = SliceParser::<Upstream>::new(
        b"city|country|pop\n'Boston, MA'|US|650706\n",
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;

    println!("\n== custom format ==");
    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;
        // The quoted field keeps its comma: `|` is the delimiter here.
        println!(
            "{}",
            String::from_utf8_lossy(record.get(0).unwrap_or_default())
        );
    }
    Ok(())
}

/// Specialization is a property of the parser type, not of one entry point, so
/// the streaming and push parsers take a static format the same way.
fn streaming() -> Result<(), Error> {
    let source = std::io::Cursor::new(b"a\tb\nc\td\n".to_vec());
    let mut parser = IoParser::<_, Tsv>::new(source, ParseOptions::new().headers(Headers::None))?;

    println!("\n== streaming, tab separated ==");
    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;
        println!("{} fields", record.len());
    }
    Ok(())
}

/// The format is a run-time value, and the work still runs specialized.
///
/// This is the case the static-only design served badly: a format that arrives
/// from a command-line flag or a config file. Nothing here names a format, and
/// no control flow is inverted to accommodate one -- the parser classifies the
/// options it was built with and picks the specialized kernel itself. A format
/// it does not recognize still parses, just without folding, so this is never a
/// restriction on what you can read -- only on what gets faster.
fn format_chosen_at_run_time() -> Result<(), Error> {
    println!("\n== chosen from a run-time value ==");
    for (name, format, input) in [
        ("csv", FormatOptions::CSV, &b"a,b\nc,d\n"[..]),
        ("tsv", FormatOptions::TSV, &b"a\tb\nc\td\n"[..]),
        // Not a format coseva specializes; it parses unspecialized.
        ("caret", FormatOptions::CSV.delimiter(b'^'), &b"a^b^c\n"[..]),
    ] {
        let mut parser =
            SliceParser::with_options(input, format, ParseOptions::new().headers(Headers::None))?;
        let mut fields = 0;
        while let Some(mut line) = parser.next_line()? {
            fields += line.record()?.len();
        }
        println!("{name}: {fields} fields");
    }
    Ok(())
}

/// Code that is generic over a declared format compiles once and serves all.
///
/// Writing `F: StaticFormat` rather than naming one format lets a helper work
/// for every format a caller declares, each instantiation folding its own
/// constants. A run-time format needs no generic at all: `with_options`
/// classifies itself and picks the specialized kernel on its own.
fn generic_over_format() -> Result<(), Error> {
    fn first_field<F: StaticFormat>(input: &[u8]) -> Result<String, Error> {
        let mut parser = SliceParser::<F>::new(input, ParseOptions::new().headers(Headers::None))?;
        let Some(mut line) = parser.next_line()? else {
            return Ok(String::new());
        };
        let record = line.record()?;
        Ok(String::from_utf8_lossy(record.get(0).unwrap_or_default()).into_owned())
    }

    println!("\n== one helper, every declared format ==");
    println!("csv:      {}", first_field::<Csv>(b"one,two\n")?);
    println!("upstream: {}", first_field::<Upstream>(b"'one, two'|x\n")?);

    // A run-time format does not need the generic: it specializes itself.
    let mut parser = SliceParser::with_options(
        b"one;two\n",
        FormatOptions::SEMICOLON,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = parser.next_line()?.expect("a record");
    let record = line.record()?;
    println!(
        "dynamic:  {}",
        String::from_utf8_lossy(record.get(0).unwrap_or_default())
    );
    Ok(())
}
