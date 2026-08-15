//! The fused and general decode paths must agree on every input.
//!
//! `#[derive(CsvDecode)]` sets `FUSED_ARITY`, so derived targets take the
//! fused path whenever the file's columns already sit in declaration order.
//! A hand-written `CsvDecode` impl leaves `FUSED_ARITY` at its `None` default
//! and therefore always takes the general path. Decoding the same bytes into
//! a derived struct and into a hand-written twin with identical fields
//! compares the two paths directly: any divergence, in values or in error
//! text, means the fused route is not a faithful specialization.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use core::fmt::Write as _;

use coseva::config::{FormatOptions, Headers, ParseOptions};
use coseva::encoding::{CsvDecode, DecodeField, DecodeRecord};
use coseva::{Error, SliceParser};

#[derive(Debug, PartialEq)]
struct Row {
    city: String,
    country: String,
    ordinal: u32,
    population: u64,
    active: bool,
}

/// The derived twin. Identical fields, so it must decode identically.
#[derive(Debug, PartialEq, CsvDecode)]
struct Derived {
    city: String,
    country: String,
    ordinal: u32,
    population: u64,
    active: bool,
}

/// The hand-written twin, which never opts into fusion.
impl<'record> CsvDecode<'record> for Row {
    fn csv_decode<R>(record: &R) -> Result<Self, Error>
    where
        R: DecodeRecord<'record> + ?Sized,
    {
        Ok(Self {
            city: String::decode_field_from_record(record, 0, "city")?,
            country: String::decode_field_from_record(record, 1, "country")?,
            ordinal: u32::decode_field_from_record(record, 2, "ordinal")?,
            population: u64::decode_field_from_record(record, 3, "population")?,
            active: bool::decode_field_from_record(record, 4, "active")?,
        })
    }

    fn field_names() -> &'static [&'static str] {
        &["city", "country", "ordinal", "population", "active"]
    }
}

fn decode_all<T>(input: &[u8], headers: Headers) -> Result<Vec<T>, String>
where
    T: for<'r> CsvDecode<'r>,
{
    let mut parser = SliceParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(headers),
    )
    .map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    loop {
        match parser.next_line() {
            Ok(Some(mut line)) => out.push(line.decoded::<T>().map_err(|e| e.to_string())?),
            Ok(None) => return Ok(out),
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// Decode `input` both ways and require the outcomes to match.
fn agree(input: &[u8], headers: Headers) {
    let fused = decode_all::<Derived>(input, headers.clone());
    let classic = decode_all::<Row>(input, headers);
    match (fused, classic) {
        (Ok(fused), Ok(classic)) => {
            let fused: Vec<Row> = fused
                .into_iter()
                .map(|r| Row {
                    city: r.city,
                    country: r.country,
                    ordinal: r.ordinal,
                    population: r.population,
                    active: r.active,
                })
                .collect();
            assert_eq!(fused, classic, "value divergence on {input:?}");
        }
        (Err(fused), Err(classic)) => {
            assert_eq!(fused, classic, "error divergence on {input:?}");
        }
        (fused, classic) => {
            assert_eq!(
                fused.is_ok(),
                classic.is_ok(),
                "outcome divergence on {input:?}: fused={fused:?} classic={classic:?}"
            );
        }
    }
}

const HEADER: &str = "city,country,ordinal,population,active\n";

#[test]
fn agrees_on_well_formed_records() {
    let mut input = String::from(HEADER);
    for index in 0..64_u32 {
        let population = u64::from(index) * 1_000_003_u64;
        let active = index % 2 == 0;
        let _ = writeln!(input, "city{index},c{index},{index},{population},{active}");
    }
    agree(input.as_bytes(), Headers::FirstRecord);
}

#[test]
fn agrees_on_quoted_and_escaped_records() {
    let input = format!(
        "{HEADER}\"a,b\",\"say \"\"hi\"\"\",7,900,true\n\"multi\nline\",x,8,901,false\n\"\",\"\",0,0,1\n"
    );
    agree(input.as_bytes(), Headers::FirstRecord);
}

#[test]
fn agrees_on_headerless_input() {
    // With no header record the mapping is identity by construction, so this
    // exercises the fused path's other admission route.
    agree(b"paris,fr,1,2,true\nlima,pe,2,3,false\n", Headers::None);
}

#[test]
fn agrees_on_permuted_headers() {
    // Declaration order no longer matches the file, so the derived target
    // falls back to the general mapped path. Both sides must still agree.
    let input = "population,active,city,ordinal,country\n900,true,paris,1,fr\n";
    agree(input.as_bytes(), Headers::FirstRecord);
}

#[test]
fn agrees_on_invalid_integers() {
    agree(
        format!("{HEADER}paris,fr,notanumber,900,true\n").as_bytes(),
        Headers::FirstRecord,
    );
}

#[test]
fn agrees_on_invalid_booleans() {
    agree(
        format!("{HEADER}paris,fr,1,900,perhaps\n").as_bytes(),
        Headers::FirstRecord,
    );
}

#[test]
fn agrees_on_invalid_utf8() {
    let mut input = HEADER.as_bytes().to_vec();
    input.extend_from_slice(b"pa\xffris,fr,1,900,true\n");
    agree(&input, Headers::FirstRecord);
}

#[test]
fn agrees_on_short_records() {
    // A record with fewer fields than the struct declares: every conversion
    // past the end sees `None`, on both paths.
    agree(b"paris,fr,1\nlima,pe,2,3,false\n", Headers::None);
}

#[test]
fn agrees_on_empty_input() {
    agree(b"", Headers::None);
    agree(HEADER.as_bytes(), Headers::FirstRecord);
}

#[test]
fn agrees_on_generated_corpus() {
    // A cheap xorshift walk over field shapes that hit the quoted parser,
    // empty fields, and stray whitespace.
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut input = String::from(HEADER);
    for index in 0..2_000_u32 {
        let pick = next() % 5;
        let city = match pick {
            0 => String::from("plain"),
            1 => String::from("\"quoted,comma\""),
            2 => String::from("\"embedded \"\"quote\"\"\""),
            3 => String::new(),
            _ => String::from("  padded  "),
        };
        let _ = writeln!(input, "{city},c,{index},{index},true");
    }
    agree(input.as_bytes(), Headers::FirstRecord);
}

/// Attribute handling must survive the trip through the fused body, which
/// reuses the generated expressions verbatim against a concrete record type.
#[derive(Debug, PartialEq, CsvDecode)]
struct Attrs {
    #[csv(rename = "CityName")]
    city: String,
    #[csv(default)]
    ordinal: u32,
    #[csv(parse_with = "parse_flag")]
    flag: bool,
    #[csv(skip)]
    computed: u8,
}

fn parse_flag(bytes: &[u8]) -> Result<bool, core::num::ParseIntError> {
    let text = core::str::from_utf8(bytes).unwrap_or("0");
    Ok(text.parse::<u8>()? != 0)
}

#[test]
fn fused_body_honors_field_attributes() {
    let input = "CityName,ordinal,flag\nparis,,1\nlima,7,0\n";
    let rows = decode_all::<Attrs>(input.as_bytes(), Headers::FirstRecord).expect("decodes");
    assert_eq!(
        rows,
        vec![
            Attrs {
                city: String::from("paris"),
                ordinal: 0,
                flag: true,
                computed: 0,
            },
            Attrs {
                city: String::from("lima"),
                ordinal: 7,
                flag: false,
                computed: 0,
            },
        ]
    );
}

#[test]
fn fused_arity_counts_only_decoded_fields() {
    // `skip` fields consume no column, so the arity must exclude them.
    assert_eq!(<Attrs as CsvDecode<'_>>::FUSED_ARITY, Some(3));
    assert_eq!(<Derived as CsvDecode<'_>>::FUSED_ARITY, Some(5));
    // A hand-written impl opts out and keeps the general path.
    assert_eq!(<Row as CsvDecode<'_>>::FUSED_ARITY, None);
}
