//! Decoding CSV records straight into your own structs.
//!
//! `#[derive(CsvDecode)]` builds a decoder that resolves each field by header
//! name and converts it in place. There is no intermediate map, no
//! `Vec<String>`, and no per-field allocation unless the field type itself
//! owns its bytes.
//!
//! The headline trick is that a struct may **borrow** from the parser. Give it
//! a lifetime and `&str` fields and decoding a record copies nothing at all:
//! the strings point straight into the parser's buffer. Use `String` fields
//! instead when the row has to outlive the parser step that produced it.
//!
//! Requires the `derive` feature.
//! Run with: `cargo run --example typed_decode --features derive`

use coseva::SliceParser;
use coseva::config::{FormatOptions, Headers, ParseOptions};
use coseva::encoding::CsvDecode;
use coseva::format::Csv;
use std::cmp::Reverse;
use std::error::Error;
use std::fmt;
use std::str;

const INPUT: &str = "\
city,country,population,coastal,founded
Boston,US,650706,true,1630
\"Washington, D.C.\",US,689545,false,1790
Sydney,AU,5312163,true,1788
";

fn main() -> Result<(), Box<dyn Error>> {
    borrowed_rows()?;
    owned_rows()?;
    attributes()?;
    Ok(())
}

/// A row that borrows its text out of the parser: zero copies, zero allocs.
#[derive(Debug, CsvDecode)]
struct CityRef<'row> {
    city: &'row str,
    country: &'row str,
    population: u64,
    coastal: bool,
}

fn borrowed_rows() -> Result<(), coseva::Error> {
    println!("== borrowed rows (no allocation) ==");

    // Field names drive header matching, so column order in the file is
    // irrelevant and extra columns (`founded`) are simply ignored.
    println!("  matched columns: {:?}", CityRef::field_names());

    let mut parser = SliceParser::<Csv>::new(INPUT, ParseOptions::new()).expect("parser");
    while let Some(mut line) = parser.next_line()? {
        let row: CityRef<'_> = line.decoded()?;
        println!(
            "  {:>18} ({}) {:>9} {}",
            row.city,
            row.country,
            row.population,
            if row.coastal { "coastal" } else { "inland" }
        );
    }
    println!();
    Ok(())
}

/// The same shape with owned fields, so rows can be collected and kept.
#[derive(Debug, CsvDecode)]
struct City {
    city: String,
    population: u64,
}

fn owned_rows() -> Result<(), coseva::Error> {
    println!("== owned rows (collectable) ==");
    let mut parser = SliceParser::<Csv>::new(INPUT, ParseOptions::new()).expect("parser");

    // `decoded_records` needs an owning type, since each item outlives the step.
    let mut rows: Vec<City> = parser
        .decoded_records::<City>()
        .collect::<Result<_, coseva::Error>>()?;

    rows.sort_by_key(|row| Reverse(row.population));
    for row in &rows {
        println!("  {:>18}  {}", row.city, row.population);
    }
    println!();
    Ok(())
}

/// A field type the crate knows nothing about, decoded by a custom function.
#[derive(Debug)]
struct NotHex;

impl fmt::Display for NotHex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("field is not a hexadecimal integer")
    }
}

impl Error for NotHex {}

/// `parse_with` callbacks take the raw bytes and return any `std` error.
fn parse_hex(bytes: &[u8]) -> Result<u32, NotHex> {
    let text = str::from_utf8(bytes).map_err(|_error| NotHex)?;
    u32::from_str_radix(text.trim_start_matches("0x"), 16).map_err(|_error| NotHex)
}

/// All four field attributes at once.
#[derive(Debug, CsvDecode)]
struct Swatch {
    /// Bind to a differently spelled column.
    #[csv(rename = "swatch_name")]
    name: String,
    /// Missing or empty input yields `Default::default()` instead of an error.
    #[csv(default)]
    opacity: f32,
    /// Convert with your own function.
    #[csv(parse_with = "parse_hex")]
    rgb: u32,
    /// Never read from the document; left at `Default::default()`.
    #[csv(skip)]
    seen: bool,
}

fn attributes() -> Result<(), Box<dyn Error>> {
    println!("== field attributes ==");

    // `skip` removes the field from the header contract entirely.
    println!("  matched columns: {:?}", Swatch::field_names());

    // Headers::None means the first record is data; fields bind by position.
    let mut parser = SliceParser::with_options(
        "vermilion,,0xE34234\nultramarine,0.5,0x120A8F\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;

    while let Some(mut line) = parser.next_line()? {
        let swatch: Swatch = line.decoded()?;
        println!(
            "  {:>12}  opacity {:.2}  #{:06X}  seen={}",
            swatch.name, swatch.opacity, swatch.rgb, swatch.seen
        );
    }

    // A conversion failure names the offending column and field index.
    let mut parser = SliceParser::with_options(
        "bad,1.0,not-a-colour\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = parser.next_line()?.expect("one record");
    let error = line
        .decoded::<Swatch>()
        .expect_err("the third column is not hexadecimal");
    println!(
        "  rejected field {} ({:?}): {error}",
        error.location().field,
        error.field_name().unwrap_or("?")
    );
    println!();
    Ok(())
}
