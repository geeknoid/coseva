//! Writing CSV.
//!
//! Encoding mirrors parsing. One core does the quoting and escaping, and three
//! front ends put the bytes somewhere:
//!
//!   * [`IoEmitter`]      — writes to any [`Write`] sink, with buffered output.
//!   * [`VecEmitter`]   — retains the whole document in a `Vec<u8>`.
//!   * [`PushEmitter`]  — hands you the encoded bytes to route yourself.
//!
//! Fields are quoted only when they need to be, which is the cheapest correct
//! choice; [`Quoting`] overrides that when a consumer demands otherwise.
//!
//! Requires the `derive` feature for the `#[derive(CsvEncode)]` section.
//! Run with: `cargo run --example writing --features derive`

use coseva::config::{EmitOptions, FormatOptions, Quoting};
use coseva::encoding::CsvEncode;
use coseva::{ByteRecord, encode_to_vec};
use coseva::{IoEmitter, PushEmitter, VecEmitter};
use std::env;
use std::error::Error;
use std::fs;

fn main() -> Result<(), Box<dyn Error>> {
    field_by_field()?;
    to_a_sink()?;
    from_structs()?;
    quoting_policies()?;
    one_call_helpers()?;
    Ok(())
}

/// The most direct form: hand over an iterator of fields per record.
fn field_by_field() -> Result<(), coseva::Error> {
    println!("== field by field ==");
    let mut emitter = VecEmitter::default();

    emitter.emit_record(["city", "country", "population"])?;
    emitter.emit_record(["Boston", "US", "650706"])?;

    // Anything needing a quote gets one, and embedded quotes are doubled.
    emitter.emit_record(["Washington, D.C.", "US", "689545"])?;
    emitter.emit_record(["L'\"Aquila\"", "IT", "69439"])?;

    print!("{}", String::from_utf8_lossy(emitter.as_bytes()));

    // Or build a record incrementally when the field count is not known
    // up front. `write_null` is distinct from writing an empty field for
    // dialects that distinguish the two.
    let mut emitter = VecEmitter::default();
    let mut pending = emitter.begin_record();
    pending.write_field("Reykjavik")?;
    pending.write_field("IS")?;
    pending.write_null()?;
    pending.finish()?;
    print!(
        "  incremental: {}",
        String::from_utf8_lossy(emitter.as_bytes())
    );
    println!();
    Ok(())
}

/// Streaming to a sink, with the buffering the sink deserves.
fn to_a_sink() -> Result<(), Box<dyn Error>> {
    println!("== to a writer and a file ==");

    // Any `Write` works; output is buffered, so small records do not turn
    // into small writes.
    let mut emitter = IoEmitter::with_options(Vec::new(), FormatOptions::CSV, EmitOptions::new())?;
    emitter.emit_record(["id", "value"])?;
    for id in 0..3 {
        emitter.emit_record([id.to_string(), format!("{}", id * id)])?;
    }
    // `into_inner` flushes and hands the sink back, reporting a flush failure
    // without losing the writer.
    let bytes = emitter.into_inner()?;
    print!("{}", String::from_utf8_lossy(&bytes));

    let path = env::temp_dir().join("coseva_example_writing.csv");
    let mut emitter = IoEmitter::to_path(&path, FormatOptions::CSV, EmitOptions::new())?;
    emitter.emit_record(["written", "to-a-file"])?;
    emitter.flush()?;
    drop(emitter);
    println!(
        "  wrote {} bytes to {}",
        fs::metadata(&path)?.len(),
        path.display()
    );
    fs::remove_file(&path)?;

    // A reusable owned record can be encoded directly, closing the loop with
    // the reader's `read_byte_record_into`.
    let mut record = ByteRecord::new();
    record.push_field("round");
    record.push_field("trip");
    let mut emitter = VecEmitter::default();
    emitter.emit_byte_record(&record)?;
    print!(
        "  from a ByteRecord: {}",
        String::from_utf8_lossy(emitter.as_bytes())
    );
    println!();
    Ok(())
}

/// `#[derive(CsvEncode)]` writes the header row and the values from one type.
#[derive(CsvEncode)]
struct Reading {
    #[csv(rename = "sensor_id")]
    id: u32,
    celsius: f64,
    #[csv(format_with = "format_status")]
    status: Status,
    #[csv(skip)]
    _internal: (),
}

#[derive(Clone, Copy)]
enum Status {
    Ok,
    Drifting,
}

/// `format_with` callbacks take a reference and return the raw field bytes.
/// The signature is fixed by the attribute, hence the reference to a `Copy` type.
#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "the `format_with` attribute fixes this signature"
)]
fn format_status(status: &Status) -> Vec<u8> {
    match status {
        Status::Ok => b"ok".to_vec(),
        Status::Drifting => b"drifting".to_vec(),
    }
}

fn from_structs() -> Result<(), coseva::Error> {
    println!("== from structs ==");
    let readings = [
        Reading {
            id: 1,
            celsius: 21.5,
            status: Status::Ok,
            _internal: (),
        },
        Reading {
            id: 2,
            celsius: 22.0,
            status: Status::Drifting,
            _internal: (),
        },
    ];

    let mut emitter = VecEmitter::default();
    // The header row comes from the field names, so it cannot drift out of
    // sync with the values written after it.
    emitter.encode_header::<Reading>()?;
    emitter.encode_all(readings)?;

    print!("{}", String::from_utf8_lossy(emitter.as_bytes()));
    println!();
    Ok(())
}

/// When a downstream consumer insists on a particular quoting shape.
fn quoting_policies() -> Result<(), coseva::Error> {
    println!("== quoting policies ==");
    for (label, quoting) in [
        ("necessary (default)", Quoting::Necessary),
        ("always", Quoting::Always),
        ("non-numeric", Quoting::NonNumeric),
    ] {
        let format = FormatOptions::CSV.quoting(quoting);
        let mut emitter = VecEmitter::with_options(Vec::new(), format, EmitOptions::new())?;
        emitter.emit_record(["Boston", "650706", "has, comma"])?;
        print!(
            "  {label:>19}: {}",
            String::from_utf8_lossy(emitter.as_bytes())
        );
    }
    println!();
    Ok(())
}

/// One-call helpers, for when the whole document is already in hand.
fn one_call_helpers() -> Result<(), Box<dyn Error>> {
    println!("== one-call helpers ==");
    let readings = [
        Reading {
            id: 7,
            celsius: 19.75,
            status: Status::Ok,
            _internal: (),
        },
        Reading {
            id: 8,
            celsius: 24.25,
            status: Status::Ok,
            _internal: (),
        },
    ];

    // `encode_to_vec` writes the header row and every value in a single call.
    let bytes = encode_to_vec(readings, FormatOptions::CSV, EmitOptions::new())?;
    print!("{}", String::from_utf8_lossy(&bytes));

    // A `PushEmitter` when you own the delivery: encode, take the bytes,
    // clear, repeat. Nothing is written anywhere you did not put it.
    let mut emitter = PushEmitter::default();
    emitter.emit_record(["chunk", "one"])?;
    let chunk = emitter.buffer().to_vec();
    emitter.clear();
    print!("  push chunk: {}", String::from_utf8_lossy(&chunk));
    println!();
    Ok(())
}
