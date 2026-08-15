//! Reading a few columns out of a wide file.
//!
//! A [`FieldProjection`] names the columns you care about once, then turns
//! every record into just those fields in the order you asked for. It is the
//! natural companion to a 200-column export where you want four of them: the
//! parser still finds the record boundaries, but you never index by a magic
//! number or re-derive positions per row.
//!
//! Projection is resolved once against the header row, so a column moving
//! sideways in next quarter's export changes nothing in your code.
//!
//! Run with: `cargo run --example projection`

use coseva::FieldProjection;
use coseva::SliceParser;
use coseva::config::ParseOptions;
use coseva::format::Csv;
use std::error::Error;
use std::str;

const DATA: &[u8] = b"\
id,first_name,last_name,email,street,city,state,postcode,country,phone,signup,plan
1,Ada,Lovelace,ada@example.com,1 Analytical Way,London,,NW1,GB,+44,2024-01-02,pro
2,Grace,Hopper,grace@example.com,2 Compiler Ct,Arlington,VA,22201,US,+1,2024-03-14,team
3,Alan,Turing,alan@example.com,3 Bombe Blvd,Wilmslow,,SK9,GB,+44,2024-07-30,free
";

fn main() -> Result<(), Box<dyn Error>> {
    by_name()?;
    by_position()?;
    reordering()?;
    Ok(())
}

/// The usual form: resolve names against the header row, once.
fn by_name() -> Result<(), Box<dyn Error>> {
    println!("== projection by name ==");
    let mut parser = SliceParser::<Csv>::new(DATA, ParseOptions::new()).expect("parser");

    // The parser consumed the header row; ask it for the names it saw.
    let headers = parser
        .headers()?
        .expect("this document has headers")
        .clone();
    let projection = FieldProjection::from_headers(&headers, ["email", "city", "plan"])?;
    println!("  resolved to columns {:?}", projection.indices());

    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;
        // `project` yields one `Option<&[u8]>` per requested column, in the
        // order requested. `None` means the record was too short.
        let fields: Vec<&str> = record
            .project(&projection)
            .map(|field| field.map_or("<missing>", |bytes| str::from_utf8(bytes).unwrap_or("")))
            .collect();
        println!("  {fields:?}");
    }
    println!();
    Ok(())
}

/// Positional, for headerless documents or a fixed schema.
fn by_position() -> Result<(), Box<dyn Error>> {
    println!("== projection by position ==");
    let projection = FieldProjection::new([0, 5, 8]);

    let mut parser = SliceParser::<Csv>::new(DATA, ParseOptions::new()).expect("parser");
    let _ = parser.headers()?;
    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;
        let fields: Vec<String> = record
            .project(&projection)
            .map(|field| String::from_utf8_lossy(field.unwrap_or(b"")).into_owned())
            .collect();
        println!("  {fields:?}");
    }
    println!();
    Ok(())
}

/// The projection order is yours; it need not match the file.
fn reordering() -> Result<(), Box<dyn Error>> {
    println!("== reordering and duplicates ==");
    let mut parser = SliceParser::<Csv>::new(DATA, ParseOptions::new()).expect("parser");
    let headers = parser.headers()?.expect("headers").clone();

    // Reverse the natural order, and ask for a column twice.
    let projection =
        FieldProjection::from_headers(&headers, ["last_name", "first_name", "last_name"])?;

    let mut line = parser.next_line()?.expect("one record");
    let record = line.record()?;
    let fields: Vec<String> = record
        .project(&projection)
        .map(|field| String::from_utf8_lossy(field.unwrap_or(b"")).into_owned())
        .collect();
    println!("  {fields:?}");

    // A name that is not there is reported when the projection is built,
    // not silently per record.
    let error = FieldProjection::from_headers(&headers, ["nonexistent"])
        .expect_err("that column does not exist");
    println!("  unknown column: {:?}", error.kind());
    println!();
    Ok(())
}
