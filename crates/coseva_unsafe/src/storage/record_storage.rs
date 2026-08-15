//! Invariant-owning storage for byte and UTF-8 records.

use alloc::vec::Vec;
use core::hash::{Hash, Hasher};
use core::ops::Range;

const NULL_END_FLAG: usize = 1 << (usize::BITS - 1);
pub const MAX_FIELD_OFFSET: usize = NULL_END_FLAG - 1;
const EMPTY_BYTE_RANGE: Range<usize> = Range {
    start: usize::MIN,
    end: usize::MIN,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TextValidity {
    Ascii,
    #[default]
    Unknown,
}

pub const fn end_offset(raw: usize) -> usize {
    raw & !NULL_END_FLAG
}

pub const fn end_is_null(raw: usize) -> bool {
    raw & NULL_END_FLAG != 0
}

const fn encode_end(offset: usize, is_null: bool) -> usize {
    if is_null {
        offset | NULL_END_FLAG
    } else {
        offset
    }
}

fn assert_offset_bounded(offset: usize, max_offset: usize) {
    assert!(
        offset <= max_offset,
        "field offset overflows into the NULL flag bit, which would decode as a \
         NULL field at a wrong offset"
    );
}

pub fn encode_end_bounded(offset: usize, is_null: bool, max_offset: usize) -> usize {
    assert_offset_bounded(offset, max_offset);
    if is_null {
        offset | NULL_END_FLAG
    } else {
        offset
    }
}

#[derive(Clone, Debug, Default)]
pub struct RecordStorage {
    bytes: Vec<u8>,
    ends: Vec<usize>,
    null_aware: bool,
    byte_range: Range<usize>,
    index: u64,
    text_validity: TextValidity,
}

impl PartialEq for RecordStorage {
    fn eq(&self, other: &Self) -> bool {
        self.ends == other.ends && self.bytes == other.bytes
    }
}

impl Eq for RecordStorage {}

impl Hash for RecordStorage {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.ends.hash(state);
        self.bytes.hash(state);
    }
}

impl RecordStorage {
    #[inline]
    pub const fn new() -> Self {
        Self {
            bytes: Vec::new(),
            ends: Vec::new(),
            null_aware: false,
            byte_range: EMPTY_BYTE_RANGE,
            index: u64::MIN,
            text_validity: TextValidity::Unknown,
        }
    }

    pub fn with_capacity(fields: usize, bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(bytes),
            ends: Vec::with_capacity(fields),
            null_aware: false,
            byte_range: EMPTY_BYTE_RANGE,
            index: u64::MIN,
            text_validity: TextValidity::Unknown,
        }
    }

    #[inline]
    pub const fn len(&self) -> usize {
        self.ends.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    #[inline]
    pub const fn bytes_len(&self) -> usize {
        self.bytes.len()
    }

    #[inline]
    pub const fn byte_capacity(&self) -> usize {
        self.bytes.capacity()
    }

    #[inline]
    pub const fn field_capacity(&self) -> usize {
        self.ends.capacity()
    }

    #[inline]
    pub const fn is_unallocated(&self) -> bool {
        self.ends.capacity() == 0
    }

    #[inline]
    pub const fn null_aware(&self) -> bool {
        self.null_aware
    }

    #[inline]
    pub fn reset_text_validity(&mut self) {
        self.text_validity = TextValidity::Unknown;
    }

    #[inline]
    pub fn certify_ascii(&mut self) {
        self.text_validity = TextValidity::Ascii;
    }

    #[inline]
    pub const fn text_validity(&self) -> TextValidity {
        self.text_validity
    }

    #[inline]
    pub fn set_null_aware(&mut self, null_aware: bool) {
        self.null_aware = null_aware;
    }

    #[inline]
    pub fn byte_range(&self) -> Range<usize> {
        self.byte_range.clone()
    }

    #[inline]
    pub const fn index(&self) -> u64 {
        self.index
    }

    #[inline]
    pub fn set_location(&mut self, byte_range: Range<usize>, index: u64) {
        self.byte_range = byte_range;
        self.index = index;
    }

    pub fn invalidate_source_metadata(&mut self) {
        self.byte_range.clone_from(&EMPTY_BYTE_RANGE);
        self.index.clone_from(&u64::MIN);
    }

    pub fn reserve(&mut self, additional_fields: usize, additional_bytes: usize) {
        self.bytes.reserve(additional_bytes);
        self.ends.reserve(additional_fields);
    }

    #[inline]
    pub fn reserve_storage(&mut self, fields: usize, bytes: usize) {
        self.ends.reserve(fields.saturating_sub(self.ends.len()));
        self.bytes.reserve(bytes.saturating_sub(self.bytes.len()));
    }

    #[inline]
    pub fn clear(&mut self) {
        self.clear_fields();
        self.invalidate_source_metadata();
    }

    #[inline]
    pub fn clear_fields(&mut self) {
        self.bytes.clear();
        self.ends.clear();
        self.null_aware = bool::default();
    }

    #[inline]
    pub fn truncate_storage(&mut self, fields: usize, bytes: usize) {
        assert!(fields <= self.ends.len());
        assert!(bytes <= self.bytes.len());
        let retained_end = fields
            .checked_sub(1)
            .map_or(0, |previous| end_offset(self.ends[previous]));
        assert!(retained_end <= bytes);
        self.ends.truncate(fields);
        self.bytes.truncate(bytes);
    }

    pub fn shrink_to_fit(&mut self) {
        self.bytes.shrink_to_fit();
        self.ends.shrink_to_fit();
    }

    pub fn reclaim(&mut self) {
        Self::reclaim_buffer(&mut self.bytes);
        Self::reclaim_buffer(&mut self.ends);
    }

    #[inline(always)]
    pub fn get(&self, index: usize) -> Option<&[u8]> {
        let &end = self.ends.get(index)?;
        let start = index
            .checked_sub(1)
            .map_or(0, |previous| self.ends[previous]);
        let start = end_offset(start);
        let end = end_offset(end);
        debug_assert!(start <= end && end <= self.bytes.len());
        // SAFETY: every storage mutation preserves ordered endpoints within
        // `bytes`; the checked endpoint lookup also proves the previous
        // endpoint exists when `index` is nonzero.
        Some(unsafe { self.bytes.get_unchecked(start..end) })
    }

    fn reclaim_buffer<T>(buffer: &mut Vec<T>) {
        const FLOOR: usize = 8 * 1024;
        const EXCESS_FACTOR: usize = 4;
        let capacity = buffer.capacity();
        let live = buffer.len();
        let keep = live.max(FLOOR);
        if capacity <= keep.saturating_mul(EXCESS_FACTOR) {
            return;
        }

        buffer.shrink_to(keep);
    }

    pub fn range(&self, index: usize) -> Option<Range<usize>> {
        let &end = self.ends.get(index)?;
        let start = index
            .checked_sub(1)
            .map_or(0, |previous| self.ends[previous]);
        Some(end_offset(start)..end_offset(end))
    }

    pub fn is_null(&self, index: usize) -> Option<bool> {
        self.ends.get(index).map(|&raw| end_is_null(raw))
    }

    #[inline]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn as_slice(&self) -> &[u8] {
        self.bytes()
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &[u8]> {
        (0..self.len()).map(|index| self.get(index).expect("index is in range"))
    }

    pub fn ends(&self) -> &[usize] {
        &self.ends
    }

    pub fn clone_from(&mut self, source: &Self) {
        self.bytes.clone_from(&source.bytes);
        self.ends.clone_from(&source.ends);
        self.null_aware = source.null_aware;
        self.byte_range = source.byte_range.clone();
        self.index = source.index;
        self.text_validity = source.text_validity;
    }

    pub fn push_field(&mut self, field: &[u8], max_offset: usize) {
        let new_len = self
            .bytes
            .len()
            .checked_add(field.len())
            .expect("field byte length overflow");
        let end = encode_end_bounded(new_len, false, max_offset);
        self.bytes.extend_from_slice(field);
        self.ends.push(end);
    }

    pub fn push_null(&mut self, max_offset: usize) {
        self.null_aware = true;
        self.ends
            .push(encode_end_bounded(self.bytes.len(), true, max_offset));
    }

    pub fn set_field(&mut self, index: usize, field: &[u8], max_offset: usize) -> bool {
        let Some(&end_raw) = self.ends.get(index) else {
            return false;
        };
        let end = end_offset(end_raw);
        let start = index
            .checked_sub(1)
            .map_or(0, |previous| end_offset(self.ends[previous]));
        let old_len = end - start;
        let new_total = self
            .bytes
            .len()
            .checked_sub(old_len)
            .and_then(|len| len.checked_add(field.len()))
            .expect("field byte length overflow");
        assert_offset_bounded(new_total, max_offset);

        self.bytes.splice(start..end, field.iter().copied());
        let delta = field.len() as isize - old_len as isize;
        for endpoint in &mut self.ends[index..] {
            let offset = end_offset(*endpoint)
                .checked_add_signed(delta)
                .expect("validated field replacement keeps endpoints in range");
            *endpoint = encode_end(offset, end_is_null(*endpoint));
        }
        self.ends[index] = encode_end(end_offset(self.ends[index]), false);
        true
    }

    #[inline(always)]
    pub fn try_set_field_equal(&mut self, index: usize, field: &[u8]) -> Option<bool> {
        let &end_raw = self.ends.get(index)?;
        let end = end_offset(end_raw);
        let start = index
            .checked_sub(1)
            .map_or(0, |previous| end_offset(self.ends[previous]));
        if field.len() != end - start {
            return Some(false);
        }
        self.bytes[start..end].copy_from_slice(field);
        self.ends[index] = end;
        Some(true)
    }

    pub fn set_null(&mut self, index: usize, max_offset: usize) -> bool {
        if !self.set_field(index, &[], max_offset) {
            return false;
        }
        let offset = end_offset(self.ends[index]);
        self.ends[index] = encode_end(offset, true);
        self.null_aware = true;
        true
    }

    #[inline(always)]
    pub fn truncate(&mut self, len: usize) {
        let Some(retained) = self.ends.get(..len) else {
            return;
        };
        let bytes = retained.last().map_or(usize::MIN, |&end| end_offset(end));
        self.ends.truncate(len);
        self.bytes.truncate(bytes);
    }

    #[inline(always)]
    pub fn extend_bytes(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    #[inline]
    pub fn append_field(&mut self, field: &[u8]) {
        self.bytes.extend_from_slice(field);
        self.ends.push(encode_end(self.bytes.len(), false));
    }

    #[inline]
    pub fn append_short_field(&mut self, field: &[u8]) {
        crate::bytes::append_short(&mut self.bytes, field);
        self.ends.push(encode_end(self.bytes.len(), false));
    }

    #[inline]
    pub fn append_empty_fields(&mut self, count: usize) {
        let end = encode_end(self.bytes.len(), false);
        self.ends.resize(self.ends.len() + count, end);
    }

    #[inline]
    pub fn append_null_field(&mut self) {
        self.null_aware = true;
        self.ends.push(encode_end(self.bytes.len(), true));
    }

    #[inline]
    pub fn push_byte(&mut self, byte: u8) {
        self.bytes.push(byte);
    }

    #[inline]
    pub fn finish_field(&mut self) {
        self.ends.push(encode_end(self.bytes.len(), false));
    }

    #[inline]
    pub fn mark_null_fields(&mut self, mut is_null: impl FnMut(&[u8]) -> bool) {
        debug_assert!(!self.null_aware);
        let mut source_start = 0;
        let mut destination = 0;
        for index in 0..self.ends.len() {
            let source_end = self.ends[index];
            let field = &self.bytes[source_start..source_end];
            let null = is_null(field);
            if null {
                self.null_aware = true;
            } else {
                self.bytes
                    .copy_within(source_start..source_end, destination);
                destination += source_end - source_start;
            }
            self.ends[index] = encode_end(destination, null);
            source_start = source_end;
        }
        self.bytes.truncate(destination);
    }

    #[inline]
    pub fn trim_fields_ascii(&mut self) {
        let mut source_start = 0;
        let mut destination = 0;
        for index in 0..self.ends.len() {
            let raw_end = self.ends[index];
            let source_end = end_offset(raw_end);
            if end_is_null(raw_end) {
                self.ends[index] = encode_end(destination, true);
                continue;
            }
            let mut trimmed_start = source_start;
            let mut trimmed_end = source_end;
            // gamma::skip(cond.negate, reason = "mutation causes non-termination or unbounded resource use")
            while trimmed_start < trimmed_end && self.bytes[trimmed_start].is_ascii_whitespace() {
                // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
                trimmed_start += 1;
            }
            while trimmed_end > trimmed_start && self.bytes[trimmed_end - 1].is_ascii_whitespace() {
                // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
                trimmed_end -= 1;
            }
            self.bytes
                .copy_within(trimmed_start..trimmed_end, destination);
            destination += trimmed_end - trimmed_start;
            self.ends[index] = encode_end(destination, false);
            source_start = source_end;
        }
        self.bytes.truncate(destination);
    }

    pub(crate) fn parts_mut(&mut self) -> (&mut Vec<u8>, &mut Vec<usize>) {
        (&mut self.bytes, &mut self.ends)
    }
}

#[cfg(test)]
mod tests {
    use super::RecordStorage;

    #[test]
    fn capacity_operations_hit_exact_reclaim_boundaries() {
        const FLOOR: usize = 8 * 1024;

        let mut reserved = RecordStorage::new();
        reserved.reserve_storage(17, 19);
        assert!(reserved.field_capacity() >= 17);
        assert!(reserved.byte_capacity() >= 19);

        let mut additional = RecordStorage::new();
        additional.reserve(23, 29);
        assert!(additional.field_capacity() >= 23);
        assert!(additional.byte_capacity() >= 29);

        let mut shrink = RecordStorage::with_capacity(1000, 1000);
        shrink.append_field(b"x");
        let old_fields = shrink.field_capacity();
        let old_bytes = shrink.byte_capacity();
        shrink.shrink_to_fit();
        assert!(shrink.field_capacity() < old_fields);
        assert!(shrink.byte_capacity() < old_bytes);
        assert!(shrink.field_capacity() >= 1);
        assert!(shrink.byte_capacity() >= 1);

        fn buffer(capacity: usize) -> alloc::vec::Vec<u8> {
            let buffer = alloc::vec::Vec::with_capacity(capacity);
            assert_eq!(buffer.capacity(), capacity);
            buffer
        }

        let mut at_factor = buffer(FLOOR * 4);
        RecordStorage::reclaim_buffer(&mut at_factor);
        assert_eq!(at_factor.capacity(), FLOOR * 4);

        let mut above_factor = buffer(FLOOR * 4 + 1);
        RecordStorage::reclaim_buffer(&mut above_factor);
        assert_eq!(above_factor.capacity(), FLOOR);

        let mut below_three = buffer(FLOOR * 3 + 1);
        RecordStorage::reclaim_buffer(&mut below_three);
        assert_eq!(below_three.capacity(), FLOOR * 3 + 1);

        let mut above_five_boundary = buffer(FLOOR * 4 + 1);
        RecordStorage::reclaim_buffer(&mut above_five_boundary);
        assert_eq!(above_five_boundary.capacity(), FLOOR);

        let mut live_above_floor = buffer(36_001);
        live_above_floor.extend(core::iter::repeat_n(0, 9_000));
        RecordStorage::reclaim_buffer(&mut live_above_floor);
        assert_eq!(live_above_floor.capacity(), 9_000);

        let mut reclaimed = RecordStorage::with_capacity(36_001, 36_001);
        for _ in 0..9_000 {
            reclaimed.append_field(b"x");
        }
        reclaimed.reclaim();
        assert_eq!(reclaimed.field_capacity(), 9_000);
        assert_eq!(reclaimed.byte_capacity(), 9_000);
    }
}
