//! Parsing CSV that is *pushed* to you, one arbitrary chunk at a time.
//!
//! `SliceParser` needs the whole input up front and `IoParser` pulls
//! from a `Read`. Neither works when something else owns the read loop and
//! hands you bytes as they arrive: an async socket, a WASM/JS boundary, an
//! FFI callback, or a decompressor emitting output blocks.
//!
//! `PushParser` inverts the control flow. You lend it whatever bytes you have
//! and then walk the records those bytes completed with the same cursor the
//! other two parsers expose, borrowing each record straight out of the chunk
//! wherever the chunk holds all of it.

use coseva::PushParser;
use coseva::config::ParseOptions;
use coseva::format::Csv;

/// A transport that delivers CSV in chunks we do not control. Note where the
/// splits fall: mid-record, inside a quoted field containing an escaped quote
/// and an embedded newline, and between the CR and LF of a CRLF terminator.
const CHUNKS: &[&[u8]] = &[
    b"sensor,reading,note\r\nrs-1,21.5,ok\r",
    b"\nrs-2,22.0,\"needs ",
    b"attention: \"\"drift\"\"\r\nsince Tuesday\"\r\nrs-3,",
    b"19.75,ok\r\nrs-1,23.5,ok\r\nrs-2,",
    b"24.25,ok",
];

/// Running summary of the readings seen so far.
#[derive(Default)]
struct Readings {
    count: u64,
    sum: f64,
    peak: Option<(String, f64)>,
    notes: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut parser = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
    let mut readings = Readings::default();
    let mut fed = 0usize;

    // The loop is driven by the transport, not by the parser.
    for (i, chunk) in CHUNKS.iter().enumerate() {
        let mut offset = 0;
        let mut completed = 0;
        while offset < chunk.len() {
            // The guard borrows the chunk, so a record lying wholly inside it
            // is read out of the transport's own memory. `done` reports how
            // much was taken, so a chunk carrying more than the record limit
            // allows is offered again until it is drained.
            let mut lent = parser.chunk(&chunk[offset..]);

            while let Some(mut line) = lent.next_line()? {
                let record = line.record()?;
                let sensor = record.get_str(0)?.unwrap_or_default().to_owned();
                let reading: f64 = record.parse(1)?.unwrap_or_default();
                let note = record
                    .get_str(2)?
                    .filter(|note| *note != "ok")
                    .map(str::to_owned);
                completed += 1;

                readings.count += 1;
                readings.sum += reading;
                if readings
                    .peak
                    .as_ref()
                    .is_none_or(|(_, best)| reading > *best)
                {
                    readings.peak = Some((sensor.clone(), reading));
                }
                if let Some(note) = note {
                    readings.notes += 1;
                    // This field was split across two chunks, contains ""
                    // escapes and an embedded newline. It is delivered fully
                    // unescaped, from the one buffer the parser did have to
                    // assemble.
                    assert_eq!(note, "needs attention: \"drift\"\r\nsince Tuesday");
                    println!("  note on {sensor}: {note:?}");
                }
            }

            let accepted = lent.done();
            offset += accepted;
            fed += accepted;
        }
        println!(
            "chunk {i}: fed {:2} bytes -> {completed} record(s) completed",
            chunk.len(),
        );
    }

    // The transport hit EOF. The last record has no terminator, so it is still
    // pending inside the parser until `finish` releases it.
    parser.finish();
    let mut tail = 0;
    let mut lent = parser.chunk(b"");
    while let Some(mut line) = lent.next_line()? {
        let record = line.record()?;
        readings.count += 1;
        readings.sum += record.parse::<f64>(1)?.unwrap_or_default();
        tail += 1;
    }
    drop(lent);
    println!("finish(): {tail} record(s) completed");
    assert!(parser.is_done());

    // Headers are consumed by the parser itself, exactly as the slice and
    // streaming parsers do, so the records above were only ever data records.
    assert_eq!(parser.header_index("reading"), Some(1));
    let headers: Vec<String> = parser
        .headers()
        .map(|record| {
            record
                .iter()
                .map(|field| String::from_utf8_lossy(field).into_owned())
                .collect()
        })
        .unwrap_or_default();
    println!("\nheaders: {headers:?}");
    println!("fed {fed} bytes total across {} chunks", CHUNKS.len());
    let mean = readings.sum / f64::from(u32::try_from(readings.count).unwrap_or(u32::MAX));
    println!("{} readings, mean {mean:.3}", readings.count);
    if let Some((sensor, reading)) = readings.peak {
        println!("peak: {sensor} at {reading}");
    }
    println!("{} reading(s) carried a note", readings.notes);
    Ok(())
}
