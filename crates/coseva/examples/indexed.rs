//! Random access into a CSV file.
//!
//! CSV has no record index, so "give me record 4,000,000" normally means
//! scanning from the top — and quoting means you cannot even seek to a newline
//! and trust it, because that newline may be inside a field. A [`CsvIndex`]
//! is built once by a full scan, records the byte offset and line of every
//! record, and can be persisted next to the file. After that, jumping to any
//! record is a seek.
//!
//! This is the right shape for a file that is read many times: a report tool,
//! a paging UI, or anything sampling rows out of order.
//!
//! The index is built with no header handling, so record 0 is the header row
//! if the document has one.
//!
//! Requires the `index` feature.
//! Run with: `cargo run --example indexed --features index`

use coseva::index::{CsvIndex, CsvIndexReader, IndexOptions};
use std::env;
use std::error::Error;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

fn main() -> Result<(), Box<dyn Error>> {
    let path = env::temp_dir().join("coseva_example_indexed.csv");
    let index_path = env::temp_dir().join("coseva_example_indexed.idx");
    write_corpus(&path)?;

    let index = in_memory(&path)?;
    random_access(&index, &path)?;
    build_and_persist(&path, &index_path)?;
    from_disk(&index_path, &path)?;
    staleness(&index, &path)?;

    fs::remove_file(&path)?;
    fs::remove_file(&index_path)?;
    Ok(())
}

/// A file worth indexing, including quoted fields with embedded newlines so
/// that seeking to a `\n` would land in the middle of a record.
fn write_corpus(path: &Path) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    writeln!(file, "id,name,note")?;
    for id in 0..1000 {
        if id % 250 == 0 {
            writeln!(file, "{id},row-{id},\"a note\nspanning two lines\"")?;
        } else {
            writeln!(file, "{id},row-{id},plain")?;
        }
    }
    file.flush()
}

/// Indexing bytes already in memory.
fn in_memory(path: &Path) -> Result<CsvIndex, Box<dyn Error>> {
    println!("== building ==");
    let source = fs::read(path)?;

    // `IndexOptions` carries the format and limits used for the scan. They are
    // stored in the index and reused when it hands you a parser, so a seek can
    // never be interpreted with a different dialect than the scan used.
    let index = CsvIndex::build(&source, IndexOptions::default())?;
    println!(
        "  indexed {} records (record 0 is the header row)",
        index.len()
    );
    println!("  scan limits stored with it: {:?}", index.limits());
    println!();
    Ok(index)
}

/// Jumping straight to a record.
fn random_access(index: &CsvIndex, path: &Path) -> Result<(), Box<dyn Error>> {
    println!("== random access ==");
    for record_number in [1_usize, 251, 1000] {
        // `parser_at_path` opens the file and positions it, so the parser
        // starts at the record you asked for rather than at the top.
        let mut parser = index.parser_at_path(path, record_number)?;
        let mut line = parser.next_line()?.expect("that record exists");
        let record = line.record()?;
        println!(
            "  record {record_number:>4} at byte {:>6}, source line {:>4}: {:?}",
            index.record_offset(record_number).unwrap_or(0),
            index.record_line(record_number).unwrap_or(0),
            record.get_str(1)?
        );
    }

    // The line number is the physical line in the file. It diverges from the
    // record number precisely because of those multi-line quoted fields —
    // which is why guessing offsets from newlines does not work.
    println!(
        "  the last record is number {} but sits on line {:?}",
        index.len() - 1,
        index.record_line(index.len() - 1)
    );

    // Out-of-range lookups are reported, not guessed at.
    println!(
        "  offset of a record past the end: {:?}",
        index.record_offset(index.len())
    );
    println!();
    Ok(())
}

/// Building straight to disk, so the scan happens once, ever.
fn build_and_persist(path: &Path, index_path: &Path) -> Result<(), Box<dyn Error>> {
    println!("== persisting ==");

    // `build_path` streams the source and writes the index as it goes, so a
    // source larger than memory is fine.
    let built = CsvIndex::build_path(path, index_path, IndexOptions::default())?;
    println!(
        "  {} bytes of index for {} bytes of CSV",
        fs::metadata(index_path)?.len(),
        fs::metadata(path)?.len()
    );

    // `load` brings a persisted index back; `save` writes one you already have.
    let reloaded = CsvIndex::load(index_path)?;
    assert_eq!(reloaded.len(), built.len());
    println!(
        "  reloaded {} records without rescanning the CSV",
        reloaded.len()
    );
    println!();
    Ok(())
}

/// Reading the index without loading all of it into memory.
fn from_disk(index_path: &Path, path: &Path) -> Result<(), Box<dyn Error>> {
    println!("== lazy index reader ==");

    // `CsvIndex` materializes the whole location table, 16 bytes per record.
    // `CsvIndexReader` leaves it on disk and reads one position per lookup, so
    // random access over a billion-record source costs constant memory.
    let mut reader = CsvIndexReader::open(index_path)?;
    println!(
        "  {} records; offset of record 500 is {:?}",
        reader.len(),
        reader.record_offset(500)?
    );

    let mut parser = reader.parser_at_path(path, 500)?;
    let mut line = parser.next_line()?.expect("record 500");
    println!("  record 500: {:?}", line.record()?.get_str(1)?);

    // Opening checks the header and length; `verify` checks the whole-index
    // checksum, which necessarily reads every position.
    reader.verify()?;
    println!("  checksum verified");
    println!();
    Ok(())
}

/// An index is only valid for the bytes it was built from.
fn staleness(index: &CsvIndex, path: &Path) -> Result<(), Box<dyn Error>> {
    println!("== staleness ==");

    // Validation compares length and content hash, so a file that changed
    // under you is reported rather than silently yielding the wrong records.
    let source = fs::read(path)?;
    println!(
        "  matches the source: {}",
        index.validate_source(&source).is_ok()
    );

    let mut file = fs::OpenOptions::new().append(true).open(path)?;
    writeln!(file, "1000,row-1000,appended")?;
    file.flush()?;
    drop(file);

    // `validate_reader` does the same check by streaming, without holding the
    // source in memory.
    match index.validate_reader(fs::File::open(path)?) {
        Ok(()) => println!("  still matches (unexpected)"),
        Err(error) => println!("  after appending: {:?}", error.kind()),
    }
    println!();
    Ok(())
}
