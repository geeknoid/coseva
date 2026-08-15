//! Reading a CSV document you already hold in memory.
//!
//! This is the shortest path into the crate. [`SliceParser`] borrows the input
//! you hand it and hands back records that borrow it right back, so a plain
//! unquoted field costs no allocation and no copy at all.
//!
//! Three ways to consume a record are shown, in increasing order of
//! convenience and decreasing order of control:
//!
//!   1. `line.record()` — a lending [`Record`] that borrows the input.
//!   2. `parser.byte_records()` — an iterator of owned [`ByteRecord`]s.
//!   3. `record.parse::<T>()` — typed conversion straight out of the bytes.
//!
//! Run with: `cargo run --example quickstart`

use coseva::config::ParseOptions;
use coseva::format::Csv;
use coseva::{ByteRecord, SliceParser};

/// A small document with a header row, a quoted field containing a comma, and
/// an escaped quote.
const INPUT: &str = "\
city,country,population,coastal
Boston,US,650706,true
\"Washington, D.C.\",US,689545,false
Sydney,AU,5312163,true
\"L'\"\"Aquila\",IT,69439,false
";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    borrowed_records()?;
    owned_records()?;
    header_lookup()?;
    Ok(())
}

/// The zero-copy path: every field borrows directly out of `INPUT`.
fn borrowed_records() -> Result<(), coseva::Error> {
    println!("== borrowed records ==");
    let mut parser = SliceParser::<Csv>::new(INPUT, ParseOptions::new()).expect("parser");

    // The first record is consumed as headers by default.
    let headers = parser.headers()?.expect("the document has a header row");
    println!("  headers: {:?}", headers.get_str(0)?);

    let mut total = 0_u64;
    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;

        // `get_str` validates UTF-8 and borrows; `parse` converts in place.
        let city = record.get_str(0)?.unwrap_or_default();
        let population: u64 = record.parse(2)?.unwrap_or_default();
        let coastal: bool = record.parse(3)?.unwrap_or_default();

        total += population;
        println!(
            "  {:>18}  {:>9}  {}",
            city,
            population,
            if coastal { "coastal" } else { "inland" }
        );
    }
    println!("  total population: {total}\n");
    Ok(())
}

/// The owning path: `ByteRecord` values that outlive the borrow of the input.
///
/// `byte_records` yields a fresh record each step. When you only need one
/// record alive at a time, prefer `read_byte_record_into` with a record you
/// reuse, which keeps the allocation across the whole scan.
fn owned_records() -> Result<(), coseva::Error> {
    println!("== owned records ==");
    let mut parser = SliceParser::<Csv>::new(INPUT, ParseOptions::new()).expect("parser");

    // Reuse a single record so the loop allocates once rather than per row.
    let mut record = ByteRecord::new();
    let mut rows = 0;
    while let Some(mut line) = parser.next_line()? {
        line.read_byte_record_into(&mut record)?;
        rows += 1;
        if rows == 2 {
            // The quotes are gone: the field is delivered already decoded.
            assert_eq!(record.get(0), Some(&b"Washington, D.C."[..]));
            println!("  row 2 field 0 decoded to {:?}", record.get_str(0)?);
        }
    }

    // The iterator form, when you would rather collect than loop.
    let mut parser = SliceParser::<Csv>::new(INPUT, ParseOptions::new()).expect("parser");
    let cities: Vec<String> = parser
        .byte_records()
        .map(|record| Ok(record?.get_str(0)?.unwrap_or_default().to_owned()))
        .collect::<Result<_, coseva::Error>>()?;
    println!("  collected {} cities: {cities:?}\n", cities.len());
    Ok(())
}

/// Columns are easier to address by name than by position.
fn header_lookup() -> Result<(), coseva::Error> {
    println!("== header lookup ==");
    let mut parser = SliceParser::<Csv>::new(INPUT, ParseOptions::new()).expect("parser");

    // Resolve the column once, before the scan, then index by number.
    let population = parser
        .header_index("population")?
        .expect("a population column");
    println!("  'population' is column {population}");

    let mut biggest: Option<(String, u64)> = None;
    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;
        let value: u64 = record.parse(population)?.unwrap_or_default();
        if biggest.as_ref().is_none_or(|(_, best)| value > *best) {
            biggest = Some((record.get_str(0)?.unwrap_or_default().to_owned(), value));
        }
    }

    let (city, value) = biggest.expect("at least one record");
    println!("  largest: {city} at {value}\n");
    Ok(())
}
