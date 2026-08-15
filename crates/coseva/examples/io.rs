//! Reading CSV that is larger than memory, or that arrives from a socket.
//!
//! [`IoParser`] pulls from any [`Read`] and keeps only a bounded
//! window of the input alive. Records still borrow that window, so the
//! zero-copy property survives: nothing is copied to give you a field, and
//! the parser's buffers are reused for the whole scan rather than regrown per
//! record.
//!
//! The window size is a policy, not a correctness constraint. A record longer
//! than the current window simply causes the window to grow until the record
//! fits or [`Limits::max_record_bytes`] is exceeded, so a 64-byte buffer parses
//! the same documents a 64-KiB buffer does — just with more reads.
//!
//! Run with: `cargo run --example streaming`

use std::env;
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::io::Cursor;

use coseva::config::{FormatOptions, Limits, ParseOptions};
use coseva::format::Csv;
use coseva::{ByteRecord, IoParser};

fn main() -> Result<(), Box<dyn Error>> {
    let corpus = build_corpus(5_000);
    println!("generated {} bytes of CSV\n", corpus.len());

    borrow_from_the_window(&corpus)?;
    reuse_one_record(&corpus)?;
    tiny_window(&corpus)?;
    from_a_file(&corpus)?;
    Ok(())
}

/// Build a synthetic document big enough to force many window refills.
fn build_corpus(rows: usize) -> Vec<u8> {
    let mut out = String::from("id,region,amount,note\n");
    for row in 0..rows {
        let region = ["north", "south", "east", "west"][row % 4];
        // Every 500th row carries a quoted field with an embedded comma, so
        // the escaped-field path is exercised across window boundaries too.
        if row % 500 == 0 {
            writeln!(
                out,
                "{row},{region},{},\"flagged, review manually\"",
                row * 7 % 1000
            )
            .expect("writing to a String cannot fail");
        } else {
            writeln!(out, "{row},{region},{},ok", row * 7 % 1000)
                .expect("writing to a String cannot fail");
        }
    }
    out.into_bytes()
}

/// The default path: fields borrow the parser's window, no copying.
fn borrow_from_the_window(corpus: &[u8]) -> Result<(), coseva::Error> {
    println!("== borrowing from the window ==");
    let mut parser =
        IoParser::<_, Csv>::new(Cursor::new(corpus), ParseOptions::new()).expect("parser");

    // Resolve columns by name once, up front.
    let amount = parser.header_index("amount")?.expect("an amount column");
    let note = parser.header_index("note")?.expect("a note column");

    let mut rows = 0_u64;
    let mut total = 0_u64;
    let mut flagged = 0_u64;
    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;
        total += record.parse::<u64>(amount)?.unwrap_or_default();
        if record.get(note) != Some(b"ok".as_slice()) {
            flagged += 1;
        }
        rows += 1;
    }
    println!("  {rows} rows, total {total}, {flagged} flagged\n");
    Ok(())
}

/// When a record must outlive the window, decode it into a record you reuse.
fn reuse_one_record(corpus: &[u8]) -> Result<(), coseva::Error> {
    println!("== one reused owned record ==");
    let mut parser =
        IoParser::<_, Csv>::new(Cursor::new(corpus), ParseOptions::new()).expect("parser");

    // Allocated once; refilled in place on every step.
    let mut record = ByteRecord::new();
    let mut longest = 0;
    let mut rows = 0_u64;
    while let Some(mut line) = parser.next_line()? {
        line.read_byte_record_into(&mut record)?;
        longest = longest.max(record.get(3).map_or(0, <[u8]>::len));
        rows += 1;
    }
    println!("  {rows} rows, longest note {longest} bytes\n");
    Ok(())
}

/// A deliberately tiny window, to show that buffering is a policy choice.
///
/// The results must match the default-window scan exactly.
fn tiny_window(corpus: &[u8]) -> Result<(), coseva::Error> {
    println!("== 64-byte window ==");
    let options = ParseOptions::new()
        .buffer_capacity(64)
        // Limits are enforced against the growing window, so a record longer
        // than this is rejected rather than buffered without bound.
        .limits(Limits::new(1 << 20, 1 << 16, 1024));
    let mut parser = IoParser::with_options(Cursor::new(corpus), FormatOptions::CSV, options)?;

    let mut rows = 0_u64;
    let mut total = 0_u64;
    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;
        total += record.parse::<u64>(2)?.unwrap_or_default();
        rows += 1;
    }
    println!("  {rows} rows, total {total} (identical to the default-sized scan)\n");
    Ok(())
}

/// Reading straight from a path, and rewinding to scan a second time.
fn from_a_file(corpus: &[u8]) -> Result<(), Box<dyn Error>> {
    println!("== from a file ==");
    let path = env::temp_dir().join("coseva_example_streaming.csv");
    fs::write(&path, corpus)?;

    let mut parser = IoParser::from_path(&path, FormatOptions::CSV, ParseOptions::new())?;
    let first_pass = parser.byte_records().count();

    // `rewind` returns to the first data record, re-using the resolved
    // headers, so a second pass costs no re-parse of the header row.
    parser.rewind()?;
    let second_pass = parser.byte_records().count();

    println!("  {first_pass} rows, then {second_pass} rows after rewind");
    assert_eq!(first_pass, second_pass);

    fs::remove_file(&path)?;
    println!("  cleaned up {}\n", path.display());
    Ok(())
}
