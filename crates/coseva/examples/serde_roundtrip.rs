//! Serde interop.
//!
//! The `derive` feature's `CsvDecode`/`CsvEncode` are the fast path: they are
//! specialized for CSV and skip serde's intermediate model entirely. Serde
//! support exists for the other case — when your type already derives
//! `Serialize`/`Deserialize` for JSON or a database and you would rather not
//! maintain a second set of attributes.
//!
//! Requires the `serde` feature.
//! Run with: `cargo run --example serde_roundtrip --features serde`

use coseva::config::ParseOptions;
use coseva::config::{EmitOptions, FormatOptions};
use coseva::format::Csv;
use coseva::{SliceParser, serialize_to_vec};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Measurement {
    station: String,
    #[serde(rename = "temp_c")]
    celsius: f64,
    quality: Quality,
    #[serde(default)]
    note: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Quality {
    Good,
    Suspect,
}

/// A type with borrowed fields avoids allocating per record. The lifetime ties
/// it to the parser's view of the input.
#[derive(Debug, Deserialize)]
struct StationRef<'a> {
    station: &'a str,
    #[serde(rename = "temp_c")]
    celsius: f64,
}

const DATA: &str = "\
station,temp_c,quality,note
KBOS,21.5,good,
KSFO,17.25,suspect,sensor drift
KJFK,19.0,good,
";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let measurements = deserialize()?;
    let bytes = serialize(&measurements)?;
    round_trip(&measurements, &bytes)?;
    borrowed()?;
    Ok(())
}

/// Reading serde types out of a document.
fn deserialize() -> Result<Vec<Measurement>, coseva::Error> {
    println!("== deserialize ==");
    let mut parser = SliceParser::<Csv>::new(DATA.as_bytes(), ParseOptions::new()).expect("parser");

    // Header names drive field matching, so `#[serde(rename)]` and column
    // order in the file are both honoured.
    let measurements: Vec<Measurement> = parser
        .deserialized_records::<Measurement>()
        .collect::<Result<_, _>>()?;

    for measurement in &measurements {
        println!("  {measurement:?}");
    }
    println!();
    Ok(measurements)
}

/// Writing them back out.
fn serialize(measurements: &[Measurement]) -> Result<Vec<u8>, coseva::Error> {
    println!("== serialize ==");
    // The header row is derived from the struct, so it matches the values.
    let bytes = serialize_to_vec(measurements, FormatOptions::CSV, EmitOptions::new())?;
    print!("{}", String::from_utf8_lossy(&bytes));
    println!();
    Ok(bytes)
}

/// The whole point of having both halves.
fn round_trip(original: &[Measurement], bytes: &[u8]) -> Result<(), coseva::Error> {
    println!("== round trip ==");
    let mut parser = SliceParser::<Csv>::new(bytes, ParseOptions::new()).expect("parser");
    let reparsed: Vec<Measurement> = parser
        .deserialized_records::<Measurement>()
        .collect::<Result<_, _>>()?;

    assert_eq!(original, reparsed.as_slice());
    println!(
        "  {} records survived the round trip unchanged",
        reparsed.len()
    );
    println!();
    Ok(())
}

/// One record at a time, and borrowing from the input.
fn borrowed() -> Result<(), Box<dyn std::error::Error>> {
    println!("== one at a time, borrowed ==");

    let mut parser = SliceParser::<Csv>::new(DATA.as_bytes(), ParseOptions::new()).expect("parser");
    while let Some(mut line) = parser.next_line()? {
        let station: StationRef<'_> = line.deserialized()?;
        println!("  {} at {:.1}C", station.station, station.celsius);
    }
    println!();
    Ok(())
}
