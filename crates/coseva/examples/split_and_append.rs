//! Splitting output into size-bounded files, and resuming an existing one.
//!
//! Two problems that show up once a CSV export leaves your process:
//!
//!   * The consumer has a size cap — an upload limit, a shard, a spreadsheet
//!     that gives up past a certain point. [`encode_to_segments`] writes as
//!     many files as it takes, each under the cap, and repeats the header row
//!     in every one so each part is a valid document on its own.
//!   * The export is incremental — a nightly job appending to yesterday's
//!     file. [`encode_append_path`] continues an existing document without
//!     writing a second header row.
//!
//! Requires the `derive` feature.
//! Run with: `cargo run --example split_and_append --features derive`

use coseva::config::ParseOptions;
use coseva::config::{EmitOptions, FormatOptions};
use coseva::encoding::CsvEncode;
use coseva::format::Csv;
use coseva::{SliceParser, encode_append_path, encode_to_path, encode_to_segments};
use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(CsvEncode)]
struct Event {
    id: u32,
    kind: &'static str,
    payload: &'static str,
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = env::temp_dir().join("coseva_example_segments");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)?;

    let segments = split(&dir)?;
    verify(&segments)?;
    append(&dir)?;

    fs::remove_dir_all(&dir)?;
    Ok(())
}

fn events(count: u32) -> Vec<Event> {
    (0..count)
        .map(|id| Event {
            id,
            kind: if id % 3 == 0 { "click" } else { "view" },
            payload: "some, quoted payload",
        })
        .collect()
}

/// One call, as many files as the cap requires.
fn split(dir: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    println!("== splitting ==");

    // The namer is called with a zero-based segment number and returns the
    // path for that part, so numbering, padding, and directory layout are
    // yours rather than the library's.
    let namer = |segment: usize| dir.join(format!("events-{segment:03}.csv"));

    // A record is never split across segments: a record that would push a
    // segment past the cap starts the next one instead.
    let segments = encode_to_segments(
        events(500),
        4096,
        namer,
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;

    for path in &segments {
        println!(
            "  {} ({} bytes)",
            path.file_name().unwrap_or_default().to_string_lossy(),
            fs::metadata(path)?.len()
        );
    }
    println!();
    Ok(segments)
}

/// Each part stands alone.
fn verify(segments: &[PathBuf]) -> Result<(), Box<dyn Error>> {
    println!("== each segment is a valid document ==");
    let mut total = 0;
    for path in segments {
        let bytes = fs::read(path)?;
        let mut parser = SliceParser::<Csv>::new(&bytes, ParseOptions::new()).expect("parser");

        // The header row is repeated in every segment, so name-based lookup
        // works in each one without stitching them back together.
        let header = parser
            .headers()?
            .expect("every segment carries headers")
            .clone();
        let count = parser.byte_records().count();
        total += count;
        println!(
            "  {}: header {:?}, {count} records",
            path.file_name().unwrap_or_default().to_string_lossy(),
            header
                .iter()
                .map(|field| String::from_utf8_lossy(field).into_owned())
                .collect::<Vec<_>>()
        );
    }
    assert_eq!(total, 500);
    println!("  {total} records across {} segments", segments.len());
    println!();
    Ok(())
}

/// Continuing a document written earlier.
fn append(dir: &Path) -> Result<(), Box<dyn Error>> {
    println!("== appending ==");
    let path = dir.join("rolling.csv");

    // The first write creates the file and its header row.
    encode_to_path(&path, events(3), FormatOptions::CSV, EmitOptions::new())?;
    print!("{}", fs::read_to_string(&path)?);

    // Later writes continue it. No second header row is emitted, so the file
    // stays a single valid document.
    encode_append_path(&path, events(2), FormatOptions::CSV, EmitOptions::new())?;

    let bytes = fs::read(&path)?;
    let mut parser = SliceParser::<Csv>::new(&bytes, ParseOptions::new()).expect("parser");
    let _ = parser.headers()?;
    println!(
        "  after appending: {} records total",
        parser.byte_records().count()
    );
    println!(
        "  header rows in the file: {}",
        fs::read_to_string(&path)?
            .lines()
            .filter(|line| line.starts_with("id,kind"))
            .count()
    );
    println!();
    Ok(())
}
