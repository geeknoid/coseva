//! Filtering records without paying to parse them.
//!
//! Most scans want a small slice of a large file. A [`Predicate`] is pushed
//! down into the parser: it tests the raw bytes of one column while the record
//! is still a span in the buffer, and records that fail never get split into
//! fields, decoded, or copied. The further the filter rejects, the less work
//! the record costs.
//!
//! Run with: `cargo run --example filtering`

use coseva::SliceParser;
use coseva::config::ParseOptions;
use coseva::format::Csv;
use coseva::{Column, Predicate};

const DATA: &[u8] = b"\
city,country,population,region
Boston,US,650706,New England
Bristol,GB,467099,South West
Bordeaux,FR,259809,Nouvelle-Aquitaine
Bologna,IT,392564,Emilia-Romagna
Baltimore,US,585708,Mid-Atlantic
Brisbane,AU,1272000,Queensland
";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    by_name()?;
    the_four_kinds()?;
    by_index()?;
    iterators()?;
    Ok(())
}

/// Naming the column is the usual case for a document with headers.
fn by_name() -> Result<(), coseva::Error> {
    println!("== equals, by column name ==");
    let predicate = Predicate::equals("country", "US");

    let mut parser = SliceParser::<Csv>::new(DATA, ParseOptions::new()).expect("parser");
    // `next_matching_line` advances to the next record that passes, skipping
    // the rest at full scan speed.
    while let Some(mut line) = parser.next_matching_line(&predicate)? {
        let record = line.record()?;
        println!(
            "  {} ({})",
            record.get_str(0)?.unwrap_or(""),
            record.parse::<u64>(2)?.unwrap_or(0)
        );
    }
    println!();
    Ok(())
}

/// The four match kinds, all on raw bytes.
fn the_four_kinds() -> Result<(), coseva::Error> {
    println!("== match kinds ==");
    let cases = [
        ("equals \"FR\"", Predicate::equals("country", "FR")),
        ("contains \"land\"", Predicate::contains("region", "land")),
        ("starts_with \"Bo\"", Predicate::starts_with("city", "Bo")),
        ("ends_with \"West\"", Predicate::ends_with("region", "West")),
    ];

    for (label, predicate) in cases {
        let mut parser = SliceParser::<Csv>::new(DATA, ParseOptions::new()).expect("parser");
        let mut hits = Vec::new();
        while let Some(mut line) = parser.next_matching_line(&predicate)? {
            hits.push(line.record()?.get_str(0)?.unwrap_or("").to_owned());
        }
        println!("  {label:>18}: {hits:?}");
    }
    println!();
    Ok(())
}

/// A positional column, for documents with no header row or unstable names.
fn by_index() -> Result<(), coseva::Error> {
    println!("== by column index ==");
    // `Column` is built from a `usize` or a `&str`, so either spelling works
    // and the explicit form is available when inference needs help.
    let predicate = Predicate::equals(Column::Index(1), "GB");

    let mut parser = SliceParser::<Csv>::new(DATA, ParseOptions::new()).expect("parser");
    let mut line = parser.next_matching_line(&predicate)?.expect("one GB city");
    println!("  {}", line.record()?.get_str(0)?.unwrap_or(""));
    println!();
    Ok(())
}

/// The same filters, as iterators.
fn iterators() -> Result<(), Box<dyn std::error::Error>> {
    println!("== matching iterators ==");
    let predicate = Predicate::equals("country", "US");

    // Owned records, filtered.
    let mut parser = SliceParser::<Csv>::new(DATA, ParseOptions::new()).expect("parser");
    let cities: Vec<String> = parser
        .matching_byte_records(&predicate)
        .map(|record| Ok(String::from_utf8_lossy(record?.get(0).unwrap_or(b"")).into_owned()))
        .collect::<Result<_, coseva::Error>>()?;
    println!("  matching_byte_records: {cities:?}");

    // Counting matches never materializes a record at all.
    let mut parser = SliceParser::<Csv>::new(DATA, ParseOptions::new()).expect("parser");
    let matches = parser.matching_byte_records(&predicate).count();
    println!(
        "  {matches} US cities out of {} total",
        SliceParser::<Csv>::new(DATA, ParseOptions::new())
            .expect("parser")
            .byte_records()
            .count()
    );
    println!();
    Ok(())
}
