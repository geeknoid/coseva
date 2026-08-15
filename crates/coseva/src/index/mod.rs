//! Persistent, source-bound record-offset indexes.
//!
//! [`CsvIndex`] holds record positions in memory. [`CsvIndex::create_path`]
//! and [`CsvIndexReader`] keep construction and lookup on disk, allowing
//! constant-memory indexing of sources larger than RAM.
//!
//! ```no_run
//! use coseva::index::{CsvIndex, CsvIndexReader, IndexOptions};
//!
//! CsvIndex::create_path("huge.csv", "huge.idx", IndexOptions::default())?;
//! let mut index = CsvIndexReader::open("huge.idx")?;
//! let mut parser = index.parser_at_path("huge.csv", 4_000_000)?;
//! let mut line = parser
//!     .next_line()?
//!     .ok_or_else(|| std::io::Error::other("expected indexed record"))?;
//! assert_eq!(line.record()?.index(), 4_000_000);
//! # Ok::<(), Box<dyn core::error::Error>>(())
//! ```

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use xxhash_rust::xxh3::Xxh3;

use crate::PushEmitter;
use crate::config::EmitOptions;
use crate::config::{FieldCount, FormatOptions, Headers, Limits, ParseOptions, ReadBom, WriteBom};
use crate::encoding::CsvEncode;
use crate::error::{Error, ErrorKind, Location};
use crate::format::Dynamic;
use crate::{IoParser, PushParser, SliceParser};

mod csv_index;
mod csv_index_reader;
mod format;
mod generate;
mod index_options;

use format::*;

#[doc(inline)]
pub use csv_index::BoundSource;
#[doc(inline)]
pub use csv_index::CsvIndex;
#[doc(inline)]
pub use csv_index_reader::CsvIndexReader;
#[doc(inline)]
pub use index_options::IndexOptions;
