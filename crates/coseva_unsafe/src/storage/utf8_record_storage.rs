use core::mem;
use core::ops::Range;
use core::str;

use crate::bytes::is_ascii;

use super::{RecordStorage, TextValidity, end_offset};

#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct Utf8RecordStorage {
    storage: RecordStorage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Utf8RecordError {
    InvalidField { index: usize, error: str::Utf8Error },
}

struct Utf8RefillRollback<'storage> {
    storage: &'storage mut RecordStorage,
}

impl Drop for Utf8RefillRollback<'_> {
    fn drop(&mut self) {
        self.storage.clear();
    }
}

fn validate_utf8(storage: &RecordStorage) -> Result<(), Utf8RecordError> {
    let bytes = storage.bytes();
    if is_ascii(bytes) {
        return Ok(());
    }
    let text = match str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => {
            let Some((index, field)) = storage
                .iter()
                .enumerate()
                .find(|(_, field)| str::from_utf8(field).is_err())
            else {
                return Err(Utf8RecordError::InvalidField {
                    index: storage.len(),
                    error,
                });
            };
            let error =
                str::from_utf8(field).expect_err("the selected field contains an invalid sequence");
            return Err(Utf8RecordError::InvalidField { index, error });
        }
    };
    if let Some((index, (field, _))) = storage
        .iter()
        .zip(storage.ends())
        .enumerate()
        .find(|entry| !text.is_char_boundary(end_offset(*entry.1.1)))
    {
        let error =
            str::from_utf8(field).expect_err("a field ending within a code point must be invalid");
        return Err(Utf8RecordError::InvalidField { index, error });
    }
    Ok(())
}

fn validate_utf8_storage(storage: &RecordStorage) -> Result<(), Utf8RecordError> {
    validate_utf8(storage)
}

impl Utf8RecordStorage {
    #[inline]
    pub const fn new() -> Self {
        Self {
            storage: RecordStorage::new(),
        }
    }

    pub fn with_capacity(fields: usize, bytes: usize) -> Self {
        Self {
            storage: RecordStorage::with_capacity(fields, bytes),
        }
    }

    #[inline(always)]
    pub fn try_from_storage(
        storage: RecordStorage,
    ) -> Result<Self, (RecordStorage, Utf8RecordError)> {
        if let Err(error) = validate_utf8(&storage) {
            return Err((storage, error));
        }
        Ok(Self { storage })
    }

    pub fn into_storage(self) -> RecordStorage {
        self.storage
    }

    #[inline(always)]
    pub fn refill_with<E>(
        &mut self,
        refill: impl FnOnce(&mut RecordStorage) -> Result<(), E>,
    ) -> Result<Result<(), E>, Utf8RecordError> {
        self.storage.clear();
        let rollback = Utf8RefillRollback {
            storage: &mut self.storage,
        };
        match refill(rollback.storage) {
            Ok(()) => {
                validate_utf8_storage(rollback.storage)?;
                mem::forget(rollback);
                Ok(Ok(()))
            }
            Err(error) => Ok(Err(error)),
        }
    }

    #[inline(always)]
    pub fn refill_with_validity<E>(
        &mut self,
        refill: impl FnOnce(&mut RecordStorage) -> Result<TextValidity, E>,
    ) -> Result<Result<(), E>, Utf8RecordError> {
        self.storage.clear();
        let rollback = Utf8RefillRollback {
            storage: &mut self.storage,
        };
        match refill(rollback.storage) {
            Ok(validity) => {
                if validity != TextValidity::Ascii {
                    validate_utf8_storage(rollback.storage)?;
                }
                mem::forget(rollback);
                Ok(Ok(()))
            }
            Err(error) => Ok(Err(error)),
        }
    }

    #[inline]
    pub const fn len(&self) -> usize {
        self.storage.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.storage.is_empty()
    }

    #[inline(always)]
    pub fn get(&self, index: usize) -> Option<&str> {
        let &end = self.storage.ends().get(index)?;
        let start = if index == 0 {
            0
        } else {
            // SAFETY: finding endpoint `index` above proves `index - 1` exists.
            unsafe { *self.storage.ends().get_unchecked(index - 1) }
        };
        // SAFETY: `RecordStorage` maintains ordered endpoints within `bytes`,
        // and UTF-8 construction validates that every endpoint is a character
        // boundary.
        let bytes = unsafe {
            self.storage
                .bytes()
                .get_unchecked(end_offset(start)..end_offset(end))
        };
        // SAFETY: construction validates the complete buffer and every field
        // endpoint, while all subsequent mutations accept only `str` values.
        Some(unsafe { str::from_utf8_unchecked(bytes) })
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        // SAFETY: the storage invariant guarantees the complete buffer is
        // valid UTF-8.
        unsafe { str::from_utf8_unchecked(self.storage.bytes()) }
    }

    pub fn bytes(&self) -> &[u8] {
        self.storage.bytes()
    }

    pub fn ends(&self) -> &[usize] {
        self.storage.ends()
    }

    pub fn range(&self, index: usize) -> Option<Range<usize>> {
        self.storage.range(index)
    }

    pub fn is_null(&self, index: usize) -> Option<bool> {
        self.storage.is_null(index)
    }

    pub const fn null_aware(&self) -> bool {
        self.storage.null_aware()
    }

    pub fn set_null_aware(&mut self, null_aware: bool) {
        self.storage.set_null_aware(null_aware);
    }

    pub fn byte_range(&self) -> Range<usize> {
        self.storage.byte_range()
    }

    pub const fn index(&self) -> u64 {
        self.storage.index()
    }

    pub fn set_location(&mut self, byte_range: Range<usize>, index: u64) {
        self.storage.set_location(byte_range, index);
    }

    pub const fn byte_capacity(&self) -> usize {
        self.storage.byte_capacity()
    }

    pub const fn field_capacity(&self) -> usize {
        self.storage.field_capacity()
    }

    pub fn reserve(&mut self, additional_fields: usize, additional_bytes: usize) {
        self.storage.reserve(additional_fields, additional_bytes);
    }

    pub fn clear(&mut self) {
        self.storage.clear();
    }

    pub fn shrink_to_fit(&mut self) {
        self.storage.shrink_to_fit();
    }

    pub fn push_field(&mut self, field: &str, max_offset: usize) {
        self.storage.push_field(field.as_bytes(), max_offset);
    }

    pub fn push_null(&mut self, max_offset: usize) {
        self.storage.push_null(max_offset);
    }

    pub fn set_field(&mut self, index: usize, field: &str, max_offset: usize) -> bool {
        self.storage.set_field(index, field.as_bytes(), max_offset)
    }

    #[inline(always)]
    pub fn try_set_field_equal(&mut self, index: usize, field: &str) -> Option<bool> {
        self.storage.try_set_field_equal(index, field.as_bytes())
    }

    pub fn set_null(&mut self, index: usize, max_offset: usize) -> bool {
        self.storage.set_null(index, max_offset)
    }

    pub fn truncate(&mut self, len: usize) {
        self.storage.truncate(len);
    }
}
