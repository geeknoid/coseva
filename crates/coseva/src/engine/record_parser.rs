//! Fast-path record parsers used by the engine.

use super::*;

#[inline(always)]
#[expect(
    clippy::inline_always,
    reason = "the scalar-prefix adapter must not add a second call boundary"
)]
pub(super) fn try_parse_default_record<const CHECK_FIELD_LIMIT: bool>(
    input: &[u8],
    output: &mut RecordStorage,
) -> Option<usize> {
    coseva_unsafe::record::try_parse_default_record::<CHECK_FIELD_LIMIT>(
        input,
        output,
        Limits::DEFAULT.max_record_bytes,
        Limits::DEFAULT.max_field_bytes,
        Limits::DEFAULT.max_fields,
    )
}

#[inline]
fn window_needs_field_limit(input: &[u8]) -> bool {
    input.len() > Limits::DEFAULT.max_field_bytes
}

pub(super) fn try_parse_default_record_windowed(
    input: &[u8],
    output: &mut RecordStorage,
) -> Option<usize> {
    if window_needs_field_limit(input) {
        try_parse_default_record::<true>(input, output)
    } else {
        try_parse_default_record::<false>(input, output)
    }
}

#[inline(never)]
pub(super) fn try_parse_default_record_prefix<const CHECK_FIELD_LIMIT: bool>(
    input: &[u8],
    output: &mut RecordStorage,
) -> Option<(usize, bool)> {
    try_parse_default_record::<CHECK_FIELD_LIMIT>(input, output).map(|consumed| (consumed, true))
}

#[cfg(test)]
pub(super) fn try_parse_default_quoted_record_structural_available() -> bool {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        crate::search::avx2_available()
    }
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    {
        false
    }
}

#[cfg(target_arch = "x86_64")]
#[cfg(test)]
#[inline]
pub(super) fn default_plain_packed_available() -> bool {
    coseva_unsafe::record::default_plain_packed_available()
}

#[cfg(target_arch = "x86_64")]
pub(super) fn try_parse_default_plain_packed(
    input: &[u8],
    output: &mut RecordStorage,
) -> Option<usize> {
    coseva_unsafe::record::try_parse_default_plain_packed(input, output)
}

#[cfg(target_arch = "x86_64")]
pub(super) fn try_parse_default_plain_packed_ascii(
    input: &[u8],
    output: &mut RecordStorage,
) -> Option<usize> {
    coseva_unsafe::record::try_parse_default_plain_packed_ascii(input, output)
}

// gamma::skip(fn_value.some, reason = "fabricating zero bytes consumed for the borrowed packed parser leaves its caller retrying the same record while accumulating output")
#[cfg(target_arch = "x86_64")]
pub(super) fn try_parse_default_borrowed_plain(
    input: &[u8],
    record_start: usize,
    spans: &mut SpanStorage,
) -> Option<usize> {
    coseva_unsafe::record::try_parse_default_borrowed_plain(input, record_start, spans)
}

pub(super) fn try_parse_default_borrowed_record(
    input: &[u8],
    record_start: usize,
    spans: &mut SpanStorage,
    limits: Limits,
) -> coseva_unsafe::record::BorrowedQuoted {
    let remaining = input.len().saturating_sub(record_start);
    if remaining <= limits.max_record_bytes
        && remaining <= limits.max_field_bytes
        && remaining < limits.max_fields
    {
        return coseva_unsafe::record::try_parse_default_borrowed_record_bounded(
            input,
            record_start,
            spans,
        );
    }
    coseva_unsafe::record::try_parse_default_borrowed_record(
        input,
        record_start,
        spans,
        limits.max_record_bytes,
        limits.max_field_bytes,
        limits.max_fields,
    )
}

#[inline]
fn structural_input_limit(input: &[u8]) -> usize {
    input.len()
}

// gamma::skip(fn_value.some, reason = "fabricating a zero-length structural parse reports success without consuming input, so streaming callers repeatedly enqueue the same record")
#[inline(never)]
#[expect(
    clippy::semicolon_outside_block,
    reason = "the cfg block contains statements rather than serving as an expression"
)]
#[cfg(test)]
pub(super) fn try_parse_default_quoted_record_structural<const CHECK_FIELD_LIMIT: bool>(
    input: &[u8],
    output: &mut RecordStorage,
) -> Option<(usize, bool)> {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if let Some(parsed) = coseva_unsafe::record::try_parse_default_quoted_record_structural::<
            CHECK_FIELD_LIMIT,
        >(input, output, structural_input_limit(input))
        {
            return Some(parsed);
        }
        output.clear_fields();
    }
    try_parse_default_record_prefix::<CHECK_FIELD_LIMIT>(input, output)
}

pub(super) fn try_parse_default_quoted_record_structural_appending<
    const CHECK_FIELD_LIMIT: bool,
>(
    input: &[u8],
    output: &mut RecordStorage,
) -> Option<(usize, bool)> {
    let initial_fields = output.len();
    let initial_bytes = output.bytes_len();
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if let Some(parsed) =
            coseva_unsafe::record::try_parse_default_quoted_record_structural_appending::<
                CHECK_FIELD_LIMIT,
            >(input, output, structural_input_limit(input))
        {
            return Some(parsed);
        }
        output.truncate_storage(initial_fields, initial_bytes);
    }
    try_parse_default_record_prefix::<CHECK_FIELD_LIMIT>(input, output)
}

pub(super) fn try_parse_default_quoted_record_structural_windowed(
    input: &[u8],
    output: &mut RecordStorage,
) -> Option<(usize, bool)> {
    if window_needs_field_limit(input) {
        try_parse_default_quoted_record_structural_appending::<true>(input, output)
    } else {
        try_parse_default_quoted_record_structural_appending::<false>(input, output)
    }
}

pub(super) fn try_parse_default_interior_record_structural_ascii(
    input: &[u8],
    output: &mut RecordStorage,
) -> Option<(usize, bool)> {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if window_needs_field_limit(input) {
            coseva_unsafe::record::try_parse_default_quoted_record_structural_ascii::<true>(
                input,
                output,
                structural_input_limit(input),
            )
        } else {
            coseva_unsafe::record::try_parse_default_quoted_record_structural_ascii::<false>(
                input,
                output,
                structural_input_limit(input),
            )
        }
    }
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    {
        let _ = (input, output);
        None
    }
}

// #[gamma::skip(fn_value.some, reason = "fabricating a zero-length quoted-prefix parse reports a completed or resumable prefix without consuming input, so owned callers retry the same record until timeout")]
/// Parse the leading quoted fields of a record, stopping at the first field
/// that is not quoted.
///
/// The scalar parser is the best thing there is at a quoted field -- it finds
/// the closing quote with one search -- but it is 46 instructions per field
/// worse than the vectorized kernel at an *unquoted* one, because it pays for
/// a search per field where the kernel pays for one scan per record. A record
/// that opens with a quote usually holds only a few quoted columns and then
/// several plain ones, so parsing the quoted head here and handing the plain
/// tail back to the kernel plays each to its strength.
///
/// Returns the offset reached and whether the record was finished there.
#[inline(always)]
#[expect(
    clippy::inline_always,
    reason = "owned quoted records enter this adapter once per record; specializing its fixed limits in the caller removes a hot wrapper frame"
)]
pub(super) fn try_parse_default_quoted_prefix<const CHECK_FIELD_LIMIT: bool>(
    input: &[u8],
    output: &mut RecordStorage,
) -> Option<(usize, bool)> {
    coseva_unsafe::record::try_parse_default_quoted_prefix::<CHECK_FIELD_LIMIT>(
        input,
        output,
        Limits::DEFAULT.max_record_bytes,
        Limits::DEFAULT.max_field_bytes,
        Limits::DEFAULT.max_fields,
    )
}

/// Parse a record's unquoted head and the quoted field that follows it,
/// stopping at the first unquoted field *after* a quoted one.
///
/// This is [`try_parse_default_quoted_prefix`] for the shape the leading-quote
/// split cannot reach: a record whose first byte is not a quote but which
/// quotes some interior column. The scalar parser reads the short unquoted
/// prefix and the quoted field that made the kernel bail -- each with the one
/// search it is best at -- and then returns, so the plain tail after the quote
/// goes back to the vectorized kernel exactly as a leading-quoted record's does.
///
/// A record that never quotes anything is parsed to its end here, which is what
/// the caller's misprediction wants: a predicted record that turns out plain
/// costs one scalar pass and nothing more, the same as the whole-record parser
/// it replaces would have.
///
/// Returns the offset reached and whether the record was finished there.
/// `false` hands the next field to the kernel; if it is also quoted, the
/// kernel's existing bail path selects the multi-quote parser for later rows.
#[inline(never)]
pub(super) fn try_parse_default_interior_prefix<const CHECK_FIELD_LIMIT: bool>(
    input: &[u8],
    output: &mut RecordStorage,
) -> Option<(usize, bool)> {
    coseva_unsafe::record::try_parse_default_interior_prefix::<CHECK_FIELD_LIMIT>(
        input,
        output,
        Limits::DEFAULT.max_record_bytes,
        Limits::DEFAULT.max_field_bytes,
        Limits::DEFAULT.max_fields,
    )
}

pub(super) fn try_parse_default_interior_prefix_windowed(
    input: &[u8],
    output: &mut RecordStorage,
) -> Option<(usize, bool)> {
    if window_needs_field_limit(input) {
        try_parse_default_interior_prefix::<true>(input, output)
    } else {
        try_parse_default_interior_prefix::<false>(input, output)
    }
}

#[inline(never)]
pub(super) fn try_parse_named_dialect_record<
    const DELIMITER: u8,
    const BACKSLASH: bool,
    const CHECK_FIELD_LIMIT: bool,
>(
    input: &[u8],
    output: &mut RecordStorage,
) -> Option<usize> {
    try_parse_named_dialect_record_runtime(input, output, DELIMITER, BACKSLASH, CHECK_FIELD_LIMIT)
}

fn try_parse_named_dialect_record_runtime(
    input: &[u8],
    output: &mut RecordStorage,
    delimiter: u8,
    backslash: bool,
    check_field_limit: bool,
) -> Option<usize> {
    let scan_end = cmp::min(input.len(), Limits::DEFAULT.max_record_bytes);
    let input = &input[..scan_end];
    let mut location = 0;
    loop {
        let &first = input.get(location)?;
        if first == b'"' {
            let mut field_raw_bytes = 0;
            let mut segment_start = location + 1;
            let mut cursor = segment_start;
            loop {
                // `location` names the first byte obtained above, so
                // `cursor <= input.len()` and search offsets remain in the suffix.
                let remaining = &input[cursor..];
                let relative = if backslash {
                    find2_near(b'"', b'\\', remaining)?
                } else {
                    find1_near(b'"', remaining)?
                };
                let at = cursor + relative;
                let segment = &input[segment_start..at];
                if check_field_limit
                    && segment.len() > Limits::DEFAULT.max_field_bytes - field_raw_bytes
                {
                    return None;
                }
                append_owned_segment(output, segment);
                match check_field_limit {
                    true => field_raw_bytes += segment.len(),
                    false => {}
                }
                let byte = input[at];
                if backslash && byte == b'\\' {
                    let &escaped = input.get(at + 1)?;
                    if escaped != b'"' && escaped != b'\\' {
                        return None;
                    }
                    if check_field_limit && field_raw_bytes > Limits::DEFAULT.max_field_bytes - 2 {
                        return None;
                    }
                    match check_field_limit {
                        true => field_raw_bytes += 2,
                        false => {}
                    }
                    output.push_byte(escaped);
                    // gamma::skip(stmt.delete_assign, reason = "not advancing past a backslash escape makes the inner quote search rediscover the same escape forever")
                    // gamma::skip(literal.int_to_zero, reason = "a zero escape width leaves the quoted-field cursor on the same escape forever")
                    cursor = at + 2;
                    // gamma::skip(stmt.delete_assign, reason = "leaving the next segment at the old offset repeatedly appends the already-consumed escape prefix and grows output without bound")
                    // gamma::skip(assign_value.default, reason = "resetting the segment start to zero repeatedly appends the record prefix after every escape and exhausts memory")
                    segment_start = cursor;
                    // gamma::skip(loop.continue_to_break, reason = "breaking after an escape leaves the outer record loop on the opening quote with no location advance")
                    continue;
                }
                if !backslash && input.get(at + 1) == Some(&b'"') {
                    if check_field_limit && field_raw_bytes > Limits::DEFAULT.max_field_bytes - 2 {
                        return None;
                    }
                    match check_field_limit {
                        true => field_raw_bytes += 2,
                        false => {}
                    }
                    output.push_byte(b'"');
                    // gamma::skip(stmt.delete_assign, reason = "not advancing past a doubled quote makes the inner quote search rediscover the same escape forever")
                    // gamma::skip(assign_value.default, reason = "resetting the cursor after a doubled quote restarts the quoted-field scan and grows output without bound")
                    // gamma::skip(literal.int_to_zero, reason = "a zero doubled-quote width leaves the cursor on the same quote pair forever")
                    cursor = at + 2;
                    segment_start = cursor;
                    // gamma::skip(loop.continue_to_break, reason = "breaking after a doubled quote returns to the outer loop without consuming the quoted field and causes unbounded retries")
                    continue;
                }
                if !finish_default_field(output) {
                    return None;
                }
                let after_quote = at + 1;
                match input.get(after_quote) {
                    Some(&byte) if byte == delimiter => {
                        // gamma::skip(stmt.delete_assign, reason = "not advancing past the delimiter makes the outer record loop parse the delimiter as the next field forever")
                        // gamma::skip(assign_value.default, reason = "resetting location after a quoted field restarts the record and repeatedly appends its first field")
                        location = after_quote + 1;
                        // gamma::skip(loop.break_to_continue, reason = "continuing the inner quoted-field loop after its closing quote repeatedly searches from the settled field")
                        // gamma::skip(loop.delete_break, reason = "deleting the inner-loop exit after a closing quote leaves the parser in an unbounded quote scan")
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
            let remaining = &input[location..];
            let relative = find3_near(delimiter, b'"', b'\n', remaining)?;
            let at = location + relative;
            let byte = input[at];
            if byte == b'"' {
                return None;
            }
            let field_end = if byte == b'\n' && at > location && input[at - 1] == b'\r' {
                at - 1
            } else {
                at
            };
            let segment = &input[location..field_end];
            if check_field_limit && segment.len() > Limits::DEFAULT.max_field_bytes {
                return None;
            }
            append_owned_segment(output, segment);
            if !finish_default_field(output) {
                return None;
            }
            location = at + 1;
            if byte == b'\n' {
                return Some(location);
            }
        }
    }
}

#[expect(
    clippy::inline_always,
    reason = "this check is part of the separately compiled contiguous-record kernel"
)]
#[inline(always)]
fn finish_default_field(output: &mut RecordStorage) -> bool {
    if output.len() == Limits::DEFAULT.max_fields {
        return false;
    }
    output.finish_field();
    true
}

#[cfg(feature = "benchmarking")]
pub(crate) fn count_structurals_scalar(
    input: &[u8],
    delimiter: u8,
    quote: u8,
    record_ending: u8,
) -> usize {
    input
        .iter()
        .filter(|&&byte| byte == delimiter || byte == quote || byte == record_ending)
        .count()
}

#[cfg(feature = "benchmarking")]
pub(crate) fn count_structurals_selected(
    input: &[u8],
    delimiter: u8,
    quote: u8,
    record_ending: u8,
) -> usize {
    crate::search::StructuralBlocks::new(input, delimiter, quote, record_ending)
        .map(|block| block.count())
        .sum::<usize>()
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use core::iter::repeat_n;

    use super::*;
    use coseva_unsafe::storage::RecordStorage as ByteRecord;

    fn filler(byte: u8, len: usize) -> Vec<u8> {
        repeat_n(byte, len).collect()
    }

    fn avx_window(record: &[u8]) -> Vec<u8> {
        let mut input = record.to_vec();
        while input.len() < 64 {
            input.extend_from_slice(b"x,y\n");
        }
        input
    }

    #[cfg(target_arch = "x86_64")]
    fn packed_window(record: &[u8]) -> Vec<u8> {
        let mut input = record.to_vec();
        let required = record.len().div_ceil(32) * 32;
        input.resize(required, b'x');
        input
    }

    #[test]
    fn default_record_bails_when_a_quoted_field_exceeds_the_byte_limit() {
        let max_field = Limits::DEFAULT.max_field_bytes;
        let mut input = Vec::with_capacity(max_field + 2);
        input.push(b'"');
        input.extend(filler(b'x', max_field + 1));
        input.push(b'"');
        let mut output = ByteRecord::new();
        let result = try_parse_default_record::<true>(&input, &mut output);
        assert!(result.is_none(), "an over-long quoted field must bail");
    }

    #[test]
    fn default_record_bails_when_an_unquoted_field_exceeds_the_byte_limit() {
        // A trailing comma is required so the field actually terminates and
        // the byte-limit check is reached, rather than the kernel bailing
        // earlier for lack of any delimiter at all.
        let max_field = Limits::DEFAULT.max_field_bytes;
        let mut input = filler(b'x', max_field + 1);
        input.extend_from_slice(b",z");
        let mut output = ByteRecord::new();
        let result = try_parse_default_record::<true>(&input, &mut output);
        assert!(result.is_none(), "an over-long unquoted field must bail");
    }

    #[test]
    fn default_record_accepts_a_field_exactly_at_the_byte_limit() {
        let max_field = Limits::DEFAULT.max_field_bytes;
        let mut input = filler(b'x', max_field);
        input.push(b'\n');
        let mut output = ByteRecord::new();

        assert_eq!(
            try_parse_default_record::<true>(&input, &mut output),
            Some(input.len())
        );
        assert_eq!(output.len(), 1);
        assert_eq!(output.get(0).map(<[u8]>::len), Some(max_field));
    }

    #[test]
    fn borrowed_record_uses_the_exact_supplied_field_limit() {
        let exact = b"a\n";
        let mut exact_spans = SpanStorage::with_capacity(1);
        assert!(exact_spans.begin(exact, Span::MAX_OFFSET));
        assert_eq!(
            try_parse_default_borrowed_record(exact, 0, &mut exact_spans, Limits::new(16, 1, 2),),
            coseva_unsafe::record::BorrowedQuoted::Parsed {
                consumed: exact.len(),
                terminated: true,
            }
        );

        let over = b"ab\n";
        let mut over_spans = SpanStorage::with_capacity(1);
        assert!(over_spans.begin(over, Span::MAX_OFFSET));
        assert_ne!(
            try_parse_default_borrowed_record(over, 0, &mut over_spans, Limits::new(16, 1, 2),),
            coseva_unsafe::record::BorrowedQuoted::Parsed {
                consumed: over.len(),
                terminated: true,
            }
        );
    }

    #[test]
    fn structural_fallback_clears_stale_output_before_scalar_parse() {
        let field = filler(b'x', 80);
        let mut input = Vec::from(&b"\""[..]);
        input.extend_from_slice(&field);
        input.extend_from_slice(b"\"\n");
        let mut output = ByteRecord::new();
        output.append_field(b"stale");

        assert_eq!(
            try_parse_default_quoted_record_structural::<true>(&input, &mut output),
            Some((input.len(), true))
        );
        assert_eq!(output.len(), 1);
        assert_eq!(output.get(0), Some(field.as_slice()));
    }

    #[test]
    fn default_record_bails_when_a_quoted_last_field_exceeds_the_field_count_limit() {
        let mut input = filler(b',', Limits::DEFAULT.max_fields);
        input.extend_from_slice(b"\"z\"");
        let mut output = ByteRecord::new();
        let result = try_parse_default_record::<false>(&input, &mut output);
        assert!(
            result.is_none(),
            "a quoted field past the field-count limit must bail"
        );
    }

    #[test]
    fn default_record_bails_when_an_unquoted_last_field_exceeds_the_field_count_limit() {
        // The trailing newline lets the field actually terminate, so the
        // field-count check inside `finish_default_field` is reached instead
        // of the kernel bailing earlier for lack of a delimiter.
        let mut input = filler(b',', Limits::DEFAULT.max_fields);
        input.extend_from_slice(b"z\n");
        let mut output = ByteRecord::new();
        let result = try_parse_default_record::<false>(&input, &mut output);
        assert!(
            result.is_none(),
            "an unquoted field past the field-count limit must bail"
        );
    }

    // ── try_parse_default_plain_packed ──────────────────────────────────────

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_64_ci_exercises_the_packed_plain_kernel() {
        // Availability is decided differently in the two configurations, so
        // this asserts what each one can actually promise. With `std` it is
        // run-time detection, and a host that fails this turns every packed
        // kernel test below into a silent no-op. Without `std` there is no
        // detection to do: `coseva_unsafe` falls back to `cfg!(target_feature
        // = ...)`, so the kernel is available exactly when the target was
        // built with AVX2 and BMI2 — which the default `x86_64` target is not.
        #[cfg(feature = "std")]
        assert!(
            default_plain_packed_available(),
            "x86_64 test hosts must expose AVX2 and BMI2 so packed-kernel tests assert behavior"
        );
        #[cfg(not(feature = "std"))]
        assert_eq!(
            default_plain_packed_available(),
            cfg!(all(target_feature = "avx2", target_feature = "bmi2")),
            "without `std` the packed kernel is gated on compile-time target features, \
             not on what the host turns out to support"
        );
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn packed_plain_record_decodes_fields_across_blocks() {
        if !default_plain_packed_available() {
            return;
        }
        let record = b"abcdefghijklmnopqrst,,uvwxyz0123456789,c\r\n";
        let input = packed_window(record);
        let mut output = ByteRecord::new();
        let consumed = try_parse_default_plain_packed(&input, &mut output)
            .expect("the packed parser should finish the record");
        assert_eq!(consumed, record.len());
        assert_eq!(
            output.iter().collect::<Vec<_>>(),
            [
                b"abcdefghijklmnopqrst".as_slice(),
                b"".as_slice(),
                b"uvwxyz0123456789".as_slice(),
                b"c".as_slice(),
            ]
        );
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn packed_plain_record_handles_every_block_edge() {
        if !default_plain_packed_available() {
            return;
        }
        for newline in [0, 31, 32, 63, 64, 95] {
            let mut record = filler(b'x', newline);
            record.push(b'\n');
            let input = packed_window(&record);
            let mut output = ByteRecord::new();
            let consumed = try_parse_default_plain_packed(&input, &mut output)
                .expect("the packed parser should find the boundary newline");
            assert_eq!(consumed, record.len());
            assert_eq!(output.get(0), Some(&record[..newline]));
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn packed_plain_record_clears_partial_output_before_quote_fallback() {
        if !default_plain_packed_available() {
            return;
        }
        let mut record = filler(b'x', 40);
        record.extend_from_slice(b"\"bad\"\n");
        let input = packed_window(&record);
        let mut output = ByteRecord::new();
        assert!(try_parse_default_plain_packed(&input, &mut output).is_none());
        assert!(output.is_empty());
        assert!(output.as_slice().is_empty());
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn packed_plain_record_clears_partial_output_before_short_tail_fallback() {
        if !default_plain_packed_available() {
            return;
        }
        let input = filler(b'x', 40);
        let mut output = ByteRecord::new();
        assert!(try_parse_default_plain_packed(&input, &mut output).is_none());
        assert!(output.is_empty());
        assert!(output.as_slice().is_empty());
    }

    // ── try_parse_default_borrowed_plain ───────────────────────────────────

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn batched_borrowed_plain_record_decodes_fields_across_blocks() {
        if !crate::search::avx2_available() {
            return;
        }
        let prefix = b"skip:";
        let record = b"abcdefghijklmnopqrst,,uvwxyz0123456789,c\r\n";
        let mut input = prefix.to_vec();
        input.extend_from_slice(record);
        input.resize(prefix.len() + record.len().div_ceil(32) * 32, b'x');
        let mut spans = SpanStorage::with_capacity(8);
        assert!(spans.begin(&input, Span::MAX_OFFSET));
        let consumed = try_parse_default_borrowed_plain(&input, prefix.len(), &mut spans)
            .expect("the batched parser should finish the record");
        assert_eq!(consumed, record.len());
        let spans = spans.resolved(&input);
        assert_eq!(
            spans
                .span_iter()
                .enumerate()
                .map(|(index, _)| spans.get(index).expect("span resolves"))
                .collect::<Vec<_>>(),
            [
                b"abcdefghijklmnopqrst".as_slice(),
                b"".as_slice(),
                b"uvwxyz0123456789".as_slice(),
                b"c".as_slice(),
            ]
        );
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn batched_borrowed_plain_record_handles_every_block_edge() {
        if !crate::search::avx2_available() {
            return;
        }
        for newline in [0, 31, 32, 63, 64, 95] {
            let mut record = filler(b'x', newline);
            record.push(b'\n');
            let input = packed_window(&record);
            let mut spans = SpanStorage::with_capacity(1);
            assert!(spans.begin(&input, Span::MAX_OFFSET));
            let consumed = try_parse_default_borrowed_plain(&input, 0, &mut spans)
                .expect("the batched parser should find the boundary newline");
            assert_eq!(consumed, record.len());
            let spans = spans.resolved(&input);
            assert_eq!(spans.len(), 1);
            assert_eq!(spans.get(0), Some(&record[..newline]));
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn batched_borrowed_plain_record_restores_spans_before_fallback() {
        if !crate::search::avx2_available() {
            return;
        }
        let mut record = filler(b'x', 40);
        record.extend_from_slice(b",\"bad\"\n");
        let input = packed_window(&record);
        let mut spans = SpanStorage::with_capacity(8);
        assert!(spans.begin(&input, Span::MAX_OFFSET));
        assert!(spans.try_push_input_bounded(0..1, false, 8, 8));
        assert!(try_parse_default_borrowed_plain(&input, 0, &mut spans).is_none());
        let spans = spans.resolved(&input);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans.get(0), Some(b"x".as_slice()));
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn batched_borrowed_plain_record_restores_spans_before_short_tail_fallback() {
        if !crate::search::avx2_available() {
            return;
        }
        let input = filler(b'x', 40);
        let mut spans = SpanStorage::with_capacity(8);
        assert!(spans.begin(&input, Span::MAX_OFFSET));
        assert!(spans.try_push_input_bounded(0..1, false, 8, 8));
        assert!(try_parse_default_borrowed_plain(&input, 0, &mut spans).is_none());
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn specialized_borrowed_quoted_record_returns_borrowed_fields() {
        let prefix = b"skip:";
        let record = b"\"alpha\",\"b,c\",3\r\n";
        let mut input = prefix.to_vec();
        input.extend_from_slice(record);
        input.resize(64 + prefix.len(), b'x');
        let mut spans = SpanStorage::with_capacity(8);
        assert!(spans.begin(&input, Span::MAX_OFFSET));
        let result =
            try_parse_default_borrowed_record(&input, prefix.len(), &mut spans, Limits::DEFAULT);
        assert_eq!(
            result,
            coseva_unsafe::record::BorrowedQuoted::Parsed {
                consumed: record.len(),
                terminated: true,
            }
        );
        let spans = spans.resolved(&input);
        assert_eq!(
            spans
                .span_iter()
                .enumerate()
                .map(|(index, _)| spans.get(index).expect("span resolves"))
                .collect::<Vec<_>>(),
            [b"alpha".as_slice(), b"b,c".as_slice(), b"3".as_slice()]
        );
    }

    #[test]
    fn specialized_borrowed_quoted_record_unescapes_into_scratch() {
        let record = b"\"a\"\"b\",c\n";
        let input = packed_window(record);
        let mut spans = SpanStorage::with_capacity(8);
        assert!(spans.begin(&input, Span::MAX_OFFSET));
        assert!(spans.try_push_input_bounded(0..1, false, 8, 8));
        let result = try_parse_default_borrowed_record(&input, 0, &mut spans, Limits::DEFAULT);
        assert_eq!(
            result,
            coseva_unsafe::record::BorrowedQuoted::Parsed {
                consumed: record.len(),
                terminated: true,
            }
        );
        assert_eq!(spans.resolved(&input).get(1), Some(b"a\"b".as_slice()));
    }

    #[test]
    fn specialized_borrowed_record_rejects_input_larger_than_storage_bound() {
        let mut spans = SpanStorage::with_capacity(2);
        assert!(spans.begin(b"a", Span::MAX_OFFSET));
        assert_eq!(
            try_parse_default_borrowed_record(b"a,b\n", 0, &mut spans, Limits::DEFAULT),
            coseva_unsafe::record::BorrowedQuoted::Unsupported
        );
        assert!(spans.resolved(b"a").is_empty());
    }

    // ── try_parse_default_quoted_record_structural ───────────────────────────

    #[test]
    fn structural_parser_uses_the_exact_input_window() {
        assert_eq!(structural_input_limit(b"abc"), 3);
    }

    #[test]
    fn quoted_structural_record_decodes_separated_quotes_and_embedded_structurals() {
        if !try_parse_default_quoted_record_structural_available() {
            return;
        }
        let record = b"id,\"a,b\",7,\"x\ny\",\"z\"\r\n";
        let input = avx_window(record);
        let mut output = ByteRecord::new();
        let parsed = try_parse_default_quoted_record_structural::<true>(&input, &mut output)
            .expect("the quote-aware structural path should parse the record");
        assert!(parsed.1);
        assert_eq!(parsed.0, record.len());
        assert_eq!(
            output.iter().collect::<Vec<_>>(),
            [
                b"id".as_slice(),
                b"a,b".as_slice(),
                b"7".as_slice(),
                b"x\ny".as_slice(),
                b"z".as_slice(),
            ]
        );
    }

    #[test]
    fn quoted_structural_record_decodes_doubled_quotes_from_the_mask() {
        if !try_parse_default_quoted_record_structural_available() {
            return;
        }
        let record = b"id,\"a\"\"b\",7,\"x\",\"y\"\n";
        let input = avx_window(record);
        let mut output = ByteRecord::new();
        let parsed = try_parse_default_quoted_record_structural::<true>(&input, &mut output)
            .expect("the scalar fallback should decode doubled quotes");
        assert!(parsed.1);
        assert_eq!(parsed.0, record.len());
        assert_eq!(output.get(1), Some(&b"a\"b"[..]));
    }

    #[test]
    fn quoted_structural_record_decodes_leading_escaped_fields() {
        if !try_parse_default_quoted_record_structural_available() {
            return;
        }
        let record = b"\"Bo\"\"ton\",\"Ma\"\"sachusetts\",4500000,42.3601,-71.0589,true\n";
        let input = avx_window(record);
        let mut output = ByteRecord::new();
        let parsed = try_parse_default_quoted_record_structural::<true>(&input, &mut output)
            .expect("the structural parser should decode the escaped record");
        assert_eq!(parsed, (record.len(), true));
        assert_eq!(
            output.iter().collect::<Vec<_>>(),
            [
                b"Bo\"ton".as_slice(),
                b"Ma\"sachusetts".as_slice(),
                b"4500000".as_slice(),
                b"42.3601".as_slice(),
                b"-71.0589".as_slice(),
                b"true".as_slice(),
            ]
        );
    }

    #[test]
    fn quoted_structural_fallback_preserves_existing_prefix() {
        let mut output = ByteRecord::new();
        output.append_field(b"Boston");
        let parsed = try_parse_default_quoted_record_structural_appending::<true>(
            b"\"Massachusetts\",1\n",
            &mut output,
        )
        .expect("the scalar fallback should complete a short quoted tail");
        assert_eq!(parsed, (18, true));
        assert_eq!(
            output.iter().collect::<Vec<_>>(),
            [
                b"Boston".as_slice(),
                b"Massachusetts".as_slice(),
                b"1".as_slice()
            ]
        );
    }

    #[test]
    fn quoted_structural_record_decodes_doubled_quotes_across_the_mask_boundary() {
        if !try_parse_default_quoted_record_structural_available() {
            return;
        }
        let mut record = b"a,\"".to_vec();
        record.extend(filler(b'x', 28));
        record.extend_from_slice(b"\"\"y\",b\n");
        assert_eq!(&record[31..33], b"\"\"");
        let input = avx_window(&record);
        let mut output = ByteRecord::new();
        let parsed = try_parse_default_quoted_record_structural::<true>(&input, &mut output)
            .expect("the doubled quote crossing the mask boundary should decode");
        assert_eq!(parsed, (record.len(), true));
        let mut expected = filler(b'x', 28);
        expected.extend_from_slice(b"\"y");
        assert_eq!(output.get(1), Some(expected.as_slice()));
    }

    #[test]
    fn quoted_structural_record_decodes_a_doubled_quote_next_to_the_closing_quote() {
        if !try_parse_default_quoted_record_structural_available() {
            return;
        }
        let record = b"a,\"x\"\"\",b\n";
        let input = avx_window(record);
        let mut output = ByteRecord::new();
        let parsed = try_parse_default_quoted_record_structural::<true>(&input, &mut output)
            .expect("an escaped quote immediately before the closing quote should decode");
        assert_eq!(parsed, (record.len(), true));
        assert_eq!(output.get(1), Some(&b"x\""[..]));
    }

    #[test]
    fn quoted_structural_record_handles_each_mask_boundary() {
        if !try_parse_default_quoted_record_structural_available() {
            return;
        }
        for newline in [31, 32, 63] {
            let field = filler(b'x', newline - 6);
            let mut record = b"a,\"".to_vec();
            record.extend_from_slice(&field);
            record.extend_from_slice(b"\",b\n");
            assert_eq!(record.len() - 1, newline);
            let input = avx_window(&record);
            let mut output = ByteRecord::new();
            let parsed = try_parse_default_quoted_record_structural::<true>(&input, &mut output)
                .expect("the record should cross the mask boundary");
            assert!(parsed.1);
            assert_eq!(parsed.0, record.len());
            assert_eq!(output.get(1), Some(field.as_slice()));
        }
    }

    #[test]
    fn quoted_structural_record_falls_back_past_two_blocks() {
        if !try_parse_default_quoted_record_structural_available() {
            return;
        }
        let field = filler(b'x', 80);
        let mut record = b"a,\"".to_vec();
        record.extend_from_slice(&field);
        record.extend_from_slice(b"\",b\n");
        let mut output = ByteRecord::new();
        let parsed = try_parse_default_quoted_record_structural::<true>(&record, &mut output)
            .expect("the scalar fallback should parse a long record");
        assert!(parsed.1);
        assert_eq!(parsed.0, record.len());
        assert_eq!(output.get(1), Some(field.as_slice()));
    }

    // ── try_parse_default_interior_prefix ────────────────────────────────────

    #[test]
    fn interior_prefix_hands_the_plain_tail_after_a_quoted_field_to_the_kernel() {
        let mut output = ByteRecord::new();
        let parsed = try_parse_default_interior_prefix::<false>(b"a,\"b\",c\n", &mut output)
            .expect("a plain head followed by a quoted field is a valid prefix");
        assert!(!parsed.1, "an unquoted tail remains for the kernel");
        assert_eq!(parsed.0, 6, "the offset points at the plain tail field");
        assert_eq!(output.get(0), Some(&b"a"[..]));
        assert_eq!(output.get(1), Some(&b"b"[..]));
        assert_eq!(
            output.len(),
            2,
            "only the head and the quoted field are read"
        );
    }

    #[test]
    fn interior_prefix_parses_a_fully_plain_record_to_its_end() {
        let mut output = ByteRecord::new();
        let parsed = try_parse_default_interior_prefix::<false>(b"a,b,c\n", &mut output)
            .expect("a record that never quotes is parsed to its end here");
        assert!(parsed.1, "the record ends at the newline");
        assert_eq!(parsed.0, 6);
        assert_eq!(output.get(0), Some(&b"a"[..]));
        assert_eq!(output.get(1), Some(&b"b"[..]));
        assert_eq!(output.get(2), Some(&b"c"[..]));
    }

    #[test]
    fn interior_prefix_ends_on_a_trailing_quoted_field() {
        let mut output = ByteRecord::new();
        let parsed = try_parse_default_interior_prefix::<false>(b"a,\"b\"\n", &mut output)
            .expect("a quoted last field terminates the record");
        assert!(parsed.1, "the quoted field ran to the newline");
        assert_eq!(parsed.0, 6);
        assert_eq!(output.get(1), Some(&b"b"[..]));
    }

    #[test]
    fn interior_prefix_hands_an_adjacent_quoted_field_to_the_kernel() {
        let mut output = ByteRecord::new();
        let parsed = try_parse_default_interior_prefix::<false>(b"a,\"b\",\"c\"\n", &mut output)
            .expect("the first quoted field is a valid prefix");
        assert_eq!(parsed, (6, false));
        assert_eq!(output.get(1), Some(&b"b"[..]));
        assert_eq!(output.len(), 2);
    }

    #[test]
    fn interior_prefix_unescapes_a_doubled_quote_in_the_interior_field() {
        let mut output = ByteRecord::new();
        let parsed = try_parse_default_interior_prefix::<false>(b"a,\"b\"\"c\"\n", &mut output)
            .expect("a doubled quote is a valid escape");
        assert!(parsed.1);
        assert_eq!(parsed.0, 9);
        assert_eq!(output.get(1), Some(&b"b\"c"[..]));
    }

    #[test]
    fn interior_prefix_strips_the_carriage_return_of_a_crlf_plain_field() {
        let mut output = ByteRecord::new();
        let parsed = try_parse_default_interior_prefix::<false>(b"a,b\r\n", &mut output)
            .expect("a CRLF-terminated plain record is parsed here");
        assert!(parsed.1);
        assert_eq!(parsed.0, 5);
        assert_eq!(
            output.get(1),
            Some(&b"b"[..]),
            "the carriage return is dropped"
        );
    }

    #[test]
    fn interior_prefix_handles_a_crlf_ending_after_the_interior_quoted_field() {
        let mut output = ByteRecord::new();
        let parsed = try_parse_default_interior_prefix::<false>(b"a,\"b\"\r\n", &mut output)
            .expect("a quoted last field followed by CRLF terminates the record");
        assert!(parsed.1);
        assert_eq!(parsed.0, 7);
        assert_eq!(output.get(1), Some(&b"b"[..]));
    }

    #[test]
    fn interior_prefix_bails_on_a_stray_byte_after_the_interior_quote() {
        let mut output = ByteRecord::new();
        let result = try_parse_default_interior_prefix::<false>(b"a,\"b\"x\n", &mut output);
        assert!(
            result.is_none(),
            "a byte other than a delimiter or terminator after a quote must bail"
        );
    }

    #[test]
    fn interior_prefix_bails_on_a_quote_inside_the_plain_prefix() {
        let mut output = ByteRecord::new();
        let result = try_parse_default_interior_prefix::<false>(b"a\"b,c\n", &mut output);
        assert!(
            result.is_none(),
            "a quote inside an unquoted prefix field must bail for the general loop"
        );
    }

    #[test]
    fn interior_prefix_bails_when_the_plain_prefix_field_exceeds_the_byte_limit() {
        let max_field = Limits::DEFAULT.max_field_bytes;
        let mut input = filler(b'x', max_field + 1);
        input.extend_from_slice(b",\"z\"\n");
        let mut output = ByteRecord::new();
        let result = try_parse_default_interior_prefix::<true>(&input, &mut output);
        assert!(
            result.is_none(),
            "an over-long plain prefix field must bail"
        );
    }

    #[test]
    fn interior_prefix_bails_when_the_interior_quoted_field_exceeds_the_byte_limit() {
        let max_field = Limits::DEFAULT.max_field_bytes;
        let mut input = Vec::with_capacity(max_field + 6);
        input.extend_from_slice(b"p,\"");
        input.extend(filler(b'x', max_field + 1));
        input.extend_from_slice(b"\"\n");
        let mut output = ByteRecord::new();
        let result = try_parse_default_interior_prefix::<true>(&input, &mut output);
        assert!(
            result.is_none(),
            "an over-long interior quoted field must bail"
        );
    }

    #[test]
    fn prefix_parsers_accept_fields_exactly_at_the_byte_limit() {
        let max_field = Limits::DEFAULT.max_field_bytes;

        let mut quoted = Vec::from(&b"\""[..]);
        quoted.extend(filler(b'x', max_field));
        quoted.extend_from_slice(b"\"\n");
        let mut quoted_output = ByteRecord::new();
        assert_eq!(
            try_parse_default_quoted_prefix::<true>(&quoted, &mut quoted_output),
            Some((quoted.len(), true))
        );
        assert_eq!(quoted_output.get(0).map(<[u8]>::len), Some(max_field));

        let mut interior = Vec::from(&b"p,\""[..]);
        interior.extend(filler(b'x', max_field));
        interior.extend_from_slice(b"\"\n");
        let mut interior_output = ByteRecord::new();
        assert_eq!(
            try_parse_default_interior_prefix::<true>(&interior, &mut interior_output),
            Some((interior.len(), true))
        );
        assert_eq!(interior_output.get(1).map(<[u8]>::len), Some(max_field));
    }

    // ── try_parse_named_dialect_record ───────────────────────────────────────

    #[test]
    fn named_dialect_record_ends_a_quoted_field_at_crlf() {
        let mut output = ByteRecord::new();
        let consumed =
            try_parse_named_dialect_record::<b';', false, false>(b"\"a\"\r\n", &mut output)
                .expect("quoted field followed by CRLF is a complete record");
        assert_eq!(consumed, 5);
        assert_eq!(output.get(0), Some(&b"a"[..]));
    }

    #[test]
    fn named_dialect_record_bails_when_a_quoted_field_exceeds_the_byte_limit() {
        let max_field = Limits::DEFAULT.max_field_bytes;
        let mut input = Vec::with_capacity(max_field + 2);
        input.push(b'"');
        input.extend(filler(b'x', max_field + 1));
        input.push(b'"');
        let mut output = ByteRecord::new();
        let result = try_parse_named_dialect_record::<b';', false, true>(&input, &mut output);
        assert!(result.is_none(), "an over-long quoted field must bail");
    }

    #[test]
    fn named_dialect_record_bails_when_a_backslash_escape_pushes_a_field_over_the_byte_limit() {
        let max_field = Limits::DEFAULT.max_field_bytes;
        let mut input = Vec::with_capacity(max_field + 4);
        input.push(b'"');
        input.extend(filler(b'x', max_field - 1));
        input.extend_from_slice(b"\\\"\"\n");
        let mut output = ByteRecord::new();
        let result = try_parse_named_dialect_record::<b',', true, true>(&input, &mut output);
        assert!(
            result.is_none(),
            "a backslash escape that pushes a field over the byte limit must bail"
        );
    }

    #[test]
    fn named_dialect_record_bails_when_a_doubled_quote_pushes_a_field_over_the_byte_limit() {
        let max_field = Limits::DEFAULT.max_field_bytes;
        let mut input = Vec::with_capacity(max_field + 4);
        input.push(b'"');
        input.extend(filler(b'x', max_field - 1));
        input.extend_from_slice(b"\"\"\"\n");
        let mut output = ByteRecord::new();
        let result = try_parse_named_dialect_record::<b';', false, true>(&input, &mut output);
        assert!(
            result.is_none(),
            "a doubled quote that pushes a field over the byte limit must bail"
        );
    }

    #[test]
    fn named_dialect_record_bails_when_an_unquoted_field_exceeds_the_byte_limit() {
        // A trailing delimiter is required so the field actually terminates
        // and the byte-limit check is reached.
        let max_field = Limits::DEFAULT.max_field_bytes;
        let mut input = filler(b'x', max_field + 1);
        input.extend_from_slice(b";z");
        let mut output = ByteRecord::new();
        let result = try_parse_named_dialect_record::<b';', false, true>(&input, &mut output);
        assert!(result.is_none(), "an over-long unquoted field must bail");
    }

    #[test]
    fn named_dialect_limit_accounting_is_exact_and_observable() {
        let max_field = Limits::DEFAULT.max_field_bytes;

        let mut exact_plain = filler(b'x', max_field);
        exact_plain.push(b'\n');
        let mut output = ByteRecord::new();
        assert_eq!(
            try_parse_named_dialect_record::<b';', false, true>(&exact_plain, &mut output),
            Some(exact_plain.len())
        );
        assert_eq!(output.get(0).map(<[u8]>::len), Some(max_field));

        output.clear();
        assert_eq!(
            try_parse_named_dialect_record::<b';', false, true>(b"\"a\"\n", &mut output),
            Some(4),
            "the parser starts at the opening quote, not one byte into the field"
        );
        assert_eq!(output.get(0), Some(&b"a"[..]));

        let mut doubled_boundary = Vec::from(&b"\""[..]);
        doubled_boundary.extend(filler(b'x', max_field - 2));
        doubled_boundary.extend_from_slice(b"\"\"\"\n");
        output.clear();
        assert_eq!(
            try_parse_named_dialect_record::<b';', false, true>(&doubled_boundary, &mut output,),
            Some(doubled_boundary.len()),
            "a doubled quote may end exactly on the raw-byte limit"
        );

        let mut backslash_boundary = Vec::from(&b"\""[..]);
        backslash_boundary.extend(filler(b'x', max_field - 2));
        backslash_boundary.extend_from_slice(b"\\\"\"\n");
        output.clear();
        assert_eq!(
            try_parse_named_dialect_record::<b';', true, true>(&backslash_boundary, &mut output,),
            Some(backslash_boundary.len()),
            "a backslash escape may end exactly on the raw-byte limit"
        );

        for backslash in [false, true] {
            let mut exact = Vec::from(&b"\""[..]);
            exact.extend(filler(b'x', max_field - 4));
            if backslash {
                exact.extend_from_slice(b"\\\"");
            } else {
                exact.extend_from_slice(b"\"\"");
            }
            exact.extend_from_slice(b"yy\"\n");
            output.clear();
            let parsed = if backslash {
                try_parse_named_dialect_record::<b';', true, true>(&exact, &mut output)
            } else {
                try_parse_named_dialect_record::<b';', false, true>(&exact, &mut output)
            };
            assert_eq!(parsed, Some(exact.len()));

            let mut over = Vec::from(&b"\""[..]);
            over.extend(filler(b'x', max_field - 4));
            if backslash {
                over.extend_from_slice(b"\\\"");
            } else {
                over.extend_from_slice(b"\"\"");
            }
            over.extend_from_slice(b"yyy\"\n");
            output.clear();
            let parsed = if backslash {
                try_parse_named_dialect_record::<b';', true, true>(&over, &mut output)
            } else {
                try_parse_named_dialect_record::<b';', false, true>(&over, &mut output)
            };
            assert!(
                parsed.is_none(),
                "under-counting the two raw escape bytes would accept this field"
            );
        }

        let mut unchecked = Vec::from(&b"\""[..]);
        unchecked.extend(filler(b'x', max_field + 1));
        unchecked.extend_from_slice(b"\"\n");
        output.clear();
        assert_eq!(
            try_parse_named_dialect_record::<b';', false, false>(&unchecked, &mut output),
            Some(unchecked.len()),
            "the unchecked instantiation must not apply the default field limit"
        );
    }

    #[test]
    fn named_dialect_record_bails_when_a_quoted_last_field_exceeds_the_field_count_limit() {
        let mut input = filler(b';', Limits::DEFAULT.max_fields);
        input.extend_from_slice(b"\"z\"");
        let mut output = ByteRecord::new();
        let result = try_parse_named_dialect_record::<b';', false, false>(&input, &mut output);
        assert!(
            result.is_none(),
            "a quoted field past the field-count limit must bail"
        );
    }

    #[test]
    fn named_dialect_record_bails_when_an_unquoted_last_field_exceeds_the_field_count_limit() {
        // The trailing newline lets the field actually terminate, so the
        // field-count check inside `finish_default_field` is reached.
        let mut input = filler(b';', Limits::DEFAULT.max_fields);
        input.extend_from_slice(b"z\n");
        let mut output = ByteRecord::new();
        let result = try_parse_named_dialect_record::<b';', false, false>(&input, &mut output);
        assert!(
            result.is_none(),
            "an unquoted field past the field-count limit must bail"
        );
    }

    #[test]
    fn named_dialect_record_all_instantiations() {
        fn exercise_dialect_parser<
            const DELIM: u8,
            const BACKSLASH: bool,
            const CHECK_LIMIT: bool,
        >() {
            let mut out = ByteRecord::new();
            // 1. Quoted field followed by newline
            let q_lf = if BACKSLASH {
                "\"hello\\\"world\"\n".to_string()
            } else {
                "\"hello\"\"world\"\n".to_string()
            };
            out.clear();
            assert!(
                try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(
                    q_lf.as_bytes(),
                    &mut out
                )
                .is_some()
            );

            // 2. Quoted field followed by CRLF
            let q_crlf = "\"val\"\r\n".to_string();
            out.clear();
            assert!(
                try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(
                    q_crlf.as_bytes(),
                    &mut out
                )
                .is_some()
            );

            // 3. Quoted field followed by delimiter and unquoted field
            let q_delim = format!("\"head\"{}{}\n", DELIM as char, "tail");
            out.clear();
            assert!(
                try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(
                    q_delim.as_bytes(),
                    &mut out
                )
                .is_some()
            );

            // 4. Unquoted field with CRLF
            let unq_crlf = "abc\r\n".to_string();
            out.clear();
            assert!(
                try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(
                    unq_crlf.as_bytes(),
                    &mut out
                )
                .is_some()
            );

            // 5. Unquoted field with LF
            let unq_lf = "abc\n".to_string();
            out.clear();
            assert!(
                try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(
                    unq_lf.as_bytes(),
                    &mut out
                )
                .is_some()
            );

            // 6. Unquoted field with delimiter
            let unq_delim = format!("abc{}def\n", DELIM as char);
            out.clear();
            assert!(
                try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(
                    unq_delim.as_bytes(),
                    &mut out
                )
                .is_some()
            );

            // 7. Stray byte after quote
            out.clear();
            assert!(
                try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(
                    b"\"a\"x\n",
                    &mut out
                )
                .is_none()
            );

            // 8. Stray \r without \n after quote
            out.clear();
            assert!(
                try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(
                    b"\"a\"\rx",
                    &mut out
                )
                .is_none()
            );

            // 9. EOF after quote
            out.clear();
            assert!(
                try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(b"\"a\"", &mut out)
                    .is_none()
            );

            // 10. Unterminated quote
            out.clear();
            assert!(
                try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(
                    b"\"unterminated",
                    &mut out
                )
                .is_none()
            );

            // 11. Quote inside unquoted field
            out.clear();
            assert!(
                try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(
                    b"a\"b\n", &mut out
                )
                .is_none()
            );

            // 12. Empty
            out.clear();
            assert!(
                try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(b"", &mut out)
                    .is_none()
            );

            // 13. Unquoted field with no delimiter or newline
            out.clear();
            assert!(
                try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(
                    b"no_delim",
                    &mut out
                )
                .is_none()
            );

            // 14. Max fields exceeded for quoted and unquoted fields
            let mut max_fields_unq = vec![DELIM; Limits::DEFAULT.max_fields];
            max_fields_unq.extend_from_slice(b"z\n");
            out.clear();
            assert!(
                try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(
                    &max_fields_unq,
                    &mut out
                )
                .is_none()
            );

            let mut max_fields_q = vec![DELIM; Limits::DEFAULT.max_fields];
            max_fields_q.extend_from_slice(b"\"z\"\n");
            out.clear();
            assert!(
                try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(
                    &max_fields_q,
                    &mut out
                )
                .is_none()
            );

            // 15. Backslash specific tests if BACKSLASH is true
            if BACKSLASH {
                out.clear();
                assert!(
                    try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(
                        b"\"a\\\\b\"\n",
                        &mut out
                    )
                    .is_some()
                );
                out.clear();
                assert!(
                    try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(
                        b"\"a\\xb\"\n",
                        &mut out
                    )
                    .is_none()
                );
                out.clear();
                assert!(
                    try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(
                        b"\"a\\", &mut out
                    )
                    .is_none()
                );
            }

            // 16. Limits
            if CHECK_LIMIT {
                let max_field = Limits::DEFAULT.max_field_bytes;
                let mut long_q = vec![b'"'];
                long_q.extend(core::iter::repeat_n(b'x', max_field + 1));
                long_q.push(b'"');
                out.clear();
                assert!(
                    try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(
                        &long_q, &mut out
                    )
                    .is_none()
                );

                let mut long_unq = vec![b'x'; max_field + 1];
                long_unq.push(DELIM);
                long_unq.push(b'z');
                out.clear();
                assert!(
                    try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(
                        &long_unq, &mut out
                    )
                    .is_none()
                );

                // Escape or doubled quote pushing field over limit
                let mut esc_over = vec![b'"'];
                esc_over.extend(core::iter::repeat_n(b'x', max_field - 1));
                if BACKSLASH {
                    esc_over.extend_from_slice(b"\\\"\"\n");
                } else {
                    esc_over.extend_from_slice(b"\"\"\"\n");
                }
                out.clear();
                assert!(
                    try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(
                        &esc_over, &mut out
                    )
                    .is_none()
                );

                // Segment before escape exceeding remaining limit
                let mut seg_over = vec![b'"'];
                if BACKSLASH {
                    seg_over.extend_from_slice(b"\\\\");
                } else {
                    seg_over.extend_from_slice(b"\"\"");
                }
                seg_over.extend(core::iter::repeat_n(b'x', max_field));
                seg_over.push(b'"');
                out.clear();
                assert!(
                    try_parse_named_dialect_record::<DELIM, BACKSLASH, CHECK_LIMIT>(
                        &seg_over, &mut out
                    )
                    .is_none()
                );
            }
        }

        exercise_dialect_parser::<b'\t', false, true>();
        exercise_dialect_parser::<b'\t', false, false>();
        exercise_dialect_parser::<b';', false, true>();
        exercise_dialect_parser::<b';', false, false>();
        exercise_dialect_parser::<b'|', false, true>();
        exercise_dialect_parser::<b'|', false, false>();
        exercise_dialect_parser::<b',', true, true>();
        exercise_dialect_parser::<b',', true, false>();
        exercise_dialect_parser::<b'\t', true, true>();
        exercise_dialect_parser::<b'\t', true, false>();

        #[cfg(feature = "benchmarking")]
        {
            let count = count_structurals_scalar(b"a,b\nc,\"d\"\n", b',', b'"', b'\n');
            assert_eq!(count, 6);
            let count_sel = count_structurals_selected(b"a,b\nc,\"d\"\n", b',', b'"', b'\n');
            assert_eq!(count_sel, 6);
        }

        // try_parse_default_quoted_prefix and try_parse_default_record_prefix permutations
        let mut out_qp = ByteRecord::new();
        let res_qp = try_parse_default_quoted_prefix::<true>(b"\"quoted\",plain\n", &mut out_qp);
        assert_eq!(res_qp, Some((9, false)));
        let res_qp_false = try_parse_default_quoted_prefix::<false>(b"\"quoted\"\n", &mut out_qp);
        assert_eq!(res_qp_false, Some((9, true)));

        let mut out_rp = ByteRecord::new();
        let res_rp = try_parse_default_record_prefix::<true>(b"a,b\n", &mut out_rp);
        assert_eq!(res_rp, Some((4, true)));
        let res_rp_false = try_parse_default_record_prefix::<false>(b"a,b\n", &mut out_rp);
        assert_eq!(res_rp_false, Some((4, true)));
        assert!(try_parse_default_record_prefix::<true>(b"\"unterminated", &mut out_rp).is_none());
        assert!(try_parse_default_record_prefix::<false>(b"\"unterminated", &mut out_rp).is_none());

        let _ = try_parse_default_quoted_record_structural_available();

        let mut out_struct = ByteRecord::new();
        let res_struct_false =
            try_parse_default_quoted_record_structural::<false>(b"\"a,b\",c\n", &mut out_struct);
        assert!(res_struct_false.is_some());
        out_struct.clear();
        let res_struct_false_fb =
            try_parse_default_quoted_record_structural::<false>(b"\"unterminated", &mut out_struct);
        assert!(res_struct_false_fb.is_none());
        out_struct.clear();
        let res_struct_true_fb =
            try_parse_default_quoted_record_structural::<true>(b"\"unterminated", &mut out_struct);
        assert!(res_struct_true_fb.is_none());
    }
}
