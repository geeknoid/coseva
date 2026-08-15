//! Whole-document read entry points.
//!
//! These are the read-side counterparts to [`crate::encode_to_vec`] and its
//! siblings: one call that takes a source and hands back every record in it,
//! rather than a parser to be held in a local and driven.
//!
//! They yield iterators rather than collections. Resident memory then stays
//! proportional to one record however large the document is, matching the
//! write side's streaming behaviour, and a caller who wants the collection is
//! one `.collect()` away. The iterator owns its parser, which is the gap a
//! collection would not have closed: [`crate::SliceParser::decoded_records`]
//! borrows the parser, so its iterator cannot outlive the local holding it.

#[cfg(feature = "std")]
use std::fs::File;
#[cfg(feature = "std")]
use std::io::Read;
#[cfg(feature = "std")]
use std::path::Path;

use crate::config::{FormatOptions, ParseOptions};
use crate::encoding::CsvDecodeOwned;
use crate::error::Error;
use crate::format::Dynamic;
#[cfg(feature = "std")]
use crate::io_parser::IoParser;
use crate::iter::DecodedRecords;
#[cfg(feature = "serde")]
use crate::iter::DeserializedRecords;
use crate::slice_parser::SliceParser;

/// Decode every record of an in-memory document.
///
/// The header record names the columns, so the target type's fields are bound
/// by name and only the ones it names are materialized. `T` must own its
/// fields: an escaped field is unescaped into scratch storage that the next
/// record reuses, so a borrowing type could not be handed out.
///
/// ```
/// # #[cfg(feature = "derive")] {
/// use coseva::config::{FormatOptions, ParseOptions};
/// use coseva::decode_from_slice;
/// use coseva::encoding::CsvDecode;
///
/// #[derive(CsvDecode)]
/// struct City {
///     name: String,
///     pop: u32,
/// }
///
/// let cities: Vec<City> = decode_from_slice(
///     b"name,pop\nBoston,650706\nLondon,8982000\n",
///     FormatOptions::CSV,
///     ParseOptions::new(),
/// )?
/// .collect::<Result<_, _>>()?;
///
/// assert_eq!(cities[1].name, "London");
/// # }
/// # Ok::<(), coseva::Error>(())
/// ```
///
/// # Errors
///
/// Returns a configuration error, or a rejected leading BOM, before any record
/// is read. Per-record decoding errors surface from the iterator.
pub fn decode_from_slice<'input, T, S>(
    input: &'input S,
    format: FormatOptions,
    options: ParseOptions,
) -> Result<impl Iterator<Item = Result<T, Error>> + 'input, Error>
where
    T: CsvDecodeOwned + 'input,
    S: AsRef<[u8]> + ?Sized,
{
    let parser = SliceParser::with_options(input, format, options)?;
    Ok(DecodedRecords::<SliceParser<'input, Dynamic>, T, _>::new(
        parser, None,
    ))
}

/// Decode every record read from `input`.
///
/// Records are pulled one at a time, so resident memory stays flat however
/// long the document is.
///
/// ```
/// # #[cfg(feature = "derive")] {
/// use coseva::config::{FormatOptions, ParseOptions};
/// use coseva::decode_from_reader;
/// use coseva::encoding::CsvDecode;
/// use std::io::Cursor;
///
/// #[derive(CsvDecode)]
/// struct City {
///     name: String,
///     pop: u32,
/// }
///
/// let cities: Vec<City> = decode_from_reader(
///     Cursor::new(b"name,pop\nBoston,650706\n".to_vec()),
///     FormatOptions::CSV,
///     ParseOptions::new(),
/// )?
/// .collect::<Result<_, _>>()?;
///
/// assert_eq!(cities[0].name, "Boston");
/// # }
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
///
/// Returns a configuration error before any record is read. Per-record
/// decoding and I/O errors surface from the iterator.
#[cfg(feature = "std")]
pub fn decode_from_reader<R, T>(
    input: R,
    format: FormatOptions,
    options: ParseOptions,
) -> Result<impl Iterator<Item = Result<T, Error>>, Error>
where
    R: Read,
    T: CsvDecodeOwned,
{
    let parser = IoParser::with_options(input, format, options)?;
    Ok(DecodedRecords::<IoParser<R, Dynamic>, T, _>::new(
        parser, None,
    ))
}

/// Open a file and decode every record in it.
///
/// Resident memory stays flat however large the file is, so this is the entry
/// point for reading a file larger than memory in one call.
///
/// ```
/// # #[cfg(feature = "derive")] {
/// use coseva::config::{FormatOptions, ParseOptions};
/// use coseva::decode_from_path;
/// use coseva::encoding::CsvDecode;
///
/// #[derive(CsvDecode)]
/// struct City {
///     name: String,
///     pop: u32,
/// }
///
/// let directory = tempfile::tempdir()?;
/// let path = directory.path().join("cities.csv");
/// std::fs::write(&path, b"name,pop\nBoston,650706\n")?;
///
/// let cities: Vec<City> =
///     decode_from_path(&path, FormatOptions::CSV, ParseOptions::new())?
///         .collect::<Result<_, _>>()?;
///
/// assert_eq!(cities[0].pop, 650_706);
/// # }
/// # Ok::<(), Box<dyn core::error::Error>>(())
/// ```
///
/// # Errors
///
/// Returns a configuration error, or an error when the file cannot be opened,
/// before any record is read. Per-record decoding and I/O errors surface from
/// the iterator.
#[cfg(feature = "std")]
pub fn decode_from_path<P, T>(
    path: P,
    format: FormatOptions,
    options: ParseOptions,
) -> Result<impl Iterator<Item = Result<T, Error>>, Error>
where
    P: AsRef<Path>,
    T: CsvDecodeOwned,
{
    let parser = IoParser::from_path(path, format, options)?;
    Ok(DecodedRecords::<IoParser<File, Dynamic>, T, _>::new(
        parser, None,
    ))
}

/// Deserialize every record of an in-memory document using Serde.
///
/// [`decode_from_slice`] is the faster path when the type can derive
/// [`CsvDecode`](crate::encoding::CsvDecode).
///
/// ```
/// use coseva::config::{FormatOptions, ParseOptions};
/// use coseva::deserialize_from_slice;
/// use serde::Deserialize;
///
/// #[derive(Deserialize)]
/// struct City {
///     name: String,
///     pop: u32,
/// }
///
/// let cities: Vec<City> = deserialize_from_slice(
///     b"name,pop\nBoston,650706\nLondon,8982000\n",
///     FormatOptions::CSV,
///     ParseOptions::new(),
/// )?
/// .collect::<Result<_, _>>()?;
///
/// assert_eq!(cities[1].pop, 8_982_000);
/// # Ok::<(), coseva::Error>(())
/// ```
///
/// # Errors
///
/// Returns a configuration error, or a rejected leading BOM, before any record
/// is read. Per-record deserialization errors surface from the iterator.
#[cfg(feature = "serde")]
pub fn deserialize_from_slice<'input, T, S>(
    input: &'input S,
    format: FormatOptions,
    options: ParseOptions,
) -> Result<impl Iterator<Item = Result<T, Error>> + 'input, Error>
where
    T: ::serde::de::DeserializeOwned + 'input,
    S: AsRef<[u8]> + ?Sized,
{
    let parser = SliceParser::with_options(input, format, options)?;
    Ok(DeserializedRecords::<SliceParser<'input, Dynamic>, T, _>::new(parser, None))
}

/// Deserialize every record read from `input` using Serde.
///
/// Records are pulled one at a time, so resident memory stays flat however
/// long the document is.
///
/// ```
/// use coseva::config::{FormatOptions, ParseOptions};
/// use coseva::deserialize_from_reader;
/// use serde::Deserialize;
/// use std::io::Cursor;
///
/// #[derive(Deserialize)]
/// struct City {
///     name: String,
///     pop: u32,
/// }
///
/// let cities: Vec<City> = deserialize_from_reader(
///     Cursor::new(b"name,pop\nBoston,650706\n".to_vec()),
///     FormatOptions::CSV,
///     ParseOptions::new(),
/// )?
/// .collect::<Result<_, _>>()?;
///
/// assert_eq!(cities[0].pop, 650_706);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
///
/// Returns a configuration error before any record is read. Per-record
/// deserialization and I/O errors surface from the iterator.
#[cfg(all(feature = "std", feature = "serde"))]
pub fn deserialize_from_reader<R, T>(
    input: R,
    format: FormatOptions,
    options: ParseOptions,
) -> Result<impl Iterator<Item = Result<T, Error>>, Error>
where
    R: Read,
    T: ::serde::de::DeserializeOwned,
{
    let parser = IoParser::with_options(input, format, options)?;
    Ok(DeserializedRecords::<IoParser<R, Dynamic>, T, _>::new(
        parser, None,
    ))
}

/// Open a file and deserialize every record in it using Serde.
///
/// Resident memory stays flat however large the file is.
///
/// ```no_run
/// use coseva::config::{FormatOptions, ParseOptions};
/// use coseva::deserialize_from_path;
/// use serde::Deserialize;
///
/// #[derive(Deserialize)]
/// struct City {
///     name: String,
///     pop: u32,
/// }
///
/// let cities: Vec<City> =
///     deserialize_from_path("cities.csv", FormatOptions::CSV, ParseOptions::new())?
///         .collect::<Result<_, _>>()?;
/// assert_eq!(cities.len(), 2);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
///
/// Returns a configuration error, or an error when the file cannot be opened,
/// before any record is read. Per-record deserialization and I/O errors
/// surface from the iterator.
#[cfg(all(feature = "std", feature = "serde"))]
pub fn deserialize_from_path<P, T>(
    path: P,
    format: FormatOptions,
    options: ParseOptions,
) -> Result<impl Iterator<Item = Result<T, Error>>, Error>
where
    P: AsRef<Path>,
    T: ::serde::de::DeserializeOwned,
{
    let parser = IoParser::from_path(path, format, options)?;
    Ok(DeserializedRecords::<IoParser<File, Dynamic>, T, _>::new(
        parser, None,
    ))
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    struct City {
        name: String,
        pop: u32,
    }

    impl<'record> crate::encoding::CsvDecode<'record> for City {
        fn csv_decode<R>(record: &R) -> Result<Self, crate::error::Error>
        where
            R: crate::encoding::DecodeRecord<'record> + ?Sized,
        {
            let name =
                String::from_utf8_lossy(record.get_field(0).unwrap_or_default()).into_owned();
            let pop_str = String::from_utf8_lossy(record.get_field(1).unwrap_or_default());
            let pop = pop_str.trim().parse().unwrap_or(0);
            Ok(Self { name, pop })
        }

        fn field_names() -> &'static [&'static str] {
            &["name", "pop"]
        }
    }

    #[test]
    fn test_consume_error_paths() {
        let invalid_format = FormatOptions::CSV.delimiter(b'"').quote(b'"');
        let options = ParseOptions::new();
        assert!(decode_from_slice::<City, _>(b"", invalid_format, options.clone()).is_err());

        #[cfg(feature = "std")]
        {
            assert!(
                decode_from_reader::<_, City>(
                    std::io::Cursor::new(b""),
                    invalid_format,
                    options.clone()
                )
                .is_err()
            );
            assert!(
                decode_from_path::<_, City>(
                    "/non_existent_path_coseva_test.csv",
                    FormatOptions::CSV,
                    options.clone()
                )
                .is_err()
            );
        }

        #[cfg(feature = "serde")]
        {
            #[derive(Debug, ::serde::Deserialize)]
            struct Dummy;
            assert!(
                deserialize_from_slice::<Dummy, _>(b"", invalid_format, options.clone()).is_err()
            );

            #[cfg(feature = "std")]
            {
                assert!(
                    deserialize_from_reader::<_, Dummy>(
                        std::io::Cursor::new(b""),
                        invalid_format,
                        options.clone()
                    )
                    .is_err()
                );
                assert!(
                    deserialize_from_path::<_, Dummy>(
                        "/non_existent_path_coseva_test.csv",
                        FormatOptions::CSV,
                        options
                    )
                    .is_err()
                );
            }
        }
    }

    #[test]
    fn test_decode_from_slice() {
        let input = b"name,pop\nBoston,650706\n";
        let res: Vec<City> = decode_from_slice(input, FormatOptions::CSV, ParseOptions::new())
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "Boston");
    }

    #[test]
    #[cfg(feature = "std")]
    fn test_decode_from_reader_and_path() {
        let input = b"name,pop\nBoston,650706\n";
        let res: Vec<City> = decode_from_reader(
            std::io::Cursor::new(input),
            FormatOptions::CSV,
            ParseOptions::new(),
        )
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
        assert_eq!(res.len(), 1);

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("decode.csv");
        std::fs::write(&path, input).unwrap();
        let res2: Vec<City> = decode_from_path(&path, FormatOptions::CSV, ParseOptions::new())
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(res2.len(), 1);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_deserialize_from_slice() {
        #[derive(Debug, PartialEq, Eq, ::serde::Deserialize)]
        struct CitySerde {
            name: String,
            pop: u32,
        }

        let input = b"name,pop\nBoston,650706\n";
        let res: Vec<CitySerde> =
            deserialize_from_slice(input, FormatOptions::CSV, ParseOptions::new())
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].name, "Boston");

        #[cfg(feature = "std")]
        {
            let res_r: Vec<CitySerde> = deserialize_from_reader(
                std::io::Cursor::new(input),
                FormatOptions::CSV,
                ParseOptions::new(),
            )
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
            assert_eq!(res_r.len(), 1);

            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("serde.csv");
            std::fs::write(&path, input).unwrap();
            let res_p: Vec<CitySerde> =
                deserialize_from_path(&path, FormatOptions::CSV, ParseOptions::new())
                    .unwrap()
                    .collect::<Result<_, _>>()
                    .unwrap();
            assert_eq!(res_p.len(), 1);
        }
    }
}
