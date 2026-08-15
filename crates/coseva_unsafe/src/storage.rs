//! Invariant-owning storage for byte and UTF-8 records.

mod record_storage;
mod utf8_record_storage;

pub use record_storage::{
    MAX_FIELD_OFFSET, RecordStorage, TextValidity, encode_end_bounded, end_is_null, end_offset,
};
pub use utf8_record_storage::{Utf8RecordError, Utf8RecordStorage};

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests;
