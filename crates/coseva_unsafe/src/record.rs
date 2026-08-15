//! Default-dialect SIMD record materializers.

use alloc::vec::Vec;
use core::ptr;

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use crate::search::{avx2_available, csv_masks_ascii_avx2, csv_masks_avx2};
#[cfg(target_arch = "x86_64")]
use crate::search::{bmi2_available, csv_masks_words_avx2, pack_bytes_bmi2};
use crate::search::{find1_near, find3_near};
use crate::span::{Source, Span, SpanStorage};
use crate::storage::RecordStorage;

#[cfg(target_arch = "x86_64")]
const MAX_BATCHED_RECORD: usize = 4 * 1024;

// gamma::skip(fn_value.some, reason = "mutation causes non-termination or unbounded resource use")
#[inline(never)]
pub fn try_parse_default_quoted_prefix<const CHECK_FIELD_LIMIT: bool>(
    input: &[u8],
    output: &mut RecordStorage,
    max_record_bytes: usize,
    max_field_bytes: usize,
    max_fields: usize,
) -> Option<(usize, bool)> {
    let (bytes, ends) = output.parts_mut();
    let input = &input[..input.len().min(max_record_bytes)];
    let mut location = 0;
    loop {
        // gamma::skip(expr.increment, reason = "mutation causes non-termination or unbounded resource use")
        let &first = input.get(location)?;
        // gamma::skip(cond.always_true, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(cond.negate, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(relational.ne_to_eq, reason = "mutation causes non-termination or unbounded resource use")
        if first != b'"' {
            return Some((location, false));
        }
        let mut field_bytes_left = max_field_bytes;
        let mut segment_start = location + 1;
        let mut cursor = segment_start;
        loop {
            // SAFETY: each cursor is derived from an in-bounds quote.
            let remaining = unsafe { input.get_unchecked(cursor..) };
            let at = cursor + find1_near(b'"', remaining)?;
            // SAFETY: quote search proves these ordered bounds are in range.
            let segment = unsafe { input.get_unchecked(segment_start..at) };
            if !consume_field_raw_bytes::<CHECK_FIELD_LIMIT>(&mut field_bytes_left, segment.len()) {
                return None;
            }
            append_segment(bytes, segment);
            if input.get(at + 1) == Some(&b'"') {
                if !consume_field_raw_bytes::<CHECK_FIELD_LIMIT>(&mut field_bytes_left, 2) {
                    return None;
                }
                bytes.push(b'"');
                // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(arith.add_to_sub, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(literal.int_to_zero, reason = "mutation causes non-termination or unbounded resource use")
                cursor = at + 2;
                segment_start = cursor;
                // gamma::skip(loop.continue_to_break, reason = "mutation causes non-termination or unbounded resource use")
                continue;
            }
            if !finish_field(bytes, ends, max_fields) {
                return None;
            }
            let after_quote = at + 1;
            match input.get(after_quote) {
                Some(b',') => {
                    // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
                    location = after_quote + 1;
                    // gamma::skip(loop.break_to_continue, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(loop.delete_break, reason = "mutation causes non-termination or unbounded resource use")
                    break;
                }
                Some(b'\n') => return Some((after_quote + 1, true)),
                Some(b'\r') if input.get(after_quote + 1) == Some(&b'\n') => {
                    return Some((after_quote + 2, true));
                }
                _ => return None,
            }
        }
    }
}

#[inline(never)]
pub fn try_parse_default_interior_prefix<const CHECK_FIELD_LIMIT: bool>(
    input: &[u8],
    output: &mut RecordStorage,
    max_record_bytes: usize,
    max_field_bytes: usize,
    max_fields: usize,
) -> Option<(usize, bool)> {
    let (bytes, ends) = output.parts_mut();
    let input = &input[..input.len().min(max_record_bytes)];
    let mut location = 0;
    loop {
        let &first = input.get(location)?;
        if first != b'"' {
            // SAFETY: `location` named the byte read above.
            let remaining = unsafe { input.get_unchecked(location..) };
            let at = location + find3_near(b',', b'"', b'\n', remaining)?;
            // SAFETY: search returned an offset inside `remaining`.
            let byte = unsafe { *input.get_unchecked(at) };
            if byte == b'"' {
                return None;
            }
            let field_end = if byte == b'\n'
                && at > location
                // SAFETY: the comparison proves the preceding byte exists.
                && unsafe { *input.get_unchecked(at - 1) } == b'\r'
            {
                at - 1
            } else {
                at
            };
            // SAFETY: all bounds were established above.
            let segment = unsafe { input.get_unchecked(location..field_end) };
            if CHECK_FIELD_LIMIT && segment.len() > max_field_bytes {
                return None;
            }
            append_segment(bytes, segment);
            if !finish_field(bytes, ends, max_fields) {
                return None;
            }
            // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(arith.add_to_mul, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
            location = at + 1;
            if byte == b'\n' {
                return Some((location, true));
            }
            continue;
        }
        let mut field_bytes_left = max_field_bytes;
        let mut segment_start = location + 1;
        let mut cursor = segment_start;
        loop {
            // SAFETY: each cursor is derived from an in-bounds quote.
            let remaining = unsafe { input.get_unchecked(cursor..) };
            let at = cursor + find1_near(b'"', remaining)?;
            // SAFETY: quote search proves these ordered bounds are in range.
            let segment = unsafe { input.get_unchecked(segment_start..at) };
            if !consume_field_raw_bytes::<CHECK_FIELD_LIMIT>(&mut field_bytes_left, segment.len()) {
                return None;
            }
            append_segment(bytes, segment);
            if input.get(at + 1) == Some(&b'"') {
                if !consume_field_raw_bytes::<CHECK_FIELD_LIMIT>(&mut field_bytes_left, 2) {
                    return None;
                }
                bytes.push(b'"');
                // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(arith.add_to_sub, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(literal.int_to_zero, reason = "mutation causes non-termination or unbounded resource use")
                cursor = at + 2;
                segment_start = cursor;
                // gamma::skip(loop.continue_to_break, reason = "mutation causes non-termination or unbounded resource use")
                continue;
            }
            if !finish_field(bytes, ends, max_fields) {
                return None;
            }
            let after_quote = at + 1;
            match input.get(after_quote) {
                Some(b',') => return Some((after_quote + 1, false)),
                Some(b'\n') => return Some((after_quote + 1, true)),
                Some(b'\r') if input.get(after_quote + 1) == Some(&b'\n') => {
                    return Some((after_quote + 2, true));
                }
                _ => return None,
            }
        }
    }
}

/// Parse one default-dialect record into owned buffers.
#[inline(always)]
#[expect(
    clippy::inline_always,
    reason = "the default record parser is the primary owned hot path"
)]
pub fn try_parse_default_record<const CHECK_FIELD_LIMIT: bool>(
    input: &[u8],
    output: &mut RecordStorage,
    max_record_bytes: usize,
    max_field_bytes: usize,
    max_fields: usize,
) -> Option<usize> {
    let (bytes, ends) = output.parts_mut();
    let scan_end = input.len().min(max_record_bytes);
    let input = &input[..scan_end];
    let mut location = 0;
    loop {
        let &first = input.get(location)?;
        if first == b'"' {
            let mut field_bytes_left = max_field_bytes;
            let mut segment_start = location + 1;
            let mut cursor = segment_start;
            loop {
                // SAFETY: `location` was read above, and each cursor comes
                // from an in-bounds quote plus one or two bytes.
                let remaining = unsafe { input.get_unchecked(cursor..) };
                let at = cursor + find1_near(b'"', remaining)?;
                // SAFETY: the search result is inside `remaining`.
                let segment = unsafe { input.get_unchecked(segment_start..at) };
                if !consume_field_raw_bytes::<CHECK_FIELD_LIMIT>(
                    &mut field_bytes_left,
                    segment.len(),
                ) {
                    return None;
                }
                append_segment(bytes, segment);
                if input.get(at + 1) == Some(&b'"') {
                    if !consume_field_raw_bytes::<CHECK_FIELD_LIMIT>(&mut field_bytes_left, 2) {
                        return None;
                    }
                    bytes.push(b'"');
                    // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(arith.add_to_sub, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(literal.int_to_zero, reason = "mutation causes non-termination or unbounded resource use")
                    cursor = at + 2;
                    segment_start = cursor;
                    // gamma::skip(loop.continue_to_break, reason = "mutation causes non-termination or unbounded resource use")
                    continue;
                }
                if !finish_field(bytes, ends, max_fields) {
                    return None;
                }
                let after_quote = at + 1;
                match input.get(after_quote) {
                    Some(b',') => {
                        location = after_quote + 1;
                        break;
                    }
                    Some(b'\n') => return Some(after_quote + 1),
                    Some(b'\r') if input.get(after_quote + 1) == Some(&b'\n') => {
                        return Some(after_quote + 2);
                    }
                    _ => return None,
                }
            }
        } else {
            // SAFETY: `location` names `first`, read above.
            let remaining = unsafe { input.get_unchecked(location..) };
            let at = location + find3_near(b',', b'"', b'\n', remaining)?;
            // SAFETY: the search result is inside `remaining`.
            let byte = unsafe { *input.get_unchecked(at) };
            if byte == b'"' {
                return None;
            }
            let field_end = if byte == b'\n'
                && at > location
                // SAFETY: `at > location` and the search proved `at` in bounds.
                && unsafe { *input.get_unchecked(at - 1) } == b'\r'
            {
                at - 1
            } else {
                at
            };
            // SAFETY: `location <= field_end <= at < input.len()`.
            let segment = unsafe { input.get_unchecked(location..field_end) };
            if CHECK_FIELD_LIMIT && segment.len() > max_field_bytes {
                return None;
            }
            append_segment(bytes, segment);
            if !finish_field(bytes, ends, max_fields) {
                return None;
            }
            location = at + 1;
            if byte == b'\n' {
                return Some(location);
            }
        }
    }
}

/// Whether the packed default-dialect materializer is available.
#[cfg(target_arch = "x86_64")]
#[inline]
pub fn default_plain_packed_available() -> bool {
    packed_features_available(avx2_available(), bmi2_available())
}

#[cfg(target_arch = "x86_64")]
const fn packed_features_available(avx2: bool, bmi2: bool) -> bool {
    matches!((avx2, bmi2), (true, true))
}

#[cold]
#[inline(never)]
#[cfg_attr(coverage_nightly, coverage(off))]
fn no_simd_option<T>() -> Option<T> {
    None
}

// gamma::skip(fn_value.some, reason = "mutation causes non-termination or unbounded resource use")
/// Parse one plain borrowed record with AVX2 delimiter masks.
#[cfg(target_arch = "x86_64")]
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn try_parse_default_borrowed_plain(
    input: &[u8],
    record_start: usize,
    spans: &mut SpanStorage,
) -> Option<usize> {
    match (spans.accepts_input(input), avx2_available()) {
        (true, true) => {
            // SAFETY: runtime detection proved AVX2 support.
            unsafe { try_parse_default_borrowed_plain_avx2(input, record_start, spans) }
        }
        _ => no_simd_option(),
    }
}

/// Outcome of attempting a borrowed default-record specialization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BorrowedQuoted {
    Parsed { consumed: usize, terminated: bool },
    Unsupported,
    TooLong,
}

/// Parse a default-CSV borrowed record, copying only escaped field segments.
pub fn try_parse_default_borrowed_record(
    input: &[u8],
    record_start: usize,
    storage: &mut SpanStorage,
    max_record_bytes: usize,
    max_field_bytes: usize,
    max_fields: usize,
) -> BorrowedQuoted {
    try_parse_default_borrowed_record_impl::<true>(
        input,
        record_start,
        storage,
        max_record_bytes,
        max_field_bytes,
        max_fields,
    )
}

/// Parse without per-field limit checks after the caller proves the complete
/// input window is smaller than every configured limit.
pub fn try_parse_default_borrowed_record_bounded(
    input: &[u8],
    record_start: usize,
    storage: &mut SpanStorage,
) -> BorrowedQuoted {
    try_parse_default_borrowed_record_impl::<false>(
        input,
        record_start,
        storage,
        usize::MAX,
        usize::MAX,
        usize::MAX,
    )
}

fn try_parse_default_borrowed_record_impl<const CHECK_LIMITS: bool>(
    input: &[u8],
    record_start: usize,
    storage: &mut SpanStorage,
    max_record_bytes: usize,
    max_field_bytes: usize,
    max_fields: usize,
) -> BorrowedQuoted {
    if !storage.accepts_input(input) {
        return BorrowedQuoted::Unsupported;
    }
    let (spans, scratch) = storage.parts_mut();
    let Some(suffix) = input.get(record_start..) else {
        return BorrowedQuoted::Unsupported;
    };
    let scan_len = if CHECK_LIMITS {
        suffix.len().min(max_record_bytes)
    } else {
        suffix.len()
    };
    let scan = &suffix[..scan_len];
    let original_len = spans.len();
    let original_scratch_len = scratch.len();
    let mut fields_added = 0;
    let mut field_start = 0;
    macro_rules! bail {
        ($outcome:expr) => {{
            spans.truncate(original_len);
            scratch.truncate(original_scratch_len);
            return $outcome;
        }};
    }

    loop {
        if CHECK_LIMITS && fields_added == max_fields {
            bail!(BorrowedQuoted::Unsupported);
        }
        // gamma::skip(cond.always_true, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(cond.negate, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(relational.eq_to_ne, reason = "mutation causes non-termination or unbounded resource use")
        if field_start == scan.len() {
            if scan_len < suffix.len() {
                bail!(BorrowedQuoted::TooLong);
            }
            spans.push(Span::from_valid_range(
                Source::Input,
                record_start + field_start..record_start + field_start,
                false,
            ));
            return BorrowedQuoted::Parsed {
                consumed: field_start,
                terminated: false,
            };
        }

        if scan[field_start] == b'"' {
            let content_start = field_start + 1;
            let scratch_start = scratch.len();
            let mut segment_start = content_start;
            let mut cursor = content_start;
            let closing = loop {
                let Some(relative) = find1_near(b'"', &scan[cursor..]) else {
                    bail!(if scan_len < suffix.len() {
                        BorrowedQuoted::TooLong
                    } else {
                        BorrowedQuoted::Unsupported
                    });
                };
                let quote = cursor + relative;
                if scan.get(quote + 1) != Some(&b'"') {
                    break quote;
                }
                scratch.extend_from_slice(&scan[segment_start..quote]);
                scratch.push(b'"');
                // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(arith.add_to_sub, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(literal.int_to_zero, reason = "mutation causes non-termination or unbounded resource use")
                cursor = quote + 2;
                segment_start = cursor;
            };
            if CHECK_LIMITS && closing + 1 - field_start > max_field_bytes {
                bail!(BorrowedQuoted::Unsupported);
            }
            let copied = scratch.len() != scratch_start;
            if copied {
                scratch.extend_from_slice(&scan[segment_start..closing]);
                spans.push(Span::from_valid_range(
                    Source::Scratch,
                    scratch_start..scratch.len(),
                    true,
                ));
            } else {
                spans.push(Span::from_valid_range(
                    Source::Input,
                    record_start + content_start..record_start + closing,
                    true,
                ));
            };
            match scan.get(closing + 1) {
                Some(b',') => {
                    fields_added += 1;
                    field_start = closing + 2;
                }
                Some(b'\n') => {
                    return BorrowedQuoted::Parsed {
                        consumed: closing + 2,
                        terminated: true,
                    };
                }
                Some(b'\r') if scan.get(closing + 2) == Some(&b'\n') => {
                    return BorrowedQuoted::Parsed {
                        consumed: closing + 3,
                        terminated: true,
                    };
                }
                None if scan_len == suffix.len() => {
                    return BorrowedQuoted::Parsed {
                        consumed: closing + 1,
                        terminated: false,
                    };
                }
                _ => {
                    bail!(BorrowedQuoted::Unsupported);
                }
            }
            continue;
        }

        let Some(relative) = find3_near(b',', b'"', b'\n', &scan[field_start..]) else {
            if scan_len < suffix.len() || CHECK_LIMITS && scan.len() - field_start > max_field_bytes
            {
                bail!(BorrowedQuoted::TooLong);
            }
            spans.push(Span::from_valid_range(
                Source::Input,
                record_start + field_start..record_start + scan.len(),
                false,
            ));
            return BorrowedQuoted::Parsed {
                consumed: scan.len(),
                terminated: false,
            };
        };
        let structural = field_start + relative;
        match scan[structural] {
            b',' => {
                if CHECK_LIMITS && structural - field_start > max_field_bytes {
                    bail!(BorrowedQuoted::Unsupported);
                }
                spans.push(Span::from_valid_range(
                    Source::Input,
                    record_start + field_start..record_start + structural,
                    false,
                ));
                fields_added += 1;
                // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(arith.add_to_mul, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
                field_start = structural + 1;
            }
            b'\n' => {
                let field_end = if structural > field_start && scan[structural - 1] == b'\r' {
                    structural - 1
                } else {
                    structural
                };
                if CHECK_LIMITS && field_end - field_start > max_field_bytes {
                    bail!(BorrowedQuoted::Unsupported);
                }
                spans.push(Span::from_valid_range(
                    Source::Input,
                    record_start + field_start..record_start + field_end,
                    false,
                ));
                return BorrowedQuoted::Parsed {
                    consumed: structural + 1,
                    terminated: true,
                };
            }
            _ => {
                bail!(BorrowedQuoted::Unsupported);
            }
        }
    }
}

// gamma::skip(fn_value.some, reason = "mutation causes non-termination or unbounded resource use")
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[cfg_attr(coverage_nightly, coverage(off))]
unsafe fn try_parse_default_borrowed_plain_avx2(
    input: &[u8],
    record_start: usize,
    spans: &mut SpanStorage,
) -> Option<usize> {
    let original_len = spans.len();
    let mut consumed = 0;
    let mut field_start = record_start;
    loop {
        if consumed == MAX_BATCHED_RECORD {
            spans.truncate(original_len);
            // gamma::skip(option.none_to_some, reason = "mutation causes non-termination or unbounded resource use")
            return None;
        }
        let Some(block) = input
            .get(record_start + consumed..)
            .and_then(|remaining| remaining.split_first_chunk::<32>().map(|(block, _)| block))
        else {
            spans.truncate(original_len);
            // gamma::skip(option.none_to_some, reason = "mutation causes non-termination or unbounded resource use")
            return None;
        };
        let (commas, quotes, newlines) = csv_masks_avx2(block);
        let record_end = newlines.trailing_zeros() as usize;
        let relevant = bits_before_first(newlines);
        if u64::from(quotes) & relevant != 0 {
            spans.truncate(original_len);
            // gamma::skip(option.none_to_some, reason = "mutation causes non-termination or unbounded resource use")
            return None;
        }

        let mut separators = u64::from(commas) & relevant;
        let finish_record = record_end != 32;
        let added = separators.count_ones() as usize + usize::from(finish_record);
        let spans_len = spans.raw_len();
        spans.reserve(added);
        let raw_spans = spans.spans_mut();
        // SAFETY: `reserve` made room for every separator and the optional
        // final field; each slot is initialized before `set_len` exposes it.
        let output = unsafe { raw_spans.as_mut_ptr().add(spans_len) };
        let mut written = 0;
        while separators != 0 {
            let separator = separators.trailing_zeros() as usize;
            separators &= separators - 1;
            let field_end = record_start + consumed + separator;
            // SAFETY: `written < added` because each write corresponds to one
            // bit counted above.
            unsafe {
                output.add(written).write(Span::from_range_unchecked(
                    Source::Input,
                    field_start..field_end,
                    false,
                ));
            }
            written += 1;
            field_start = field_end + 1;
        }

        if finish_record {
            let newline = record_start + consumed + record_end;
            let field_end = if newline > field_start && input[newline - 1] == b'\r' {
                newline - 1
            } else {
                newline
            };
            // SAFETY: the final field owns the extra reserved slot.
            unsafe {
                output.add(written).write(Span::from_range_unchecked(
                    Source::Input,
                    field_start..field_end,
                    false,
                ));
                raw_spans.set_len(spans_len + written + 1);
            }
            // gamma::skip(arith.add_to_mul, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(arith.add_to_sub, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(expr.decrement, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
            return Some(consumed + record_end + 1);
        }
        // SAFETY: all `written` slots were initialized above.
        unsafe { raw_spans.set_len(spans_len + written) };
        // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(literal.int_to_zero, reason = "mutation causes non-termination or unbounded resource use")
        consumed += 32;
    }
}

/// Parse one plain owned record with AVX2 and BMI2.
#[cfg(target_arch = "x86_64")]
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn try_parse_default_plain_packed(input: &[u8], output: &mut RecordStorage) -> Option<usize> {
    let (bytes, ends) = output.parts_mut();
    match default_plain_packed_available() {
        true => {
            // SAFETY: runtime detection proved AVX2 and BMI2 support.
            unsafe { try_parse_default_plain_packed_x86(input, bytes, ends) }
        }
        false => no_simd_option(),
    }
}

/// Parse one plain owned record and certify ASCII while processing it.
#[cfg(target_arch = "x86_64")]
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn try_parse_default_plain_packed_ascii(
    input: &[u8],
    output: &mut RecordStorage,
) -> Option<usize> {
    let (bytes, ends) = output.parts_mut();
    let result = match default_plain_packed_available() {
        true => {
            // SAFETY: runtime detection proved AVX2 and BMI2 support.
            unsafe { try_parse_default_plain_packed_ascii_x86(input, bytes, ends) }
        }
        false => no_simd_option(),
    };
    if let Some((consumed, true)) = result {
        output.certify_ascii();
        Some(consumed)
    } else {
        result.map(|(consumed, _)| consumed)
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,bmi2")]
#[cfg_attr(coverage_nightly, coverage(off))]
unsafe fn try_parse_default_plain_packed_ascii_x86(
    input: &[u8],
    bytes: &mut Vec<u8>,
    ends: &mut Vec<usize>,
) -> Option<(usize, bool)> {
    unsafe { try_parse_default_plain_packed_mode_x86::<true>(input, bytes, ends) }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,bmi2")]
#[cfg_attr(coverage_nightly, coverage(off))]
unsafe fn try_parse_default_plain_packed_x86(
    input: &[u8],
    bytes: &mut Vec<u8>,
    ends: &mut Vec<usize>,
) -> Option<usize> {
    unsafe { try_parse_default_plain_packed_mode_x86::<false>(input, bytes, ends) }
        .map(|(consumed, _)| consumed)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,bmi2")]
#[cfg_attr(coverage_nightly, coverage(off))]
unsafe fn try_parse_default_plain_packed_mode_x86<const CERTIFY_ASCII: bool>(
    input: &[u8],
    bytes: &mut Vec<u8>,
    ends: &mut Vec<usize>,
) -> Option<(usize, bool)> {
    let mut consumed = 0;
    let mut ascii = true;
    loop {
        if consumed == MAX_BATCHED_RECORD {
            clear(bytes, ends);
            return None;
        }
        let Some(block) = input
            .get(consumed..)
            .and_then(|remaining| remaining.split_first_chunk::<32>().map(|(block, _)| block))
        else {
            clear(bytes, ends);
            return None;
        };
        let (commas, quotes, newlines, words) = csv_masks_words_avx2(block);
        if CERTIFY_ASCII {
            ascii &= (words[0] | words[1] | words[2] | words[3]) & 0x8080_8080_8080_8080 == 0;
        }
        let record_end = newlines.trailing_zeros() as usize;
        let relevant = bits_before_first(newlines);
        if u64::from(quotes) & relevant != 0 {
            clear(bytes, ends);
            return None;
        }
        let commas = u64::from(commas) & relevant;
        let finish_record = record_end != 32;
        let mut removed = commas;
        if finish_record {
            if record_end != 0 && block[record_end - 1] == b'\r' {
                removed |= 1_u64 << (record_end - 1);
            }
            removed |= !lower_bits(record_end);
        }
        let removed = removed.to_le_bytes();
        let packed = [
            pack_bytes_bmi2(words[0], removed[0]),
            pack_bytes_bmi2(words[1], removed[1]),
            pack_bytes_bmi2(words[2], removed[2]),
            pack_bytes_bmi2(words[3], removed[3]),
        ];
        let decoded_len = packed.iter().map(|&(_, len)| usize::from(len)).sum();
        append_packed_plain(bytes, ends, &packed, commas, decoded_len, finish_record);
        // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(literal.int_to_zero, reason = "mutation causes non-termination or unbounded resource use")
        consumed += if finish_record { record_end + 1 } else { 32 };
        if finish_record {
            return Some((consumed, ascii));
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[cfg_attr(coverage_nightly, coverage(off))]
fn append_packed_plain(
    bytes: &mut Vec<u8>,
    ends: &mut Vec<usize>,
    packed: &[(u64, u8)],
    mut commas: u64,
    decoded_len: usize,
    finish_record: bool,
) {
    let base = bytes.len();
    bytes.reserve(decoded_len + 8);
    ends.reserve(commas.count_ones() as usize + usize::from(finish_record));
    let mut written = 0;
    for &(word, len) in packed {
        // SAFETY: eight spare bytes after the decoded record make each
        // overlapping full-word store fit, including the final partial word.
        unsafe {
            bytes
                .as_mut_ptr()
                .add(base + written)
                .cast::<u64>()
                .write_unaligned(word)
        };
        written += usize::from(len);
    }
    debug_assert_eq!(written, decoded_len);
    // SAFETY: every byte below the new length was initialized by the stores.
    unsafe { bytes.set_len(base + decoded_len) };

    let mut removed = 0;
    while commas != 0 {
        let comma = commas.trailing_zeros() as usize;
        commas &= commas - 1;
        push_reserved(ends, base + comma - removed);
        removed += 1;
    }
    if finish_record {
        push_reserved(ends, base + decoded_len);
    }
}

/// Parse one quote-heavy record from two AVX2 masks.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn try_parse_default_quoted_record_structural<const CHECK_FIELD_LIMIT: bool>(
    input: &[u8],
    output: &mut RecordStorage,
    max_field_bytes: usize,
) -> Option<(usize, bool)> {
    try_parse_default_quoted_record_structural_mode::<CHECK_FIELD_LIMIT, false, false>(
        input,
        output,
        max_field_bytes,
    )
}

/// Parse one quote-heavy record while preserving a previously settled prefix
/// if the structural attempt cannot handle the remaining window.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn try_parse_default_quoted_record_structural_appending<const CHECK_FIELD_LIMIT: bool>(
    input: &[u8],
    output: &mut RecordStorage,
    max_field_bytes: usize,
) -> Option<(usize, bool)> {
    try_parse_default_quoted_record_structural_mode::<CHECK_FIELD_LIMIT, true, false>(
        input,
        output,
        max_field_bytes,
    )
}

/// Parse one complete quote-heavy record and certify ASCII from loaded blocks.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn try_parse_default_quoted_record_structural_ascii<const CHECK_FIELD_LIMIT: bool>(
    input: &[u8],
    output: &mut RecordStorage,
    max_field_bytes: usize,
) -> Option<(usize, bool)> {
    try_parse_default_quoted_record_structural_mode::<CHECK_FIELD_LIMIT, false, true>(
        input,
        output,
        max_field_bytes,
    )
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn try_parse_default_quoted_record_structural_mode<
    const CHECK_FIELD_LIMIT: bool,
    const PRESERVE_PREFIX: bool,
    const CERTIFY_ASCII: bool,
>(
    input: &[u8],
    output: &mut RecordStorage,
    max_field_bytes: usize,
) -> Option<(usize, bool)> {
    match avx2_available() {
        true => {
            // SAFETY: runtime detection proved AVX2 support.
            unsafe {
                try_parse_default_quoted_record_structural_avx2::<
                    CHECK_FIELD_LIMIT,
                    PRESERVE_PREFIX,
                    CERTIFY_ASCII,
                >(input, output, max_field_bytes)
            }
        }
        false => no_simd_option(),
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
#[cfg_attr(coverage_nightly, coverage(off))]
unsafe fn try_parse_default_quoted_record_structural_avx2<
    const CHECK_FIELD_LIMIT: bool,
    const PRESERVE_PREFIX: bool,
    const CERTIFY_ASCII: bool,
>(
    input: &[u8],
    output: &mut RecordStorage,
    max_field_bytes: usize,
) -> Option<(usize, bool)> {
    let (bytes, ends) = output.parts_mut();
    let initial_bytes = bytes.len();
    let initial_fields = ends.len();
    let Some((first, rest)) = input.split_first_chunk::<32>() else {
        return None;
    };
    let Some((second, _)) = rest.split_first_chunk::<32>() else {
        return None;
    };
    let (commas_low, quotes_low, newlines_low, ascii_low) = if CERTIFY_ASCII {
        csv_masks_ascii_avx2(first)
    } else {
        let (commas, quotes, newlines) = csv_masks_avx2(first);
        (commas, quotes, newlines, false)
    };
    let (commas_high, quotes_high, newlines_high, ascii_high) = if CERTIFY_ASCII {
        csv_masks_ascii_avx2(second)
    } else {
        let (commas, quotes, newlines) = csv_masks_avx2(second);
        (commas, quotes, newlines, false)
    };
    let commas = u64::from(commas_low) | (u64::from(commas_high) << 32);
    let quotes = u64::from(quotes_low) | (u64::from(quotes_high) << 32);
    let newlines = u64::from(newlines_low) | (u64::from(newlines_high) << 32);
    if CERTIFY_ASCII && quotes == 0 {
        return None;
    }
    let outside = !quote_parity(quotes);
    let record_end_mask = newlines & outside;
    if record_end_mask == 0 {
        return None;
    }
    let record_end_bit = record_end_mask & record_end_mask.wrapping_neg();
    let record_end = record_end_bit.trailing_zeros() as usize;
    let mut separators = commas & outside & record_end_bit.wrapping_sub(1);
    bytes.reserve(record_end);
    ends.reserve(separators.count_ones() as usize + 1);
    let mut field_start = 0;
    let mut field_start_marker = 1_u64;
    while separators != 0 {
        let separator_bit = separators & separators.wrapping_neg();
        let separator = separators.trailing_zeros() as usize;
        separators &= separators - 1;
        let field_quotes =
            quotes & separator_bit.wrapping_sub(1) & !field_start_marker.wrapping_sub(1);
        if !append_structural_field::<CHECK_FIELD_LIMIT>(
            input,
            bytes,
            field_start,
            separator,
            field_quotes,
            max_field_bytes,
        ) {
            rollback_structural::<PRESERVE_PREFIX>(bytes, ends, initial_bytes, initial_fields);
            return None;
        }
        push_reserved(ends, bytes.len());
        field_start = separator + 1;
        field_start_marker = separator_bit;
    }
    let field_end = if record_end > field_start && input[record_end - 1] == b'\r' {
        record_end - 1
    } else {
        record_end
    };
    let field_quotes =
        quotes & record_end_bit.wrapping_sub(1) & !field_start_marker.wrapping_sub(1);
    if !append_structural_field::<CHECK_FIELD_LIMIT>(
        input,
        bytes,
        field_start,
        field_end,
        field_quotes,
        max_field_bytes,
    ) {
        rollback_structural::<PRESERVE_PREFIX>(bytes, ends, initial_bytes, initial_fields);
        return None;
    }
    push_reserved(ends, bytes.len());
    if CERTIFY_ASCII && ascii_low && ascii_high {
        output.certify_ascii();
    }
    Some((record_end + 1, true))
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[inline(always)]
fn append_structural_field<const CHECK_FIELD_LIMIT: bool>(
    input: &[u8],
    bytes: &mut Vec<u8>,
    field_start: usize,
    field_end: usize,
    field_quotes: u64,
    max_field_bytes: usize,
) -> bool {
    if field_quotes != 0 {
        return append_masked_quoted_field::<CHECK_FIELD_LIMIT>(
            input,
            bytes,
            field_start,
            field_end,
            field_quotes,
            max_field_bytes,
        );
    }
    let field = &input[field_start..field_end];
    if CHECK_FIELD_LIMIT && field.len() > max_field_bytes {
        return false;
    }
    append_reserved(bytes, field);
    true
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[inline]
fn rollback_structural<const PRESERVE_PREFIX: bool>(
    bytes: &mut Vec<u8>,
    ends: &mut Vec<usize>,
    initial_bytes: usize,
    initial_fields: usize,
) {
    if PRESERVE_PREFIX {
        bytes.truncate(initial_bytes);
        ends.truncate(initial_fields);
    } else {
        clear(bytes, ends);
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[inline(always)]
#[expect(
    clippy::inline_always,
    reason = "quoted structural records call this once per field; folding mask bounds into the caller removes the dominant per-field call frame"
)]
#[cfg_attr(coverage_nightly, coverage(off))]
fn append_masked_quoted_field<const CHECK_FIELD_LIMIT: bool>(
    input: &[u8],
    bytes: &mut Vec<u8>,
    field_start: usize,
    field_end: usize,
    field_quotes: u64,
    max_field_bytes: usize,
) -> bool {
    let Some(closing) = field_end.checked_sub(1) else {
        return false;
    };
    let outer_quotes = (1_u64 << field_start) | (1_u64 << closing);
    if field_quotes == outer_quotes {
        let field = &input[field_start + 1..closing];
        if CHECK_FIELD_LIMIT && field.len() > max_field_bytes {
            return false;
        }
        append_reserved(bytes, field);
        return true;
    }
    if field_quotes & outer_quotes != outer_quotes {
        return false;
    }
    let field = &input[field_start + 1..closing];
    if CHECK_FIELD_LIMIT && field.len() > max_field_bytes {
        return false;
    }
    let mut escaped = field_quotes & !outer_quotes;
    let mut segment_start = field_start + 1;
    while escaped != 0 {
        let first = escaped.trailing_zeros() as usize;
        let second = first + 1;
        let pair = (1_u64 << first) | (1_u64 << second);
        if escaped & pair != pair {
            return false;
        }
        append_reserved(bytes, &input[segment_start..first]);
        push_reserved(bytes, b'"');
        segment_start = second + 1;
        escaped &= !pair;
    }
    append_reserved(bytes, &input[segment_start..closing]);
    true
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[inline(always)]
fn append_reserved(bytes: &mut Vec<u8>, segment: &[u8]) {
    let len = bytes.len();
    let new_len = len + segment.len();
    debug_assert!(new_len <= bytes.capacity());
    // SAFETY: the structural parser reserves the complete encoded record
    // before emitting fields. The source window and owned destination cannot
    // overlap through the safe parser API, and `new_len` stays within capacity.
    unsafe {
        core::ptr::copy_nonoverlapping(
            segment.as_ptr(),
            bytes.as_mut_ptr().add(len),
            segment.len(),
        );
        bytes.set_len(new_len);
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[inline(always)]
fn push_reserved<T>(values: &mut Vec<T>, value: T) {
    let len = values.len();
    debug_assert!(len < values.capacity());
    // SAFETY: the structural parser reserves one endpoint per separator and
    // enough byte capacity for every decoded byte before entering its loop.
    unsafe {
        values.as_mut_ptr().add(len).write(value);
        values.set_len(len + 1);
    }
}

#[inline(always)]
#[expect(
    clippy::inline_always,
    reason = "segment length specialization is part of the quoted parser hot loop"
)]
fn append_segment(bytes: &mut Vec<u8>, segment: &[u8]) {
    let len = bytes.len();
    if bytes.capacity() - len < segment.len() {
        bytes.extend_from_slice(segment);
        return;
    }

    // SAFETY: the capacity check proves the destination range is allocated,
    // and parser input cannot alias caller-owned output through the safe API.
    unsafe {
        ptr::copy_nonoverlapping(segment.as_ptr(), bytes.as_mut_ptr().add(len), segment.len());
        bytes.set_len(len + segment.len());
    }
}

#[inline(always)]
fn consume_field_raw_bytes<const CHECK_FIELD_LIMIT: bool>(
    field_bytes_left: &mut usize,
    added: usize,
) -> bool {
    if !CHECK_FIELD_LIMIT {
        return true;
    }
    let Some(remaining) = field_bytes_left.checked_sub(added) else {
        return false;
    };
    *field_bytes_left = remaining;
    true
}

#[inline(always)]
fn finish_field(bytes: &[u8], ends: &mut Vec<usize>, max_fields: usize) -> bool {
    if ends.len() >= max_fields {
        return false;
    }
    ends.push(bytes.len());
    true
}

#[inline]
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[cfg_attr(coverage_nightly, coverage(off))]
fn clear(bytes: &mut Vec<u8>, ends: &mut Vec<usize>) {
    bytes.clear();
    ends.clear();
}

#[inline]
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[cfg_attr(coverage_nightly, coverage(off))]
fn lower_bits(bit: usize) -> u64 {
    if bit >= 64 {
        u64::MAX
    } else {
        (1_u64 << bit).wrapping_sub(1)
    }
}

#[inline]
#[cfg(target_arch = "x86_64")]
fn bits_before_first(mask: u32) -> u64 {
    u64::from((mask & mask.wrapping_neg()).wrapping_sub(1))
}

#[inline]
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[cfg_attr(coverage_nightly, coverage(off))]
fn quote_parity(mut quotes: u64) -> u64 {
    quotes ^= quotes << 1;
    quotes ^= quotes << 2;
    quotes ^= quotes << 4;
    quotes ^= quotes << 8;
    quotes ^= quotes << 16;
    quotes ^ (quotes << 32)
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn certified_parsers_only_mark_ascii_output() {
        if !default_plain_packed_available() {
            return;
        }

        let mut plain = RecordStorage::new();
        assert!(
            try_parse_default_plain_packed_ascii(
                b"alpha,beta,gamma,delta,zeta\npadding",
                &mut plain,
            )
            .is_some()
        );
        assert_eq!(plain.text_validity(), crate::storage::TextValidity::Ascii);

        let mut non_ascii = RecordStorage::new();
        assert!(
            try_parse_default_plain_packed_ascii(
                b"alpha,beta,gamma,delta,\xC3\xA9\npadding",
                &mut non_ascii,
            )
            .is_some()
        );
        assert_eq!(
            non_ascii.text_validity(),
            crate::storage::TextValidity::Unknown
        );

        let mut quoted = RecordStorage::new();
        assert!(
            try_parse_default_quoted_record_structural_ascii::<false>(
                b"alpha,beta,\"gamma\",delta,epsilon,zeta,eta,theta,iota,kappa\npadding",
                &mut quoted,
                64,
            )
            .is_some()
        );
        assert_eq!(quoted.text_validity(), crate::storage::TextValidity::Ascii);
    }

    #[test]
    fn default_record_parsers_comprehensive() {
        let mut out = RecordStorage::new();
        assert_eq!(lower_bits(0), 0);
        assert_eq!(lower_bits(32), 0xFFFF_FFFF);
        assert_eq!(lower_bits(64), u64::MAX);
        assert_eq!(lower_bits(65), u64::MAX);

        // Exercise all combinations of CHECK_FIELD_LIMIT for all three parsers
        for check_limit in [true, false] {
            if check_limit {
                out.clear();
                assert_eq!(
                    try_parse_default_record::<true>(b"a,b,c\n", &mut out, 100, 10, 10),
                    Some(6)
                );
                out.clear();
                assert_eq!(
                    try_parse_default_record::<true>(b"\"a\",\"b\"\n", &mut out, 100, 10, 10),
                    Some(8)
                );
                out.clear();
                assert_eq!(
                    try_parse_default_record::<true>(b"\"a\"\"b\",c\n", &mut out, 100, 10, 10),
                    Some(9)
                );
                out.clear();
                assert_eq!(
                    try_parse_default_record::<true>(b"\"a\"\r\n", &mut out, 100, 10, 10),
                    Some(5)
                );
                out.clear();
                assert_eq!(
                    try_parse_default_record::<true>(b"a,b\r\n", &mut out, 100, 10, 10),
                    Some(5)
                );
                out.clear();
                assert_eq!(
                    try_parse_default_record::<true>(b"\"a\",b\r\n", &mut out, 100, 10, 10),
                    Some(7)
                );

                out.clear();
                assert_eq!(
                    try_parse_default_quoted_prefix::<true>(
                        b"\"a\",\"b\"\n",
                        &mut out,
                        100,
                        10,
                        10
                    ),
                    Some((8, true))
                );
                out.clear();
                assert_eq!(
                    try_parse_default_quoted_prefix::<true>(
                        b"\"a\"\"b\",c\n",
                        &mut out,
                        100,
                        10,
                        10
                    ),
                    Some((7, false))
                );
                out.clear();
                assert_eq!(
                    try_parse_default_quoted_prefix::<true>(b"\"a\"\r\n", &mut out, 100, 10, 10),
                    Some((5, true))
                );

                out.clear();
                assert_eq!(
                    try_parse_default_interior_prefix::<true>(
                        b"a,\"b\",c\n",
                        &mut out,
                        100,
                        10,
                        10
                    ),
                    Some((6, false))
                );
                out.clear();
                assert_eq!(
                    try_parse_default_interior_prefix::<true>(
                        b"a,\"b\"\"c\"\n",
                        &mut out,
                        100,
                        10,
                        10
                    ),
                    Some((9, true))
                );
                out.clear();
                assert_eq!(
                    try_parse_default_interior_prefix::<true>(
                        b"a,\"b\"\r\n",
                        &mut out,
                        100,
                        10,
                        10
                    ),
                    Some((7, true))
                );
                out.clear();
                assert_eq!(
                    try_parse_default_interior_prefix::<true>(b"a\r\n", &mut out, 100, 10, 10),
                    Some((3, true))
                );
                out.clear();
                assert_eq!(
                    try_parse_default_interior_prefix::<true>(b"a\n", &mut out, 100, 10, 10),
                    Some((2, true))
                );
            } else {
                out.clear();
                assert_eq!(
                    try_parse_default_record::<false>(b"a,b,c\n", &mut out, 100, 10, 10),
                    Some(6)
                );
                out.clear();
                assert_eq!(
                    try_parse_default_record::<false>(b"\"a\",\"b\"\n", &mut out, 100, 10, 10),
                    Some(8)
                );
                out.clear();
                assert_eq!(
                    try_parse_default_record::<false>(b"\"a\"\"b\",c\n", &mut out, 100, 10, 10),
                    Some(9)
                );
                out.clear();
                assert_eq!(
                    try_parse_default_record::<false>(b"\"a\"\r\n", &mut out, 100, 10, 10),
                    Some(5)
                );
                out.clear();
                assert_eq!(
                    try_parse_default_record::<false>(b"a,b\r\n", &mut out, 100, 10, 10),
                    Some(5)
                );
                out.clear();
                assert_eq!(
                    try_parse_default_record::<false>(b"\"a\",b\r\n", &mut out, 100, 10, 10),
                    Some(7)
                );

                out.clear();
                assert_eq!(
                    try_parse_default_quoted_prefix::<false>(
                        b"\"a\",\"b\"\n",
                        &mut out,
                        100,
                        10,
                        10
                    ),
                    Some((8, true))
                );
                out.clear();
                assert_eq!(
                    try_parse_default_quoted_prefix::<false>(
                        b"\"a\"\"b\",c\n",
                        &mut out,
                        100,
                        10,
                        10
                    ),
                    Some((7, false))
                );
                out.clear();
                assert_eq!(
                    try_parse_default_quoted_prefix::<false>(b"\"a\"\r\n", &mut out, 100, 10, 10),
                    Some((5, true))
                );

                out.clear();
                assert_eq!(
                    try_parse_default_interior_prefix::<false>(
                        b"a,\"b\",c\n",
                        &mut out,
                        100,
                        10,
                        10
                    ),
                    Some((6, false))
                );
                out.clear();
                assert_eq!(
                    try_parse_default_interior_prefix::<false>(
                        b"a,\"b\"\"c\"\n",
                        &mut out,
                        100,
                        10,
                        10
                    ),
                    Some((9, true))
                );
                out.clear();
                assert_eq!(
                    try_parse_default_interior_prefix::<false>(
                        b"a,\"b\"\r\n",
                        &mut out,
                        100,
                        10,
                        10
                    ),
                    Some((7, true))
                );
                out.clear();
                assert_eq!(
                    try_parse_default_interior_prefix::<false>(b"a\r\n", &mut out, 100, 10, 10),
                    Some((3, true))
                );
                out.clear();
                assert_eq!(
                    try_parse_default_interior_prefix::<false>(b"a\n", &mut out, 100, 10, 10),
                    Some((2, true))
                );
            }
        }

        // try_parse_default_borrowed_record
        let mut spans = SpanStorage::with_capacity(10);
        let input = b"\"a\",\"b\"\"c\",d\n";
        assert!(spans.begin(input, input.len()));
        let borrowed_res = try_parse_default_borrowed_record(input, 0, &mut spans, 100, 10, 10);
        assert_eq!(
            borrowed_res,
            BorrowedQuoted::Parsed {
                consumed: 13,
                terminated: true
            }
        );

        #[cfg(target_arch = "x86_64")]
        {
            let plain_input = b"field1,field2,field3,field4,field5,field6,field7\n";
            let mut plain_spans = SpanStorage::with_capacity(10);
            assert!(plain_spans.begin(plain_input, plain_input.len()));
            let _ = try_parse_default_borrowed_plain(plain_input, 0, &mut plain_spans);
            assert!(try_parse_default_borrowed_plain(plain_input, 100, &mut plain_spans).is_none());

            // unaccepted input
            let mut short_spans = SpanStorage::with_capacity(10);
            assert!(short_spans.begin(b"a", 1));
            assert!(try_parse_default_borrowed_plain(plain_input, 0, &mut short_spans).is_none());

            // quote encountered in plain borrowed
            let quote_plain = b"abc,\"def\",123456789012345678901234567890\n";
            let mut qspans = SpanStorage::with_capacity(10);
            assert!(qspans.begin(quote_plain, quote_plain.len()));
            assert!(try_parse_default_borrowed_plain(quote_plain, 0, &mut qspans).is_none());

            // 4KB plain record without newline
            let huge = [b'a'; 4096];
            let mut huge_spans = SpanStorage::with_capacity(10);
            assert!(huge_spans.begin(&huge, huge.len()));
            assert!(try_parse_default_borrowed_plain(&huge, 0, &mut huge_spans).is_none());

            let mut packed_out = RecordStorage::new();
            let _ = try_parse_default_plain_packed(plain_input, &mut packed_out);
            let _ = try_parse_default_plain_packed(b"too,short", &mut packed_out);
            let _ = try_parse_default_plain_packed(
                b"\"quoted\",in,packed,mode,here,now,32bytes\n",
                &mut packed_out,
            );
            let _ = try_parse_default_plain_packed(
                b"field1,field2,field3,field4,field5,field6\r\n",
                &mut packed_out,
            );
            let _ = try_parse_default_plain_packed(&huge, &mut packed_out);

            let mut quoted_out = RecordStorage::new();
            let quoted_input = b"\"a\",b,\"c\",d,\"e\",f,\"g\",h,\"i\",j,\"k\",l,\"m\",n,\"o\",p\n";
            let _ = try_parse_default_quoted_record_structural::<false>(
                quoted_input,
                &mut quoted_out,
                100,
            );
            let _ = try_parse_default_quoted_record_structural::<true>(
                quoted_input,
                &mut quoted_out,
                1,
            );
            let _ =
                try_parse_default_quoted_record_structural::<false>(b"short", &mut quoted_out, 100);
            let _ = try_parse_default_quoted_record_structural::<false>(
                b"\"unclosed,here,no,newline,at,all,keep,padding,12345678901234567890",
                &mut quoted_out,
                100,
            );
            let _ = try_parse_default_quoted_record_structural::<false>(
                b"\"escaped\"\"quote\",b,c,d,e,f,g,h,i,j,k,l,m,n,o,p,1234567890123456\n",
                &mut quoted_out,
                100,
            );
            let _ = try_parse_default_quoted_record_structural::<true>(
                b"\"escaped\"\"quote\",b,c,d,e,f,g,h,i,j,k,l,m,n,o,p,1234567890123456\n",
                &mut quoted_out,
                1,
            );
            let _ = try_parse_default_quoted_record_structural::<false>(
                b"\"a\",b,\"c\",d,\"e\",f,\"g\",h,\"i\",j,\"k\",l,\"m\",n,\"o\",p\r\n",
                &mut quoted_out,
                100,
            );
            let _ = try_parse_default_quoted_record_structural::<false>(
                &[b'a'; 64],
                &mut quoted_out,
                100,
            );
            assert_eq!(lower_bits(64), u64::MAX);
        }

        // Limit failure paths
        let mut out = RecordStorage::new();
        assert!(
            try_parse_default_quoted_prefix::<true>(b"\"toolongfield\"\n", &mut out, 100, 2, 10)
                .is_none()
        );
        out.clear();
        assert!(
            try_parse_default_quoted_prefix::<true>(b"\"a\"\"b\"\n", &mut out, 100, 2, 10)
                .is_none()
        );
        out.clear();
        assert!(
            try_parse_default_quoted_prefix::<true>(b"\"a\"\r\n", &mut out, 100, 10, 10).is_some()
        );
        out.clear();
        assert!(
            try_parse_default_quoted_prefix::<true>(b"\"a\"x", &mut out, 100, 10, 10).is_none()
        );
        out.clear();
        assert!(
            try_parse_default_quoted_prefix::<false>(b"\"a\"\r\n", &mut out, 100, 10, 10).is_some()
        );
        out.clear();
        assert!(
            try_parse_default_quoted_prefix::<false>(b"\"a\"x", &mut out, 100, 10, 10).is_none()
        );
        out.clear();
        assert!(
            try_parse_default_quoted_prefix::<false>(b"\"a\",", &mut out, 100, 10, 0).is_none()
        );
        out.clear();
        assert!(try_parse_default_quoted_prefix::<true>(b"\"a\",", &mut out, 100, 10, 0).is_none());
        out.clear();

        assert!(
            try_parse_default_interior_prefix::<true>(b"toolong,b\n", &mut out, 100, 2, 10)
                .is_none()
        );
        out.clear();
        assert!(
            try_parse_default_interior_prefix::<true>(b"a,\"toolong\"\n", &mut out, 100, 2, 10)
                .is_none()
        );
        out.clear();
        assert!(
            try_parse_default_interior_prefix::<true>(b"a,\"b\"\"c\"\n", &mut out, 100, 2, 10)
                .is_none()
        );
        out.clear();
        assert!(
            try_parse_default_interior_prefix::<true>(b"a,\"b\"\r\n", &mut out, 100, 10, 10)
                .is_some()
        );
        out.clear();
        assert!(
            try_parse_default_interior_prefix::<true>(b"a,\"b\"x", &mut out, 100, 10, 10).is_none()
        );
        out.clear();
        assert!(try_parse_default_interior_prefix::<true>(b"a,", &mut out, 100, 10, 0).is_none());
        out.clear();
        assert!(
            try_parse_default_interior_prefix::<true>(b"a,\"b\",", &mut out, 100, 10, 1).is_none()
        );
        out.clear();
        assert!(
            try_parse_default_interior_prefix::<false>(b"a,\"b\"\r\n", &mut out, 100, 10, 10)
                .is_some()
        );
        out.clear();
        assert!(
            try_parse_default_interior_prefix::<false>(b"a,\"b\"x", &mut out, 100, 10, 10)
                .is_none()
        );
        out.clear();
        assert!(try_parse_default_interior_prefix::<false>(b"a,", &mut out, 100, 10, 0).is_none());
        out.clear();
        assert!(
            try_parse_default_interior_prefix::<false>(b"a,\"b\",", &mut out, 100, 10, 1).is_none()
        );
        out.clear();

        assert!(try_parse_default_record::<true>(b"toolong\n", &mut out, 100, 2, 10).is_none());
        out.clear();
        assert!(try_parse_default_record::<true>(b"\"toolong\"\n", &mut out, 100, 2, 10).is_none());
        out.clear();
        assert!(try_parse_default_record::<true>(b"\"a\"\"b\"\n", &mut out, 100, 2, 10).is_none());
        out.clear();
        assert!(try_parse_default_record::<true>(b"\"a\"x", &mut out, 100, 10, 10).is_none());
        out.clear();
        assert!(try_parse_default_record::<true>(b"a,b", &mut out, 100, 10, 1).is_none());
        out.clear();
        assert!(try_parse_default_record::<true>(b"\"a\",b", &mut out, 100, 10, 1).is_none());
        out.clear();
        assert!(try_parse_default_record::<false>(b"\"a\"x", &mut out, 100, 10, 10).is_none());
        out.clear();
        assert!(try_parse_default_record::<false>(b"a,b", &mut out, 100, 10, 1).is_none());
        out.clear();
        assert!(try_parse_default_record::<false>(b"\"a\",b", &mut out, 100, 10, 1).is_none());
        out.clear();

        // try_parse_default_borrowed_record edge cases
        let mut spans = SpanStorage::with_capacity(10);
        let input_eof = b"\"a\",b";
        assert!(spans.begin(input_eof, input_eof.len()));
        assert_eq!(
            try_parse_default_borrowed_record(input_eof, 0, &mut spans, 100, 10, 10),
            BorrowedQuoted::Parsed {
                consumed: 5,
                terminated: false
            }
        );

        // storage doesn't accept input
        let mut short_storage = SpanStorage::with_capacity(10);
        assert!(short_storage.begin(b"a", 1));
        assert_eq!(
            try_parse_default_borrowed_record(b"longer", 0, &mut short_storage, 100, 10, 10),
            BorrowedQuoted::Unsupported
        );

        // field_start == scan.len() with scan_len < suffix.len() and scan_len == suffix.len()
        assert!(spans.begin(b"a,b", 3));
        assert_eq!(
            try_parse_default_borrowed_record(b"a,b", 2, &mut spans, 0, 10, 10),
            BorrowedQuoted::TooLong
        );
        assert!(spans.begin(b"a,", 2));
        assert_eq!(
            try_parse_default_borrowed_record(b"a,", 2, &mut spans, 10, 10, 10),
            BorrowedQuoted::Parsed {
                consumed: 0,
                terminated: false
            }
        );

        // quoted field at EOF
        assert!(spans.begin(b"\"hello\"", 7));
        assert_eq!(
            try_parse_default_borrowed_record(b"\"hello\"", 0, &mut spans, 100, 10, 10),
            BorrowedQuoted::Parsed {
                consumed: 7,
                terminated: false
            }
        );

        let input_unterm = b"\"unclosed";
        assert!(spans.begin(input_unterm, input_unterm.len()));
        assert_eq!(
            try_parse_default_borrowed_record(input_unterm, 0, &mut spans, 100, 10, 10),
            BorrowedQuoted::Unsupported
        );
        assert_eq!(
            try_parse_default_borrowed_record(input_unterm, 0, &mut spans, 4, 10, 10),
            BorrowedQuoted::TooLong
        );

        let input_plain_eof = b"plain";
        assert!(spans.begin(input_plain_eof, input_plain_eof.len()));
        assert_eq!(
            try_parse_default_borrowed_record(input_plain_eof, 0, &mut spans, 100, 10, 10),
            BorrowedQuoted::Parsed {
                consumed: 5,
                terminated: false
            }
        );
        assert_eq!(
            try_parse_default_borrowed_record(input_plain_eof, 0, &mut spans, 2, 10, 10),
            BorrowedQuoted::TooLong
        );

        let input_crlf = b"\"a\"\r\n";
        assert!(spans.begin(input_crlf, input_crlf.len()));
        assert_eq!(
            try_parse_default_borrowed_record(input_crlf, 0, &mut spans, 100, 10, 10),
            BorrowedQuoted::Parsed {
                consumed: 5,
                terminated: true
            }
        );

        let input_stray = b"\"a\"x";
        assert!(spans.begin(input_stray, input_stray.len()));
        assert_eq!(
            try_parse_default_borrowed_record(input_stray, 0, &mut spans, 100, 10, 10),
            BorrowedQuoted::Unsupported
        );

        // borrowed record quote followed by newline or crlf
        let input_q_nl = b"\"a\"\n";
        assert!(spans.begin(input_q_nl, input_q_nl.len()));
        assert_eq!(
            try_parse_default_borrowed_record(input_q_nl, 0, &mut spans, 100, 10, 10),
            BorrowedQuoted::Parsed {
                consumed: 4,
                terminated: true
            }
        );

        let input_unquoted_crlf = b"plain\r\n";
        assert!(spans.begin(input_unquoted_crlf, input_unquoted_crlf.len()));
        assert_eq!(
            try_parse_default_borrowed_record(input_unquoted_crlf, 0, &mut spans, 100, 10, 10),
            BorrowedQuoted::Parsed {
                consumed: 7,
                terminated: true
            }
        );

        let input_stray_quote = b"a\"b\n";
        assert!(spans.begin(input_stray_quote, input_stray_quote.len()));
        assert_eq!(
            try_parse_default_borrowed_record(input_stray_quote, 0, &mut spans, 100, 10, 10),
            BorrowedQuoted::Unsupported
        );

        // Additional cases for try_parse_default_quoted_prefix, try_parse_default_interior_prefix, try_parse_default_record
        let mut out = RecordStorage::new();
        assert!(try_parse_default_quoted_prefix::<false>(b"", &mut out, 100, 10, 10).is_none());
        assert!(
            try_parse_default_quoted_prefix::<false>(b"\"unclosed", &mut out, 100, 10, 10)
                .is_none()
        );
        assert!(try_parse_default_quoted_prefix::<true>(b"", &mut out, 100, 10, 10).is_none());
        assert!(
            try_parse_default_quoted_prefix::<true>(b"\"unclosed", &mut out, 100, 10, 10).is_none()
        );
        assert!(try_parse_default_interior_prefix::<false>(b"", &mut out, 100, 10, 10).is_none());
        assert!(
            try_parse_default_interior_prefix::<false>(b"a,\"unclosed", &mut out, 100, 10, 10)
                .is_none()
        );
        assert!(try_parse_default_interior_prefix::<false>(b"a", &mut out, 100, 10, 10).is_none());
        assert!(try_parse_default_interior_prefix::<true>(b"", &mut out, 100, 10, 10).is_none());
        assert!(
            try_parse_default_interior_prefix::<true>(b"a,\"unclosed", &mut out, 100, 10, 10)
                .is_none()
        );
        assert!(try_parse_default_interior_prefix::<true>(b"a", &mut out, 100, 10, 10).is_none());
        assert!(try_parse_default_record::<false>(b"", &mut out, 100, 10, 10).is_none());
        assert!(try_parse_default_record::<false>(b"\"unclosed", &mut out, 100, 10, 10).is_none());
        assert!(try_parse_default_record::<false>(b"unclosed", &mut out, 100, 10, 10).is_none());
        assert!(try_parse_default_record::<true>(b"", &mut out, 100, 10, 10).is_none());
        assert!(try_parse_default_record::<true>(b"\"unclosed", &mut out, 100, 10, 10).is_none());
        assert!(try_parse_default_record::<true>(b"unclosed", &mut out, 100, 10, 10).is_none());
        assert_eq!(
            try_parse_default_quoted_prefix::<false>(b"a,b\n", &mut out, 100, 10, 10),
            Some((0, false))
        );
        out.clear();
        assert_eq!(
            try_parse_default_quoted_prefix::<true>(b"a,b\n", &mut out, 100, 10, 10),
            Some((0, false))
        );
        out.clear();
        assert_eq!(
            try_parse_default_quoted_prefix::<true>(
                b"\"escaped\"\"quote\"\n",
                &mut out,
                100,
                20,
                10
            ),
            Some((17, true))
        );
        out.clear();
        assert_eq!(
            try_parse_default_quoted_prefix::<false>(
                b"\"escaped\"\"quote\"\n",
                &mut out,
                100,
                20,
                10
            ),
            Some((17, true))
        );
        out.clear();
        assert!(
            try_parse_default_interior_prefix::<false>(b"a\"quote,b\n", &mut out, 100, 10, 10)
                .is_none()
        );
        out.clear();
        assert!(
            try_parse_default_interior_prefix::<true>(b"a\"quote,b\n", &mut out, 100, 10, 10)
                .is_none()
        );
        out.clear();
        assert_eq!(
            try_parse_default_interior_prefix::<false>(b"a\r\n", &mut out, 100, 10, 10),
            Some((3, true))
        );
        out.clear();
        assert_eq!(
            try_parse_default_interior_prefix::<false>(b"a\n", &mut out, 100, 10, 10),
            Some((2, true))
        );
        out.clear();
        assert_eq!(
            try_parse_default_interior_prefix::<true>(
                b"a,\"escaped\"\"quote\",b\n",
                &mut out,
                100,
                20,
                10
            ),
            Some((19, false))
        );
        out.clear();
        assert_eq!(
            try_parse_default_interior_prefix::<false>(
                b"a,\"escaped\"\"quote\",b\n",
                &mut out,
                100,
                20,
                10
            ),
            Some((19, false))
        );
        out.clear();
        assert_eq!(
            try_parse_default_interior_prefix::<false>(b"a,\"b\"\n", &mut out, 100, 10, 10),
            Some((6, true))
        );
        out.clear();
        assert!(
            try_parse_default_record::<false>(b"\"a\",\"b\"\n", &mut out, 100, 10, 1).is_none()
        );
        out.clear();
        assert!(try_parse_default_record::<true>(b"\"a\",\"b\"\n", &mut out, 100, 10, 1).is_none());
        out.clear();
        assert_eq!(
            try_parse_default_record::<false>(b"\"a\"\r\n", &mut out, 100, 10, 10),
            Some(5)
        );
        out.clear();
        assert_eq!(
            try_parse_default_record::<true>(b"\"a\"\r\n", &mut out, 100, 10, 10),
            Some(5)
        );
        out.clear();
        assert!(
            try_parse_default_record::<false>(b"a\"quote,b\n", &mut out, 100, 10, 10).is_none()
        );
        out.clear();
        assert!(try_parse_default_record::<true>(b"a\"quote,b\n", &mut out, 100, 10, 10).is_none());
        out.clear();
        assert!(try_parse_default_record::<false>(b"a,b\n", &mut out, 100, 10, 1).is_none());
        out.clear();
        assert!(try_parse_default_record::<true>(b"a,b\n", &mut out, 100, 10, 1).is_none());
        out.clear();

        let input_max_fields = b"a,b,c";
        assert!(spans.begin(input_max_fields, input_max_fields.len()));
        assert_eq!(
            try_parse_default_borrowed_record(input_max_fields, 0, &mut spans, 100, 10, 1),
            BorrowedQuoted::Unsupported
        );

        let input_field_limit = b"toolongfield,b";
        assert!(spans.begin(input_field_limit, input_field_limit.len()));
        assert_eq!(
            try_parse_default_borrowed_record(input_field_limit, 0, &mut spans, 100, 2, 10),
            BorrowedQuoted::Unsupported
        );
        let input_quoted_field_overflow = b"\"toolongfield\",b";
        assert!(spans.begin(
            input_quoted_field_overflow,
            input_quoted_field_overflow.len()
        ));
        assert_eq!(
            try_parse_default_borrowed_record(
                input_quoted_field_overflow,
                0,
                &mut spans,
                100,
                2,
                10
            ),
            BorrowedQuoted::Unsupported
        );

        let input_nl_limit = b"toolongfield\n";
        assert!(spans.begin(input_nl_limit, input_nl_limit.len()));
        assert_eq!(
            try_parse_default_borrowed_record(input_nl_limit, 0, &mut spans, 100, 2, 10),
            BorrowedQuoted::Unsupported
        );

        assert_eq!(
            try_parse_default_borrowed_record(b"a", 5, &mut spans, 10, 10, 10),
            BorrowedQuoted::Unsupported
        );

        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            let mut qout = RecordStorage::new();
            let mut padded_unquoted = b"toolongfield,b\n".to_vec();
            padded_unquoted.resize(64, b' ');
            assert!(
                try_parse_default_quoted_record_structural::<true>(&padded_unquoted, &mut qout, 2)
                    .is_none()
            );

            let mut padded_simple_q = b"\"toolongfield\",b\n".to_vec();
            padded_simple_q.resize(64, b' ');
            assert!(
                try_parse_default_quoted_record_structural::<true>(&padded_simple_q, &mut qout, 2)
                    .is_none()
            );

            let mut padded_odd_q = b"\"a\"c\"\",b\n".to_vec();
            padded_odd_q.resize(64, b' ');
            assert!(
                try_parse_default_quoted_record_structural::<false>(&padded_odd_q, &mut qout, 100)
                    .is_none()
            );

            let mut empty_bytes = Vec::new();
            assert!(!append_masked_quoted_field::<false>(
                b"a",
                &mut empty_bytes,
                0,
                0,
                0,
                10
            ));
        }
    }

    fn assert_owned(storage: &RecordStorage, expected: &[&[u8]]) {
        let fields: Vec<&[u8]> = storage.iter().collect();
        assert_eq!(fields, expected);
        assert_eq!(
            storage.ends(),
            &expected
                .iter()
                .scan(0, |end, field| {
                    *end += field.len();
                    Some(*end)
                })
                .collect::<Vec<_>>()
        );
    }

    fn assert_borrowed(storage: &SpanStorage, input: &[u8], expected: &[&[u8]]) {
        let resolved = storage.resolved(input);
        let fields: Vec<&[u8]> = resolved.fields().collect();
        assert_eq!(fields, expected);
    }

    #[test]
    fn scalar_parsers_honor_exact_record_field_and_count_boundaries() {
        let mut out = RecordStorage::new();

        assert_eq!(
            try_parse_default_quoted_prefix::<true>(b"\"a\"\n", &mut out, 4, 1, 1),
            Some((4, true))
        );
        assert_owned(&out, &[b"a"]);
        out.clear();
        assert_eq!(
            try_parse_default_quoted_prefix::<true>(b"\"a\"\n", &mut out, 3, 1, 1),
            None
        );
        out.clear();
        assert_eq!(
            try_parse_default_quoted_prefix::<false>(b"\"a\"\n", &mut out, 4, 0, 1),
            Some((4, true))
        );
        assert_owned(&out, &[b"a"]);
        out.clear();

        let escaped = b"\"ab\"\"cd\"\"ef\"\n";
        assert_eq!(
            try_parse_default_quoted_prefix::<true>(escaped, &mut out, escaped.len(), 10, 1),
            Some((escaped.len(), true))
        );
        assert_owned(&out, &[b"ab\"cd\"ef"]);
        out.clear();
        assert_eq!(
            try_parse_default_quoted_prefix::<true>(escaped, &mut out, escaped.len(), 9, 1),
            None
        );
        out.clear();

        assert_eq!(
            try_parse_default_quoted_prefix::<true>(b"\"a\",\"b\"\n", &mut out, 8, 1, 2),
            Some((8, true))
        );
        assert_owned(&out, &[b"a", b"b"]);
        out.clear();
        assert_eq!(
            try_parse_default_quoted_prefix::<true>(b"\"a\",\"b\"\n", &mut out, 8, 1, 1),
            None
        );
        out.clear();
        assert_eq!(
            try_parse_default_quoted_prefix::<true>(b"\"a\",plain\n", &mut out, 10, 1, 1),
            Some((4, false))
        );
        assert_owned(&out, &[b"a"]);
        out.clear();

        assert_eq!(
            try_parse_default_interior_prefix::<true>(b"a,\"b\"\n", &mut out, 6, 1, 2),
            Some((6, true))
        );
        assert_owned(&out, &[b"a", b"b"]);
        out.clear();
        assert_eq!(
            try_parse_default_interior_prefix::<true>(b"a,\"b\"\n", &mut out, 5, 1, 2),
            None
        );
        out.clear();
        assert_eq!(
            try_parse_default_interior_prefix::<false>(b"abcd,\"ef\",g\n", &mut out, 12, 0, 2),
            Some((10, false))
        );
        assert_owned(&out, &[b"abcd", b"ef"]);
        out.clear();
        assert_eq!(
            try_parse_default_interior_prefix::<true>(b"abcd,\"ef\",g\n", &mut out, 12, 3, 2),
            None
        );
        out.clear();
        assert_eq!(
            try_parse_default_interior_prefix::<true>(b"a\r\n", &mut out, 3, 1, 1),
            Some((3, true))
        );
        assert_owned(&out, &[b"a"]);
        out.clear();

        let complete = b"a,\"b\"\"c\",d\r\n";
        assert_eq!(
            try_parse_default_record::<true>(complete, &mut out, complete.len(), 4, 3),
            Some(complete.len())
        );
        assert_owned(&out, &[b"a", b"b\"c", b"d"]);
        out.clear();
        assert_eq!(
            try_parse_default_record::<true>(complete, &mut out, complete.len() - 1, 4, 3),
            None
        );
        out.clear();
        assert_eq!(
            try_parse_default_record::<true>(complete, &mut out, complete.len(), 3, 3),
            None
        );
        out.clear();
        assert_eq!(
            try_parse_default_record::<false>(b"abcd,\"ef\"\n", &mut out, 10, 0, 2),
            Some(10)
        );
        assert_owned(&out, &[b"abcd", b"ef"]);
        out.clear();
        assert_eq!(
            try_parse_default_record::<true>(b"a,b\n", &mut out, 4, 1, 1),
            None
        );
        assert_owned(&out, &[b"a"]);
        out.clear();

        assert_eq!(
            try_parse_default_interior_prefix::<true>(b"a,b,\"c\"\n", &mut out, 8, 1, 1),
            None
        );
        assert_owned(&out, &[b"a"]);
        out.clear();

        assert_eq!(
            try_parse_default_record::<true>(b"abc\n", &mut out, 4, 3, 1),
            Some(4)
        );
        assert_owned(&out, &[b"abc"]);
        out.clear();

        assert_eq!(
            try_parse_default_record::<true>(b"\"a\"\rX", &mut out, 5, 3, 1),
            None
        );
    }

    #[test]
    fn borrowed_record_reports_exact_spans_sources_and_rolls_back_failures() {
        let input = b"XX\"a\",\"b\"\"c\",d\r\nTAIL";
        let mut storage = SpanStorage::with_capacity(8);
        assert!(storage.begin(input, input.len()));
        assert_eq!(
            try_parse_default_borrowed_record(input, 2, &mut storage, 14, 6, 3),
            BorrowedQuoted::Parsed {
                consumed: 14,
                terminated: true
            }
        );
        assert_borrowed(&storage, input, &[b"a", b"b\"c", b"d"]);
        let spans: Vec<(Source, core::ops::Range<usize>, bool)> = storage
            .iter()
            .map(|span| (span.source(), span.range(), span.is_quoted()))
            .collect();
        assert_eq!(
            spans,
            vec![
                (Source::Input, 3..4, true),
                (Source::Scratch, 0..3, true),
                (Source::Input, 13..14, false),
            ]
        );

        assert!(storage.begin(input, input.len()));
        assert_eq!(
            try_parse_default_borrowed_record(input, 2, &mut storage, 13, 6, 3),
            BorrowedQuoted::TooLong
        );
        assert_eq!(storage.len(), 0);
        assert_eq!(storage.scratch_len(), 0);

        let exact = b"\"a\"";
        assert!(storage.begin(exact, exact.len()));
        assert_eq!(
            try_parse_default_borrowed_record(exact, 0, &mut storage, 3, 3, 1),
            BorrowedQuoted::Parsed {
                consumed: 3,
                terminated: false
            }
        );
        assert_borrowed(&storage, exact, &[b"a"]);
        assert_eq!(storage.iter().next().map(|span| span.range()), Some(1..2));
        assert!(storage.begin(exact, exact.len()));
        assert_eq!(
            try_parse_default_borrowed_record(exact, 0, &mut storage, 3, 2, 1),
            BorrowedQuoted::Unsupported
        );

        let end = b"a,";
        assert!(storage.begin(end, end.len()));
        assert_eq!(
            try_parse_default_borrowed_record(end, end.len(), &mut storage, 1, 0, 1),
            BorrowedQuoted::Parsed {
                consumed: 0,
                terminated: false
            }
        );
        assert_borrowed(&storage, end, &[b""]);
        assert_eq!(storage.iter().next().map(|span| span.range()), Some(2..2));

        let malformed = b"p\"a\"x";
        assert!(storage.begin(malformed, malformed.len()));
        {
            let (spans, scratch) = storage.parts_mut();
            spans.push(Span::from_valid_range(Source::Input, 0..1, false));
            scratch.extend_from_slice(b"seed");
        }
        assert_eq!(
            try_parse_default_borrowed_record(malformed, 1, &mut storage, 4, 4, 2),
            BorrowedQuoted::Unsupported
        );
        assert_eq!(storage.len(), 1);
        assert_eq!(storage.scratch_len(), 4);
        assert_borrowed(&storage, malformed, &[b"p"]);

        let unquoted = b"prea,b\r\npost";
        assert!(storage.begin(unquoted, unquoted.len()));
        assert_eq!(
            try_parse_default_borrowed_record(unquoted, 3, &mut storage, 5, 1, 2),
            BorrowedQuoted::Parsed {
                consumed: 5,
                terminated: true
            }
        );
        assert_borrowed(&storage, unquoted, &[b"a", b"b"]);
        let ranges: Vec<_> = storage.iter().map(|span| span.range()).collect();
        assert_eq!(ranges, vec![3..4, 5..6]);

        let trailing_empty = b"xxa,";
        assert!(storage.begin(trailing_empty, trailing_empty.len()));
        assert_eq!(
            try_parse_default_borrowed_record(trailing_empty, 2, &mut storage, 2, 1, 2),
            BorrowedQuoted::Parsed {
                consumed: 2,
                terminated: false
            }
        );
        assert_borrowed(&storage, trailing_empty, &[b"a", b""]);
        let trailing_spans: Vec<_> = storage
            .iter()
            .map(|span| (span.range(), span.is_quoted()))
            .collect();
        assert_eq!(trailing_spans, vec![(2..3, false), (4..4, false)]);

        let truncated_quote = b"xx\"a\"x";
        assert!(storage.begin(truncated_quote, truncated_quote.len()));
        assert_eq!(
            try_parse_default_borrowed_record(truncated_quote, 2, &mut storage, 3, 3, 1),
            BorrowedQuoted::Unsupported
        );
        assert_eq!(storage.len(), 0);

        let final_plain = b"xxa,bc";
        assert!(storage.begin(final_plain, final_plain.len()));
        assert_eq!(
            try_parse_default_borrowed_record(final_plain, 2, &mut storage, 4, 2, 2),
            BorrowedQuoted::Parsed {
                consumed: 4,
                terminated: false
            }
        );
        assert_borrowed(&storage, final_plain, &[b"a", b"bc"]);
        let final_spans: Vec<_> = storage
            .iter()
            .map(|span| (span.range(), span.is_quoted()))
            .collect();
        assert_eq!(final_spans, vec![(2..3, false), (4..6, false)]);

        let multiple_plain = b"xxa,bc,d\n";
        assert!(storage.begin(multiple_plain, multiple_plain.len()));
        assert_eq!(
            try_parse_default_borrowed_record(multiple_plain, 2, &mut storage, 7, 2, 3),
            BorrowedQuoted::Parsed {
                consumed: 7,
                terminated: true
            }
        );
        assert_borrowed(&storage, multiple_plain, &[b"a", b"bc", b"d"]);
        assert!(storage.iter().all(|span| !span.is_quoted()));

        let empty_line = b"xx\n";
        assert!(storage.begin(empty_line, empty_line.len()));
        assert_eq!(
            try_parse_default_borrowed_record(empty_line, 2, &mut storage, 1, 0, 1),
            BorrowedQuoted::Parsed {
                consumed: 1,
                terminated: true
            }
        );
        assert_borrowed(&storage, empty_line, &[b""]);
        assert_eq!(storage.iter().next().map(|span| span.range()), Some(2..2));
    }

    #[test]
    fn bit_helpers_match_simple_reference_implementations() {
        for bit in 0..=70 {
            let expected = if bit >= 64 {
                u64::MAX
            } else if bit == 0 {
                0
            } else {
                u64::MAX >> (64 - bit)
            };
            assert_eq!(lower_bits(bit), expected, "bit={bit}");
        }

        #[cfg(target_arch = "x86_64")]
        {
            assert_eq!(bits_before_first(0), u64::from(u32::MAX));
            for bit in 0..32 {
                assert_eq!(bits_before_first(1_u32 << bit), lower_bits(bit));
            }
        }

        let mut masks = vec![
            0,
            1,
            1 << 31,
            1 << 32,
            1 << 63,
            u64::MAX,
            0xAAAA_AAAA_5555_5555,
        ];
        let mut state = 0xC0DE_CAFE_F00D_BAAD_u64;
        for _ in 0..256 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            masks.push(state);
        }
        for quotes in masks {
            let mut inside = false;
            let mut expected = 0;
            for bit in 0..64 {
                if quotes & (1_u64 << bit) != 0 {
                    inside = !inside;
                }
                if inside {
                    expected |= 1_u64 << bit;
                }
            }
            assert_eq!(quote_parity(quotes), expected, "quotes={quotes:#018x}");
        }
    }

    #[test]
    fn small_materialization_helpers_preserve_all_bytes_and_exact_field_limit() {
        let mut bytes = b"prefix".to_vec();
        for len in 0..=8 {
            let segment: Vec<u8> = (0..len).map(|index| b'a' + index as u8).collect();
            let old_len = bytes.len();
            append_segment(&mut bytes, &segment);
            assert_eq!(&bytes[old_len..], segment);
        }

        let mut ends = Vec::new();
        assert!(finish_field(b"abc", &mut ends, 1));
        assert_eq!(ends, [3]);
        assert!(!finish_field(b"abcd", &mut ends, 1));
        assert_eq!(ends, [3]);
    }

    #[test]
    fn quoted_field_budget_is_exact_and_disabled_when_unchecked() {
        let mut remaining = 4;
        assert!(consume_field_raw_bytes::<true>(&mut remaining, 2));
        assert_eq!(remaining, 2);
        assert!(consume_field_raw_bytes::<true>(&mut remaining, 2));
        assert_eq!(remaining, 0);
        assert!(!consume_field_raw_bytes::<true>(&mut remaining, 1));
        assert_eq!(remaining, 0);

        let mut unchecked = usize::MAX;
        assert!(consume_field_raw_bytes::<false>(&mut unchecked, usize::MAX));
        assert_eq!(unchecked, usize::MAX);

        let mut exhausted = 0;
        assert!(!consume_field_raw_bytes::<true>(&mut exhausted, 1));
        assert_eq!(exhausted, 0);
    }

    #[test]
    fn escaped_pair_limits_are_observed_at_each_scalar_call_site() {
        let quoted = b"\"ab\"\"\"\n";
        let mut output = RecordStorage::new();
        assert_eq!(
            try_parse_default_quoted_prefix::<true>(quoted, &mut output, quoted.len(), 4, 1),
            Some((quoted.len(), true))
        );
        assert_owned(&output, &[b"ab\""]);
        output.clear();
        assert_eq!(
            try_parse_default_quoted_prefix::<true>(quoted, &mut output, quoted.len(), 3, 1),
            None
        );

        let interior = b"a,\"ab\"\"\"\n";
        output.clear();
        assert_eq!(
            try_parse_default_interior_prefix::<true>(interior, &mut output, interior.len(), 4, 2),
            Some((interior.len(), true))
        );
        assert_owned(&output, &[b"a", b"ab\""]);
        output.clear();
        assert_eq!(
            try_parse_default_interior_prefix::<true>(interior, &mut output, interior.len(), 3, 2),
            None
        );

        output.clear();
        assert_eq!(
            try_parse_default_record::<true>(interior, &mut output, interior.len(), 4, 2),
            Some(interior.len())
        );
        assert_owned(&output, &[b"a", b"ab\""]);
        output.clear();
        assert_eq!(
            try_parse_default_record::<true>(interior, &mut output, interior.len(), 3, 2),
            None
        );
    }

    #[test]
    fn borrowed_record_counts_quoted_fields_and_offsets_suffix_spans() {
        let mut storage = SpanStorage::with_capacity(4);

        let exact_count = b"\"a\",b";
        assert!(storage.begin(exact_count, exact_count.len()));
        assert_eq!(
            try_parse_default_borrowed_record(exact_count, 0, &mut storage, 5, 3, 2),
            BorrowedQuoted::Parsed {
                consumed: 5,
                terminated: false
            }
        );
        assert_borrowed(&storage, exact_count, &[b"a", b"b"]);

        let over_count = b"\"a\",\"b\",c";
        assert!(storage.begin(over_count, over_count.len()));
        assert_eq!(
            try_parse_default_borrowed_record(over_count, 0, &mut storage, 9, 3, 2),
            BorrowedQuoted::Unsupported
        );
        assert_eq!(storage.len(), 0);
        assert_eq!(storage.scratch_len(), 0);

        let offset_suffix = b"xxx\"a\",bc";
        assert!(storage.begin(offset_suffix, offset_suffix.len()));
        assert_eq!(
            try_parse_default_borrowed_record(offset_suffix, 3, &mut storage, 6, 3, 2),
            BorrowedQuoted::Parsed {
                consumed: 6,
                terminated: false
            }
        );
        assert_borrowed(&storage, offset_suffix, &[b"a", b"bc"]);
        let ranges: Vec<_> = storage.iter().map(|span| span.range()).collect();
        assert_eq!(ranges, vec![4..5, 7..9]);
    }

    #[cfg(target_arch = "x86_64")]
    fn padded_plain_record(newline: usize, crlf: bool) -> Vec<u8> {
        let blocks = (newline + 32) / 32;
        let mut input = vec![b'a'; blocks * 32];
        for separator in [1, 15, 31, 32, 47, 63, 64, 79] {
            if separator < newline {
                input[separator] = b',';
            }
        }
        if crlf && newline > 0 {
            input[newline - 1] = b'\r';
        }
        input[newline] = b'\n';
        input
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn borrowed_plain_simd_matches_delimiter_layout_at_every_block_boundary() {
        let boundary = vec![b'a'; 32];
        let mut boundary_storage = SpanStorage::with_capacity(1);
        assert!(boundary_storage.begin(&boundary, boundary.len()));
        boundary_storage
            .spans_mut()
            .push(Span::from_valid_range(Source::Input, 0..1, false));
        assert_eq!(
            try_parse_default_borrowed_plain(&boundary, boundary.len(), &mut boundary_storage),
            None
        );
        assert_eq!(boundary_storage.len(), 1);

        if !avx2_available() {
            return;
        }
        for newline in 0..96 {
            let crlf = newline % 11 == 0 && newline > 0;
            let suffix = padded_plain_record(newline, crlf);
            let mut input = b"xyz".to_vec();
            input.extend_from_slice(&suffix);
            let mut storage = SpanStorage::with_capacity(16);
            assert!(storage.begin(&input, input.len()));
            assert_eq!(
                try_parse_default_borrowed_plain(&input, 3, &mut storage),
                Some(newline + 1),
                "newline={newline}"
            );

            let record_end = if crlf { newline - 1 } else { newline };
            let expected: Vec<&[u8]> = suffix[..record_end].split(|&byte| byte == b',').collect();
            assert_borrowed(&storage, &input, &expected);
            let mut field_start = 3;
            for (span, field) in storage.iter().zip(&expected) {
                assert_eq!(span.source(), Source::Input);
                assert_eq!(span.range(), field_start..field_start + field.len());
                assert!(!span.is_quoted());
                field_start += field.len() + 1;
            }
        }

        let mut quoted = vec![b'a'; 64];
        quoted[5] = b'"';
        quoted[20] = b'\n';
        let mut storage = SpanStorage::with_capacity(4);
        assert!(storage.begin(&quoted, quoted.len()));
        storage
            .spans_mut()
            .push(Span::from_valid_range(Source::Input, 0..1, false));
        assert_eq!(
            try_parse_default_borrowed_plain(&quoted, 0, &mut storage),
            None
        );
        assert_eq!(storage.len(), 1);

        let no_newline = vec![b'a'; MAX_BATCHED_RECORD];
        assert!(storage.begin(&no_newline, no_newline.len()));
        assert_eq!(
            try_parse_default_borrowed_plain(&no_newline, 0, &mut storage),
            None
        );
        assert_eq!(storage.len(), 0);

        let mut beyond_limit = vec![b'a'; MAX_BATCHED_RECORD + 32];
        beyond_limit[MAX_BATCHED_RECORD] = b'\n';
        assert!(storage.begin(&beyond_limit, beyond_limit.len()));
        assert_eq!(
            try_parse_default_borrowed_plain(&beyond_limit, 0, &mut storage),
            None
        );
        assert_eq!(storage.len(), 0);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn packed_plain_simd_matches_the_scalar_parser_and_clears_failures() {
        for (avx2, bmi2, expected) in [
            (false, false, false),
            (false, true, false),
            (true, false, false),
            (true, true, true),
        ] {
            assert_eq!(packed_features_available(avx2, bmi2), expected);
        }
        assert_eq!(
            default_plain_packed_available(),
            avx2_available() && bmi2_available()
        );
        if !default_plain_packed_available() {
            return;
        }
        for newline in 0..96 {
            let input = padded_plain_record(newline, newline % 13 == 0 && newline > 0);
            let mut scalar = RecordStorage::new();
            let mut packed = RecordStorage::new();
            let expected =
                try_parse_default_record::<false>(&input, &mut scalar, input.len(), 0, usize::MAX);
            assert_eq!(
                try_parse_default_plain_packed(&input, &mut packed),
                expected,
                "newline={newline}"
            );
            assert_eq!(packed.bytes(), scalar.bytes(), "newline={newline}");
            assert_eq!(packed.ends(), scalar.ends(), "newline={newline}");
        }

        let mut output = RecordStorage::new();
        output.append_field(b"seed");
        let mut quoted = vec![b'a'; 32];
        quoted[3] = b'"';
        quoted[20] = b'\n';
        assert_eq!(try_parse_default_plain_packed(&quoted, &mut output), None);
        assert_owned(&output, &[]);

        output.append_field(b"seed");
        assert_eq!(
            try_parse_default_plain_packed(&vec![b'a'; MAX_BATCHED_RECORD], &mut output),
            None
        );
        assert_owned(&output, &[]);

        let mut beyond_limit = vec![b'a'; MAX_BATCHED_RECORD + 32];
        beyond_limit[MAX_BATCHED_RECORD] = b'\n';
        output.append_field(b"seed");
        assert_eq!(
            try_parse_default_plain_packed(&beyond_limit, &mut output),
            None
        );
        assert_owned(&output, &[]);

        let second_block = padded_plain_record(40, false);
        assert_eq!(
            try_parse_default_plain_packed(&second_block, &mut output),
            Some(41)
        );
        let mut scalar = RecordStorage::new();
        assert_eq!(
            try_parse_default_record::<false>(
                &second_block,
                &mut scalar,
                second_block.len(),
                0,
                usize::MAX
            ),
            Some(41)
        );
        assert_eq!(output.bytes(), scalar.bytes());
        assert_eq!(output.ends(), scalar.ends());

        let mut interior_cr = vec![b'a'; 64];
        interior_cr[31] = b'\r';
        interior_cr[40] = b'\n';
        let mut packed_output = RecordStorage::new();
        let mut scalar_output = RecordStorage::new();
        assert_eq!(
            try_parse_default_plain_packed(&interior_cr, &mut packed_output),
            Some(41)
        );
        assert_eq!(
            try_parse_default_record::<false>(
                &interior_cr,
                &mut scalar_output,
                interior_cr.len(),
                0,
                usize::MAX
            ),
            Some(41)
        );
        assert_eq!(packed_output.bytes(), scalar_output.bytes());
        assert_eq!(packed_output.ends(), scalar_output.ends());
        assert_eq!(packed_output.bytes().get(31), Some(&b'\r'));

        let packed = [
            (u64::from_ne_bytes(*b"ignored!"), 0),
            (u64::from_ne_bytes(*b"abxxxxxx"), 2),
            (u64::from_ne_bytes(*b"ignored!"), 0),
            (u64::from_ne_bytes(*b"cdxxxxxx"), 2),
        ];
        let mut bytes = b"z".to_vec();
        let mut ends = vec![1];
        append_packed_plain(&mut bytes, &mut ends, &packed, (1 << 2) | (1 << 4), 4, true);
        assert_eq!(bytes, b"zabcd");
        assert_eq!(ends, [1, 3, 4, 5]);
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn structural_quoted_simd_matches_scalar_layouts_and_rejects_malformed_masks() {
        if !avx2_available() {
            return;
        }

        let mut short_output = RecordStorage::new();
        short_output.append_field(b"seed");
        assert_eq!(
            try_parse_default_quoted_record_structural::<false>(&[b'a'; 63], &mut short_output, 64),
            None
        );
        assert_owned(&short_output, &[b"seed"]);

        for record in [
            b"left,right\n".as_slice(),
            b",\"\",x,\n",
            b"\"ab\"\"cd\",ef\r\n",
            b"0123456789012345678901234567890,\"x\"\"y\",z\n",
            b"\"a\",b,\"c\",d,\"e\",f\n",
        ] {
            let mut input = record.to_vec();
            input.resize(64, b' ');
            let mut scalar = RecordStorage::new();
            let mut structural = RecordStorage::new();
            let consumed =
                try_parse_default_record::<true>(&input, &mut scalar, record.len(), 64, usize::MAX);
            assert_eq!(
                try_parse_default_quoted_record_structural::<true>(&input, &mut structural, 64),
                consumed.map(|consumed| (consumed, true)),
                "{record:?}"
            );
            assert_eq!(structural.bytes(), scalar.bytes(), "{record:?}");
            assert_eq!(structural.ends(), scalar.ends(), "{record:?}");
        }

        let mut limited = b"\"abcd\",x\n".to_vec();
        limited.resize(64, b' ');
        let mut output = RecordStorage::new();
        assert_eq!(
            try_parse_default_quoted_record_structural::<true>(&limited, &mut output, 4),
            Some((9, true))
        );
        assert_owned(&output, &[b"abcd", b"x"]);
        output.clear();
        assert_eq!(
            try_parse_default_quoted_record_structural::<true>(&limited, &mut output, 3),
            None
        );
        assert_owned(&output, &[]);
        assert_eq!(
            try_parse_default_quoted_record_structural::<false>(&limited, &mut output, 0),
            Some((9, true))
        );

        let mut exact_unquoted = b"abcd\n".to_vec();
        exact_unquoted.resize(64, b' ');
        output.clear();
        assert_eq!(
            try_parse_default_quoted_record_structural::<true>(&exact_unquoted, &mut output, 4),
            Some((5, true))
        );
        assert_owned(&output, &[b"abcd"]);
        output.clear();
        output.append_field(b"seed");
        assert_eq!(
            try_parse_default_quoted_record_structural::<true>(&exact_unquoted, &mut output, 3),
            None
        );
        assert_owned(&output, &[]);

        let mut offset_quoted = b"x,\"ab\"\n".to_vec();
        offset_quoted.resize(64, b' ');
        output.clear();
        assert_eq!(
            try_parse_default_quoted_record_structural::<true>(&offset_quoted, &mut output, 4),
            Some((7, true))
        );
        assert_owned(&output, &[b"x", b"ab"]);

        let mut escaped_exact = b"\"a\"\"b\"\n".to_vec();
        escaped_exact.resize(64, b' ');
        output.clear();
        assert_eq!(
            try_parse_default_quoted_record_structural::<true>(&escaped_exact, &mut output, 4),
            Some((7, true))
        );
        assert_owned(&output, &[b"a\"b"]);
        output.clear();
        output.append_field(b"seed");
        assert_eq!(
            try_parse_default_quoted_record_structural::<true>(&escaped_exact, &mut output, 3),
            None
        );
        assert_owned(&output, &[]);

        let mut quoted_crlf_with_following_quote = b"\"a\"\"b\"\r\n\"ignored".to_vec();
        quoted_crlf_with_following_quote.resize(64, b' ');
        output.clear();
        assert_eq!(
            try_parse_default_quoted_record_structural::<true>(
                &quoted_crlf_with_following_quote,
                &mut output,
                4
            ),
            Some((8, true))
        );
        assert_owned(&output, &[b"a\"b"]);

        for malformed in [b"\"a\"x,b\n".as_slice(), b"ab\"c\",d\n"] {
            let mut input = malformed.to_vec();
            input.resize(64, b' ');
            output.clear();
            output.append_field(b"seed");
            assert_eq!(
                try_parse_default_quoted_record_structural::<false>(&input, &mut output, 64),
                None
            );
            assert_owned(&output, &[]);
            output.append_field(b"seed");
            assert_eq!(
                try_parse_default_quoted_record_structural_appending::<false>(
                    &input,
                    &mut output,
                    64
                ),
                None
            );
            assert_owned(&output, &[b"seed"]);
        }

        let valid = b"\"a\"\"b\"";
        let valid_quotes = (1 << 0) | (1 << 2) | (1 << 3) | (1 << 5);
        let mut bytes = b"prefix".to_vec();
        bytes.reserve(valid.len());
        assert!(append_masked_quoted_field::<true>(
            valid,
            &mut bytes,
            0,
            valid.len(),
            valid_quotes,
            4
        ));
        assert_eq!(bytes, b"prefixa\"b");

        for quotes in [
            valid_quotes & !(1 << 0),
            valid_quotes & !(1 << 5),
            valid_quotes & !(1 << 3),
        ] {
            let mut bytes = Vec::new();
            assert!(!append_masked_quoted_field::<false>(
                valid,
                &mut bytes,
                0,
                valid.len(),
                quotes,
                0
            ));
        }
        let mut bytes = Vec::new();
        assert!(!append_masked_quoted_field::<true>(
            valid,
            &mut bytes,
            0,
            valid.len(),
            valid_quotes,
            3
        ));
    }
}
