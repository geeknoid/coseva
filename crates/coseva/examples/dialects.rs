//! Reading the CSV-shaped formats that are not quite CSV.
//!
//! "CSV" is a family, not a format. Tabs instead of commas, semicolons because
//! the decimal separator took the comma, backslash escapes instead of doubled
//! quotes, `\N` for NULL, comment lines, CRLF terminators, a byte-order mark
//! glued to the front by a spreadsheet. [`FormatOptions`] describes all of it,
//! and ships presets for the dialects you are most likely to meet.
//!
//! [`ParseOptions`] is the orthogonal half: it governs how the parser behaves
//! (headers, limits, buffering) rather than what the bytes mean.
//!
//! Run with: `cargo run --example dialects`

use coseva::SliceParser;
use coseva::config::{
    BlankRecords, FieldCount, FormatOptions, Headers, Limits, ParseOptions, ReadBom, RecordEnding,
    Whitespace,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    presets()?;
    nulls()?;
    comments_and_blanks()?;
    whitespace()?;
    custom_format()?;
    parse_policies()?;
    Ok(())
}

/// Dump every field of every record, so differences are impossible to miss.
fn dump(label: &str, input: &[u8], format: FormatOptions) -> Result<(), coseva::Error> {
    let options = ParseOptions::new().headers(Headers::None);
    let mut parser = SliceParser::with_options(input, format, options)?;
    print!("  {label:>16}: ");
    let mut records = Vec::new();
    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;
        let fields: Vec<String> = record
            .iter()
            .map(|field| String::from_utf8_lossy(field).into_owned())
            .collect();
        records.push(format!("{fields:?}"));
    }
    println!("{}", records.join(" "));
    Ok(())
}

/// The built-in presets.
fn presets() -> Result<(), coseva::Error> {
    println!("== presets ==");
    dump("CSV", b"a,b;c\td\n", FormatOptions::CSV)?;
    dump("TSV", b"a,b;c\td\n", FormatOptions::TSV)?;
    dump("SEMICOLON", b"a,b;c\td\n", FormatOptions::SEMICOLON)?;
    dump("PIPE", b"a|b|c\n", FormatOptions::PIPE)?;

    // A quote inside a quoted field: CSV doubles it, BACKSLASH_CSV escapes it.
    // Both spell the same field, `a"b`.
    dump("CSV", br#""a""b",c"#, FormatOptions::CSV)?;
    dump(
        "BACKSLASH_CSV",
        br#""a\"b",c"#,
        FormatOptions::BACKSLASH_CSV,
    )?;

    // RFC 4180 mandates CRLF; EXCEL and PYTHON_CSV match those tools.
    dump("RFC4180", b"a,b\r\nc,d\r\n", FormatOptions::RFC4180)?;
    dump("EXCEL", b"a,b\r\nc,d\r\n", FormatOptions::EXCEL)?;
    println!();
    Ok(())
}

/// Empty is not the same as absent, in the dialects that say so.
fn nulls() -> Result<(), coseva::Error> {
    println!("== NULL representations ==");

    // PostgreSQL `COPY ... CSV`: an *unquoted* empty field is NULL, a quoted
    // empty field is the empty string.
    let mut parser = SliceParser::with_options(
        b",\"\",value\n",
        FormatOptions::POSTGRES_COPY_CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = parser.next_line()?.expect("one record");
    let record = line.record()?;
    println!(
        "  postgres: field0 null={:?}  field1 null={:?}  field2 null={:?}",
        record.is_null(0),
        record.is_null(1),
        record.is_null(2)
    );

    // MySQL text exports spell NULL as an unescaped `\N`.
    let mut parser = SliceParser::with_options(
        b"\\N\tvalue\n",
        FormatOptions::MYSQL,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = parser.next_line()?.expect("one record");
    let record = line.record()?;
    println!(
        "  mysql:    field0 null={:?}  field1 null={:?}",
        record.is_null(0),
        record.is_null(1)
    );
    println!();
    Ok(())
}

/// Lines the document does not intend as data.
fn comments_and_blanks() -> Result<(), coseva::Error> {
    println!("== comments and blank lines ==");
    let input = b"# generated 2026-01-01\na,b\n\nc,d\n";

    dump("plain CSV", input, FormatOptions::CSV)?;
    dump("COMMENTED_CSV", input, FormatOptions::COMMENTED_CSV)?;

    // Comment byte and blank-line policy are independent knobs.
    let format = FormatOptions::CSV
        .comment(Some(b'#'))
        .blank_records(BlankRecords::Skip);
    dump("both skipped", input, format)?;
    println!();
    Ok(())
}

/// Padding around fields, and whether quoting protects it.
fn whitespace() -> Result<(), coseva::Error> {
    println!("== whitespace ==");
    let input = b"  a  ,\"  b  \"\n";

    dump("no trimming", input, FormatOptions::CSV)?;
    dump("TRIMMED_CSV", input, FormatOptions::TRIMMED_CSV)?;

    // Trim unquoted fields but treat quoting as "hands off", which is what
    // most tools mean by trimming.
    let format = FormatOptions::CSV.trim(Whitespace::ALL.unquoted_only());
    dump("unquoted only", input, format)?;

    // `skip_initial_space` is the narrower Python-style rule: drop spaces
    // after a delimiter only, leaving trailing spaces alone.
    let input = b"a, b , c\n";
    dump("no trimming", input, FormatOptions::CSV)?;
    let format = FormatOptions::CSV.skip_initial_space(true);
    dump("skip_initial", input, format)?;
    println!();
    Ok(())
}

/// Anything the presets do not cover, built from parts.
fn custom_format() -> Result<(), coseva::Error> {
    println!("== a format of your own ==");

    // Records ended by CRLF, fields separated by `~`, quoted with `'`,
    // comments after `%`, and a leading BOM stripped if present.
    let format = FormatOptions::new()
        .delimiter(b'~')
        .quote(b'\'')
        .record_ending(RecordEnding::CrLf)
        .comment(Some(b'%'))
        .read_bom(ReadBom::Detect);

    let input = "\u{feff}% a comment\r\nalpha~'beta~gamma'~delta\r\n".as_bytes();
    dump("custom", input, format)?;
    println!();
    Ok(())
}

/// Parser behaviour, as opposed to byte meaning.
fn parse_policies() -> Result<(), Box<dyn std::error::Error>> {
    println!("== parse policies ==");

    // `Headers::Provided` supplies names for a document that has none, so
    // name-based lookup works without consuming a record.
    let headers = coseva::ByteRecord::from_iter(["city", "population"]);
    let mut parser = SliceParser::with_options(
        b"Boston,650706\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::Provided(headers)),
    )?;
    println!(
        "  provided headers: 'population' is column {:?}",
        parser.header_index("population")?
    );
    println!(
        "  first record is still data: {}",
        parser.byte_records().count()
    );

    // `FieldCount` turns a ragged document into an error rather than a
    // silently short record.
    let mut parser = SliceParser::with_options(
        b"a,b,c\n1,2\n",
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .field_count(FieldCount::MatchFirst),
    )?;
    parser.next_line()?.expect("first record").record()?;
    let error = parser
        .next_line()?
        .expect("second record")
        .record()
        .expect_err("the second record is one field short");
    println!("  ragged record rejected: {:?}", error.kind());

    // Limits bound the work a hostile document can cause.
    let mut parser = SliceParser::with_options(
        b"aaaaaaaaaaaaaaaaaaaa,b\n",
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .limits(Limits::new(1024, 8, 16)),
    )?;
    let error = parser
        .next_line()?
        .expect("one record")
        .record()
        .expect_err("the first field exceeds max_field_bytes");
    println!("  oversized field rejected: {:?}", error.kind());
    println!();
    Ok(())
}
