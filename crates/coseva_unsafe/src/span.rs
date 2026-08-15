//! Byte spans into parser-owned storage.

use alloc::vec::Vec;
use core::marker::PhantomData;
use core::mem;
use core::num::NonZeroUsize;
use core::ops::Range;
use core::slice;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    Input,
    Scratch,
}

/// Location of one field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Span {
    start: usize,
    end: usize,
}

impl Span {
    /// High bit of `start`: input-vs-scratch source.
    const FLAG: usize = 1 << (usize::BITS - 1);
    /// Second-highest bit of `start`: explicit NULL marker.
    ///
    /// NULL fields always carry a zero-length range (`start == end`), so this
    /// bit never needs to be preserved across offset arithmetic; it is only
    /// ever set at construction time and cleared implicitly whenever a span
    /// is rebuilt from a non-NULL range.
    const NULL_FLAG: usize = 1 << (usize::BITS - 2);
    /// Largest offset representable without colliding with `FLAG` or
    /// `NULL_FLAG`.
    ///
    /// On 64-bit targets this is beyond any possible allocation, but on 32-bit
    /// targets it is `1 GiB - 1`, which a real document can exceed. Callers
    /// that build spans through the unchecked constructors must reject buffers
    /// longer than this before parsing.
    pub const MAX_OFFSET: usize = Self::NULL_FLAG - 1;

    const fn offset_fits(offset: usize) -> bool {
        offset <= Self::MAX_OFFSET
    }

    const fn bounded_offset(offset: usize) -> usize {
        if Self::offset_fits(offset) {
            offset
        } else {
            Self::MAX_OFFSET
        }
    }

    pub fn new(source: Source, range: Range<usize>, quoted: bool) -> Option<Self> {
        if range.start > range.end || range.start > Self::MAX_OFFSET || range.end > Self::MAX_OFFSET
        {
            return None;
        }
        Some(Self::from_valid_range(source, range, quoted))
    }

    /// # Panics
    ///
    /// Panics when the range is inverted or an endpoint cannot be packed.
    pub fn from_valid_range(source: Source, range: Range<usize>, quoted: bool) -> Self {
        assert!(
            range.start <= range.end && range.end <= Self::MAX_OFFSET,
            "span endpoint exceeds the packed offset width, so it would be \
             truncated and later resolve to the wrong bytes"
        );
        // SAFETY: the assertion establishes every packing precondition.
        unsafe { Self::from_range_unchecked(source, range, quoted) }
    }

    pub(crate) unsafe fn from_range_unchecked(
        source: Source,
        range: Range<usize>,
        quoted: bool,
    ) -> Self {
        let start = match source {
            Source::Input => range.start,
            Source::Scratch => range.start | Self::FLAG,
        };
        let end = if quoted {
            range.end | Self::FLAG
        } else {
            range.end
        };
        Self { start, end }
    }

    /// Construct a zero-length span for an explicit NULL field.
    ///
    /// NULL fields carry no bytes; `offset` is simply the cursor location at
    /// which the (empty) field would have started.
    ///
    /// # Panics
    ///
    /// Panics when `offset` cannot be packed.
    pub fn from_valid_null(source: Source, offset: usize) -> Self {
        assert!(
            offset <= Self::MAX_OFFSET,
            "NULL span offset exceeds the packed offset width"
        );
        let start = match source {
            Source::Input => offset,
            Source::Scratch => offset | Self::FLAG,
        } | Self::NULL_FLAG;
        Self { start, end: offset }
    }

    pub const fn source(self) -> Source {
        if self.start & Self::FLAG == 0 {
            Source::Input
        } else {
            Source::Scratch
        }
    }

    const fn start(self) -> usize {
        self.start & !(Self::FLAG | Self::NULL_FLAG)
    }

    const fn end(self) -> usize {
        self.end & !Self::FLAG
    }

    /// The offset at which this field's bytes begin, within whichever buffer
    /// [`Self::source`] names.
    pub const fn start_offset(self) -> usize {
        self.start()
    }

    /// This field's byte range within whichever buffer [`Self::source`] names.
    pub const fn range(self) -> Range<usize> {
        self.start()..self.end()
    }

    pub const fn is_quoted(self) -> bool {
        self.end & Self::FLAG != 0
    }

    /// Whether this field is an explicit NULL rather than merely empty.
    pub const fn is_null(self) -> bool {
        self.start & Self::NULL_FLAG != 0
    }

    pub fn trim_ascii(&mut self, input: &[u8], scratch: &[u8]) {
        if self.is_null() {
            // NULL fields are always zero-length; there is nothing to trim.
            return;
        }
        let bytes = match self.source() {
            Source::Input => input,
            Source::Scratch => scratch,
        };
        let mut start = self.start();
        let mut end = self.end();
        // gamma::skip(cond.negate, reason = "mutation causes non-termination or unbounded resource use")
        while start < end && bytes[start].is_ascii_whitespace() {
            // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
            start += 1;
        }
        while end > start && bytes[end - 1].is_ascii_whitespace() {
            // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
            end -= 1;
        }
        let source = self.start & Self::FLAG;
        let quoted = self.end & Self::FLAG;
        self.start = start | source;
        self.end = end | quoted;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpanSet {
    spans: Vec<Span>,
    input_end: usize,
    scratch_end: usize,
}

impl SpanSet {
    pub fn new() -> Self {
        Self {
            spans: Vec::new(),
            input_end: usize::MIN,
            scratch_end: usize::MIN,
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            spans: Vec::with_capacity(capacity),
            input_end: usize::MIN,
            scratch_end: usize::MIN,
        }
    }

    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.spans.capacity()
    }

    pub fn reserve(&mut self, additional: usize) {
        self.spans.reserve(additional);
    }

    pub fn shrink_to(&mut self, min_capacity: usize) {
        self.spans.shrink_to(min_capacity);
    }

    pub fn clear(&mut self) {
        self.spans.clear();
        self.rebuild_bounds();
    }

    pub fn truncate(&mut self, len: usize) {
        self.spans.truncate(len);
        self.rebuild_bounds();
    }

    #[inline(always)]
    pub fn push(&mut self, source: Source, range: Range<usize>, quoted: bool) {
        let span = Span::from_valid_range(source, range, quoted);
        self.include(span);
        self.spans.push(span);
    }

    #[inline(always)]
    pub fn try_push_bounded(
        &mut self,
        source: Source,
        range: Range<usize>,
        quoted: bool,
        max_fields: usize,
        max_field_bytes: usize,
    ) -> bool {
        let Some(field_len) = range.end.checked_sub(range.start) else {
            return false;
        };
        if self.len() >= max_fields || field_len > max_field_bytes || range.end > Span::MAX_OFFSET {
            return false;
        }
        // SAFETY: the checks above establish the packed-range preconditions.
        let span = unsafe { Span::from_range_unchecked(source, range, quoted) };
        self.include(span);
        self.spans.push(span);
        true
    }

    #[inline(always)]
    pub fn push_null(&mut self, source: Source, offset: usize) {
        let span = Span::from_valid_null(source, offset);
        self.include(span);
        self.spans.push(span);
    }

    pub fn get(&self, index: usize) -> Option<&Span> {
        self.spans.get(index)
    }

    pub fn iter(&self) -> slice::Iter<'_, Span> {
        self.spans.iter()
    }

    /// # Panics
    ///
    /// Panics when the recorded bounds do not fit the supplied buffers.
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "record construction must keep the once-per-view bounds proof out of field access"
    )]
    pub fn resolved<'record>(
        &'record self,
        input: &'record [u8],
        scratch: &'record [u8],
    ) -> ResolvedSpans<'record> {
        assert!(
            self.input_end() <= input.len(),
            "span set does not fit the input buffer it is being resolved against"
        );
        assert!(
            self.scratch_end() == 0 || self.scratch_end() <= scratch.len(),
            "span set does not fit the scratch buffer it is being resolved against"
        );
        ResolvedSpans {
            input,
            scratch,
            spans: &self.spans,
        }
    }

    /// # Panics
    ///
    /// Panics when the recorded bounds do not fit the supplied buffers.
    pub fn trim_ascii_where(
        &mut self,
        input: &[u8],
        scratch: &[u8],
        mut should_trim: impl FnMut(bool) -> bool,
    ) {
        assert!(self.input_end() <= input.len() && self.scratch_end() <= scratch.len());
        for span in &mut self.spans {
            if should_trim(span.is_quoted()) {
                span.trim_ascii(input, scratch);
            }
        }
        self.rebuild_bounds();
    }

    /// # Panics
    ///
    /// Panics when the recorded input bound does not fit `input`.
    pub fn mark_input_nulls(&mut self, input: &[u8], mut is_null: impl FnMut(&[u8]) -> bool) {
        assert!(self.input_end() <= input.len());
        for span in &mut self.spans {
            if !span.is_quoted() && span.source() == Source::Input {
                let range = span.range();
                if is_null(&input[range]) {
                    *span = Span::from_valid_null(Source::Input, span.start_offset());
                }
            }
        }
    }

    #[inline(always)]
    pub(crate) fn include(&mut self, span: Span) {
        let end = span.range().end;
        match span.source() {
            Source::Input => self.input_end = self.input_end.max(end),
            Source::Scratch => self.scratch_end = self.scratch_end.max(end),
        }
    }

    pub(crate) fn rebuild_bounds(&mut self) {
        self.input_end = self
            .spans
            .iter()
            .filter(|span| span.source() == Source::Input)
            .map(|span| span.range().end)
            .max()
            .unwrap_or_default();
        self.scratch_end = self
            .spans
            .iter()
            .filter(|span| span.source() == Source::Scratch)
            .map(|span| span.range().end)
            .max()
            .unwrap_or_default();
    }

    #[inline(always)]
    fn input_end(&self) -> usize {
        self.input_end
    }

    #[inline(always)]
    fn scratch_end(&self) -> usize {
        self.scratch_end
    }
}

impl Default for SpanSet {
    fn default() -> Self {
        Self::new()
    }
}

impl<'set> IntoIterator for &'set SpanSet {
    type Item = &'set Span;
    type IntoIter = slice::Iter<'set, Span>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<const N: usize> From<[Span; N]> for SpanSet {
    fn from(spans: [Span; N]) -> Self {
        let mut set = Self::with_capacity(N);
        for span in spans {
            set.include(span);
            set.spans.push(span);
        }
        set
    }
}

impl From<Vec<Span>> for SpanSet {
    fn from(spans: Vec<Span>) -> Self {
        let mut set = Self::with_capacity(spans.len());
        for span in spans {
            set.include(span);
            set.spans.push(span);
        }
        set
    }
}

/// Reusable storage that owns the scratch buffer its spans may reference.
#[derive(Debug)]
pub struct SpanStorage {
    spans: Vec<Span>,
    scratch: Vec<u8>,
    input_len: NonZeroUsize,
}

impl SpanStorage {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            spans: Vec::with_capacity(capacity),
            scratch: Vec::new(),
            input_len: NonZeroUsize::MIN,
        }
    }

    pub fn begin(&mut self, input: &[u8], max_offset: usize) -> bool {
        if input.len() > Span::bounded_offset(max_offset) {
            return false;
        }
        self.spans.clear();
        self.scratch.clear();
        self.input_len = NonZeroUsize::new(
            input
                .len()
                .checked_add(1)
                .expect("a slice length leaves room for the bound sentinel"),
        )
        .expect("adding one makes the encoded input length nonzero");
        true
    }

    fn input_len(&self) -> usize {
        self.input_len.get() - 1
    }

    #[expect(
        clippy::len_without_is_empty,
        reason = "the private engine only needs the field count"
    )]
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn capacity(&self) -> usize {
        self.spans.capacity()
    }

    pub fn scratch_capacity(&self) -> usize {
        self.scratch.capacity()
    }

    pub fn scratch_len(&self) -> usize {
        self.scratch.len()
    }

    pub fn reserve(&mut self, additional: usize) {
        self.spans.reserve(additional);
    }

    pub fn shrink_to(&mut self, min_capacity: usize) {
        self.spans.shrink_to(min_capacity);
    }

    pub fn shrink_scratch_to(&mut self, min_capacity: usize) {
        self.scratch.shrink_to(min_capacity);
    }

    pub fn clear(&mut self) {
        self.spans.clear();
        self.scratch.clear();
        self.input_len = NonZeroUsize::MIN;
    }

    #[expect(
        clippy::iter_without_into_iter,
        reason = "iteration is an internal parser operation, not a collection API"
    )]
    pub fn iter(&self) -> slice::Iter<'_, Span> {
        self.spans.iter()
    }

    pub fn scratch_push(&mut self, byte: u8) {
        self.scratch.push(byte);
    }

    pub fn scratch_extend_from_slice(&mut self, bytes: &[u8]) {
        self.scratch.extend_from_slice(bytes);
    }

    #[cfg(target_arch = "x86_64")]
    pub(crate) fn truncate(&mut self, len: usize) {
        self.spans.truncate(len);
    }

    pub fn clear_spans(&mut self) {
        self.spans.clear();
    }

    #[inline(always)]
    #[expect(
        clippy::suspicious_operation_groupings,
        reason = "these are independent count, field-width, and source-bound checks"
    )]
    pub fn try_push_input_bounded(
        &mut self,
        range: Range<usize>,
        quoted: bool,
        max_fields: usize,
        max_field_bytes: usize,
    ) -> bool {
        let Some(field_len) = range.end.checked_sub(range.start) else {
            return false;
        };
        if self.len() >= max_fields || field_len > max_field_bytes || range.end > self.input_len() {
            return false;
        }
        // SAFETY: `begin` proved the whole input fits the packed offset, and
        // the checks above establish an ordered range within that input.
        self.spans
            .push(unsafe { Span::from_range_unchecked(Source::Input, range, quoted) });
        true
    }

    #[inline(always)]
    pub fn try_push_scratch_bounded(
        &mut self,
        range: Range<usize>,
        quoted: bool,
        max_fields: usize,
        max_field_bytes: usize,
    ) -> bool {
        let Some(field_len) = range.end.checked_sub(range.start) else {
            return false;
        };
        if self.len() >= max_fields
            || field_len > max_field_bytes
            || range.end > self.scratch.len()
            || range.end > self.input_len()
        {
            return false;
        }
        // SAFETY: the checks above establish an ordered, packable range in
        // scratch, which this storage owns for as long as the span survives.
        self.spans
            .push(unsafe { Span::from_range_unchecked(Source::Scratch, range, quoted) });
        true
    }

    pub fn push_null(&mut self, offset: usize) {
        assert!(offset <= self.input_len());
        self.spans
            .push(Span::from_valid_null(Source::Input, offset));
    }

    pub fn trim_ascii_where(&mut self, input: &[u8], mut should_trim: impl FnMut(bool) -> bool) {
        assert!(self.input_len() <= input.len());
        for span in &mut self.spans {
            if should_trim(span.is_quoted()) {
                span.trim_ascii(input, &self.scratch);
            }
        }
    }

    pub fn mark_input_nulls(&mut self, input: &[u8], mut is_null: impl FnMut(&[u8]) -> bool) {
        assert!(self.input_len() <= input.len());
        for span in &mut self.spans {
            if !span.is_quoted() && span.source() == Source::Input {
                let range = span.range();
                if is_null(&input[range]) {
                    *span = Span::from_valid_null(Source::Input, span.start_offset());
                }
            }
        }
    }

    #[cfg(target_arch = "x86_64")]
    pub(crate) fn spans_mut(&mut self) -> &mut Vec<Span> {
        &mut self.spans
    }

    #[cfg(target_arch = "x86_64")]
    pub(crate) fn raw_len(&self) -> usize {
        self.spans.len()
    }

    pub(crate) fn parts_mut(&mut self) -> (&mut Vec<Span>, &mut Vec<u8>) {
        (&mut self.spans, &mut self.scratch)
    }

    pub(crate) fn accepts_input(&self, input: &[u8]) -> bool {
        input.len() <= self.input_len()
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "cursor filtering resolves one field in its per-record hot loop"
    )]
    pub fn get<'record>(
        &'record self,
        input: &'record [u8],
        index: usize,
    ) -> Option<&'record [u8]> {
        let span = *self.spans.get(index)?;
        if span.source() == Source::Scratch {
            return Some(resolve_span(input, &self.scratch, span));
        }
        if span.end() > input.len() {
            return None;
        }
        let range = span.start()..span.end();
        // SAFETY: `SpanStorage` only contains ordered ranges, and the check
        // above establishes the external input's remaining bound.
        Some(unsafe { input.get_unchecked(range) })
    }

    /// Resolve a previously completed record against another view of its
    /// original input extent.
    ///
    /// # Panics
    ///
    /// Panics when `input` is shorter than the input bound established when
    /// the record was parsed.
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "reusing a materialized cursor record must keep its single input proof in the caller"
    )]
    pub fn resolved<'record>(&'record self, input: &'record [u8]) -> ResolvedSpans<'record> {
        assert!(
            self.input_len() <= input.len(),
            "span storage does not fit the input buffer it is being resolved against"
        );
        ResolvedSpans {
            input,
            scratch: &self.scratch,
            spans: &self.spans,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ResolvedSpans<'record> {
    input: &'record [u8],
    scratch: &'record [u8],
    spans: &'record [Span],
}

impl<'record> ResolvedSpans<'record> {
    pub const fn len(&self) -> usize {
        self.spans.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "field access must retain the pre-extraction zero-cost span resolution"
    )]
    pub fn get(&self, index: usize) -> Option<&'record [u8]> {
        let span = *self.spans.get(index)?;
        Some(resolve_span(self.input, self.scratch, span))
    }

    pub const fn fields(self) -> ResolvedSpanIter<'record> {
        let current = self.spans.as_ptr();
        ResolvedSpanIter {
            input: self.input,
            scratch: self.scratch,
            current,
            end: current.wrapping_add(self.spans.len()),
            marker: PhantomData,
        }
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "owned-record materialization resolves every field in its hot loop"
    )]
    pub fn get_entry(&self, index: usize) -> Option<(&'record [u8], bool)> {
        let span = *self.spans.get(index)?;
        Some((resolve_span(self.input, self.scratch, span), span.is_null()))
    }

    pub fn is_null(&self, index: usize) -> Option<bool> {
        self.spans.get(index).map(|span| span.is_null())
    }

    #[inline(always)]
    pub fn field_is_null(&self, index: usize) -> bool {
        self.spans.get(index).is_some_and(|span| span.is_null())
    }

    pub fn source(&self, index: usize) -> Option<Source> {
        self.spans.get(index).map(|span| span.source())
    }

    pub fn span_iter(&self) -> slice::Iter<'record, Span> {
        self.spans.iter()
    }

    pub fn nulls(&self) -> impl ExactSizeIterator<Item = bool> + Clone + 'record {
        self.spans.iter().map(|span| span.is_null())
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedSpanIter<'record> {
    input: &'record [u8],
    scratch: &'record [u8],
    current: *const Span,
    end: *const Span,
    marker: PhantomData<&'record Span>,
}

impl<'record> Iterator for ResolvedSpanIter<'record> {
    type Item = &'record [u8];

    // gamma::skip(fn_value.some, reason = "mutation causes non-termination or unbounded resource use")
    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        if self.current == self.end {
            // gamma::skip(option.none_to_some, reason = "mutation causes non-termination or unbounded resource use")
            return None;
        }
        // SAFETY: `current..end` is the original validated span slice, and
        // each successful call advances `current` by exactly one element.
        let span = unsafe {
            let span = *self.current;
            // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
            self.current = self.current.add(1);
            span
        };
        Some(resolve_span(self.input, self.scratch, span))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // gamma::skip(arith.sub_to_add, reason = "mutation causes non-termination or unbounded resource use")
        let remaining = (self.end as usize - self.current as usize) / mem::size_of::<Span>();
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for ResolvedSpanIter<'_> {}

// SAFETY: the pointers represent a shared `&[Span]` for `'record`; moving the
// iterator between threads is as safe as moving that shared slice.
unsafe impl Send for ResolvedSpanIter<'_> {}
// SAFETY: shared access to the iterator never advances its private pointer,
// and every referenced component is immutable.
unsafe impl Sync for ResolvedSpanIter<'_> {}

#[inline(always)]
#[expect(
    clippy::inline_always,
    reason = "validated record views must retain zero-cost field resolution"
)]
fn resolve_span<'record>(
    input: &'record [u8],
    scratch: &'record [u8],
    span: Span,
) -> &'record [u8] {
    let source = match span.source() {
        Source::Input => input,
        Source::Scratch => scratch,
    };
    let start = span.start();
    let end = span.end();
    // SAFETY: the owning span collection checked each source bound while
    // constructing the span and revalidated the external input once when
    // constructing this view.
    unsafe { source.get_unchecked(start..end) }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::{Source, Span, SpanSet, SpanStorage};

    #[test]
    fn span_flags_do_not_consume_offset_bits() {
        // The top two bits of `Span::start` are reserved for the source and
        // NULL flags; every value up to `MAX_OFFSET` must round-trip
        // exactly through a real `start`/`end` pair.
        let start = Span::MAX_OFFSET - 1;
        let span = Span::new(Source::Scratch, start..start + 1, true)
            .expect("valid slice offsets fit below the two reserved top bits");
        assert_eq!(span.source(), Source::Scratch);
        assert_eq!(span.start()..span.end(), start..start + 1);
        assert!(span.is_quoted());
        assert!(!span.is_null());
    }

    #[test]
    fn span_rejects_offsets_that_collide_with_flags() {
        assert!(Span::new(Source::Input, Span::MAX_OFFSET + 1..usize::MAX, false).is_none());
    }

    #[test]
    fn from_valid_range_round_trips_at_the_max_offset_boundary() {
        // `push_span` builds spans through `from_valid_range`, never `new`, so
        // the packing must be pinned directly against the constructor the
        // parser actually calls. `MAX_OFFSET` is the largest offset that does
        // not alias `FLAG` or `NULL_FLAG`; anything wrong in the packing
        // arithmetic would corrupt exactly this boundary value.
        let mut spans = SpanSet::new();
        spans.push(
            Source::Scratch,
            Span::MAX_OFFSET - 1..Span::MAX_OFFSET,
            true,
        );
        let span = *spans.get(0).expect("one span");
        assert_eq!(span.source(), Source::Scratch);
        assert_eq!(
            span.start()..span.end(),
            Span::MAX_OFFSET - 1..Span::MAX_OFFSET
        );
        assert!(span.is_quoted());
        assert!(!span.is_null());
    }

    #[test]
    fn storage_binds_spans_to_checked_input_and_owned_scratch() {
        let input = b"abc";
        let mut storage = SpanStorage::with_capacity(2);
        assert!(storage.begin(input, input.len()));
        assert!(storage.try_push_input_bounded(0..2, false, 2, 2));
        storage.scratch_extend_from_slice(b"c");
        assert!(storage.try_push_scratch_bounded(0..1, true, 2, 2));

        let resolved = storage.resolved(input);
        assert_eq!(resolved.get(0), Some(b"ab".as_slice()));
        assert_eq!(resolved.get(1), Some(b"c".as_slice()));
        assert_eq!(storage.get(b"a", 0), None);
        assert_eq!(storage.get(b"", 1), Some(b"c".as_slice()));
        assert_eq!(storage.get(b"abc", 999), None);
    }

    #[test]
    fn storage_rejects_ranges_outside_the_bound_record() {
        let mut storage = SpanStorage::with_capacity(1);
        assert!(storage.begin(b"abc", 3));
        assert!(!storage.try_push_input_bounded(2..4, false, 1, 4));
        storage.scratch_extend_from_slice(b"abcd");
        assert!(!storage.try_push_scratch_bounded(0..4, false, 1, 4));
    }

    #[test]
    fn storage_respects_a_shrunken_packed_offset_limit() {
        let mut storage = SpanStorage::with_capacity(1);
        assert!(!storage.begin(b"abc", 2));
        assert!(storage.begin(b"ab", 2));
    }

    #[test]
    #[should_panic(expected = "span storage does not fit the input buffer")]
    fn storage_refuses_to_resolve_against_a_shorter_input() {
        let mut storage = SpanStorage::with_capacity(1);
        assert!(storage.begin(b"abc", 3));
        assert!(storage.try_push_input_bounded(0..3, false, 1, 3));
        let _ = storage.resolved(b"ab");
    }

    #[test]
    #[expect(
        clippy::reversed_empty_ranges,
        reason = "deliberately testing inverted range rejection"
    )]
    fn span_set_comprehensive_methods() {
        let mut set = SpanSet::default();
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);
        set.reserve(10);
        assert!(set.capacity() >= 10);
        set.shrink_to(2);

        set.push(Source::Input, 0..3, false);
        set.push_null(Source::Scratch, 3);
        assert_eq!(set.len(), 2);
        assert!(!set.is_empty());

        assert!(!set.try_push_bounded(Source::Input, 5..2, false, 10, 10)); // inverted
        assert!(!set.try_push_bounded(Source::Input, 0..3, false, 2, 10)); // max_fields
        assert!(!set.try_push_bounded(Source::Input, 0..20, false, 10, 5)); // max_field_bytes
        assert!(!set.try_push_bounded(
            Source::Input,
            0..Span::MAX_OFFSET + 1,
            false,
            10,
            usize::MAX
        )); // max offset
        assert!(set.try_push_bounded(Source::Input, 3..6, true, 10, 10));

        let mut count = 0;
        for s in &set {
            count += 1;
            assert!(s.end() >= s.start());
        }
        assert_eq!(count, 3);

        let input = b"abc   def";
        let scratch = b"123456";
        let _ = set.resolved(input, scratch);
        set.push(Source::Input, 6..9, false);
        set.push(Source::Scratch, 0..3, false);
        set.trim_ascii_where(input, scratch, |_| true);
        set.trim_ascii_where(input, scratch, |quoted| !quoted);
        set.mark_input_nulls(input, |b| b == b"abc");

        let resolved = set.resolved(input, scratch);
        assert_eq!(resolved.len(), 5);
        assert!(!resolved.is_empty());
        assert_eq!(resolved.get(0), Some(b"".as_slice()));
        assert_eq!(resolved.is_null(0), Some(true));
        assert!(resolved.field_is_null(0));
        assert_eq!(resolved.source(0), Some(Source::Input));
        assert_eq!(resolved.get_entry(0), Some((b"".as_slice(), true)));

        // Out of bounds access on resolved spans
        assert_eq!(resolved.get(999), None);
        assert_eq!(resolved.get_entry(999), None);
        assert_eq!(resolved.is_null(999), None);
        assert_eq!(resolved.source(999), None);

        let mut fields_iter = resolved.fields();
        let (hint_min, hint_max) = fields_iter.size_hint();
        assert_eq!(hint_min, 5);
        assert_eq!(hint_max, Some(5));
        assert!(fields_iter.next().is_some());

        assert_eq!(resolved.span_iter().count(), 5);
        assert_eq!(resolved.nulls().count(), 5);

        set.truncate(1);
        assert_eq!(set.len(), 1);
        set.clear();
        assert_eq!(set.len(), 0);

        let arr = [Span::from_valid_range(Source::Input, 0..1, false)];
        let from_arr = SpanSet::from(arr);
        assert_eq!(from_arr.len(), 1);

        let v = alloc::vec![Span::from_valid_range(Source::Input, 0..1, false)];
        let from_vec = SpanSet::from(v);
        assert_eq!(from_vec.len(), 1);
    }

    #[test]
    #[expect(
        clippy::reversed_empty_ranges,
        reason = "deliberately testing inverted range rejection"
    )]
    fn span_storage_comprehensive_methods() {
        let mut storage = SpanStorage::with_capacity(4);
        assert_eq!(storage.len(), 0);
        assert!(storage.capacity() >= 4);
        storage.reserve(10);
        storage.shrink_to(2);
        storage.shrink_scratch_to(0);
        assert_eq!(storage.scratch_capacity(), 0);
        assert_eq!(storage.scratch_len(), 0);

        storage.scratch_push(b'x');
        assert_eq!(storage.scratch_len(), 1);
        assert!(!storage.try_push_scratch_bounded(0..1, false, 0, 10)); // max_fields reached

        let input = b"  hello  world  ";
        assert!(storage.begin(input, input.len()));
        assert!(!storage.try_push_input_bounded(5..2, false, 10, 10)); // inverted
        assert!(storage.try_push_input_bounded(0..9, false, 5, 20));
        assert!(storage.try_push_input_bounded(9..16, true, 5, 20));
        storage.push_null(16);

        assert!(!storage.try_push_scratch_bounded(5..2, false, 10, 10)); // inverted
        assert!(!storage.try_push_scratch_bounded(0..1, false, 3, 10)); // max_fields
        assert!(!storage.try_push_scratch_bounded(0..10, false, 10, 5)); // max_field_bytes
        assert!(!storage.try_push_scratch_bounded(0..10, false, 10, 20)); // scratch len exceeded
        assert!(!storage.try_push_scratch_bounded(0..1, false, 10, 20)); // input len check

        storage.scratch_extend_from_slice(b"world");
        assert!(storage.try_push_scratch_bounded(0..5, true, 10, 20));
        assert!(storage.try_push_scratch_bounded(0..5, false, 10, 20));

        storage.trim_ascii_where(input, |quoted| !quoted);
        storage.trim_ascii_where(input, |_| true);
        storage.mark_input_nulls(input, |b| b == b"hello");

        assert_eq!(storage.iter().count(), 5);
        let mut iter = storage.resolved(input).fields();
        while iter.next().is_some() {}
        assert!(iter.next().is_none());

        assert_eq!(storage.get(input, 0), Some(b"".as_slice()));
        assert_eq!(storage.get(input, 1), Some(b"world".as_slice()));
        assert_eq!(storage.get(b"short", 1), None);

        storage.clear_spans();
        assert_eq!(storage.len(), 0);

        storage.clear();
        assert_eq!(storage.len(), 0);
        assert_eq!(storage.scratch_len(), 0);
    }

    #[test]
    fn span_constructor_boundaries_are_exact() {
        assert!(Span::offset_fits(Span::MAX_OFFSET));
        assert!(!Span::offset_fits(Span::MAX_OFFSET + 1));
        assert_eq!(Span::bounded_offset(3), 3);
        assert_eq!(Span::bounded_offset(Span::MAX_OFFSET), Span::MAX_OFFSET);
        assert_eq!(Span::bounded_offset(Span::MAX_OFFSET + 1), Span::MAX_OFFSET);
        assert_eq!(
            Span::new(Source::Input, Span::MAX_OFFSET..Span::MAX_OFFSET, false)
                .expect("the maximum packed offset is valid")
                .range(),
            Span::MAX_OFFSET..Span::MAX_OFFSET
        );
        assert!(Span::new(Source::Input, 2..1, false).is_none());
        assert!(
            Span::new(
                Source::Input,
                Span::MAX_OFFSET + 1..Span::MAX_OFFSET + 1,
                false
            )
            .is_none()
        );
        assert!(Span::new(Source::Input, 0..Span::MAX_OFFSET + 1, false).is_none());

        let mut null = Span::from_valid_null(Source::Scratch, Span::MAX_OFFSET);
        null.trim_ascii(&[], &[]);
        assert_eq!(null.source(), Source::Scratch);
        assert_eq!(null.range(), Span::MAX_OFFSET..Span::MAX_OFFSET);
        assert!(null.is_null());

        assert!(
            std::panic::catch_unwind(|| { Span::from_valid_range(Source::Input, 2..1, false) })
                .is_err()
        );
        assert!(
            std::panic::catch_unwind(|| {
                Span::from_valid_range(Source::Input, Span::MAX_OFFSET..Span::MAX_OFFSET + 1, false)
            })
            .is_err()
        );
    }

    #[test]
    fn span_set_bounds_follow_every_mutation() {
        let mut set = SpanSet::with_capacity(4);
        assert_eq!(set.capacity(), 4);
        set.reserve(128);
        let reserved_capacity = set.capacity();
        set.shrink_to(4);
        assert!(set.capacity() < reserved_capacity);
        assert!(set.capacity() >= 4);
        set.push(Source::Input, 5..9, false);
        set.push(Source::Scratch, 2..7, true);
        set.push_null(Source::Scratch, 10);
        assert_eq!((set.input_end(), set.scratch_end()), (9, 10));

        set.truncate(2);
        assert_eq!((set.input_end(), set.scratch_end()), (9, 7));
        set.truncate(1);
        assert_eq!((set.input_end(), set.scratch_end()), (9, 0));

        set.push(Source::Input, 0..12, false);
        set.push(Source::Scratch, 0..11, false);
        set.truncate(1);
        assert_eq!((set.input_end(), set.scratch_end()), (9, 0));

        set.push(Source::Scratch, 0..6, false);
        set.clear();
        assert_eq!((set.input_end(), set.scratch_end()), (0, 0));

        let from_array = SpanSet::from([
            Span::from_valid_range(Source::Scratch, 3..8, false),
            Span::from_valid_range(Source::Input, 1..4, false),
        ]);
        assert_eq!((from_array.input_end(), from_array.scratch_end()), (4, 8));

        let from_vec = SpanSet::from(alloc::vec![
            Span::from_valid_range(Source::Input, 0..2, false),
            Span::from_valid_null(Source::Scratch, 6),
        ]);
        assert_eq!((from_vec.input_end(), from_vec.scratch_end()), (2, 6));
    }

    #[test]
    fn span_set_bounded_push_accepts_exact_limits_and_rejects_each_violation() {
        let mut set = SpanSet::new();
        assert!(set.try_push_bounded(Source::Input, 0..3, false, 1, 3));
        assert_eq!(set.get(0).expect("one field").range(), 0..3);
        assert!(!set.try_push_bounded(Source::Input, 3..3, false, 1, 0));

        set.clear();
        assert!(!set.try_push_bounded(Source::Input, 4..3, false, 2, 10));
        assert!(!set.try_push_bounded(Source::Input, 0..4, false, 2, 3));
        assert_eq!((set.input_end(), set.scratch_end()), (0, 0));
        assert!(set.try_push_bounded(
            Source::Input,
            Span::MAX_OFFSET..Span::MAX_OFFSET,
            true,
            2,
            0,
        ));
        let span = set.get(0).expect("maximum endpoint is accepted");
        assert_eq!(span.range(), Span::MAX_OFFSET..Span::MAX_OFFSET);
        assert!(span.is_quoted());
        assert_eq!(set.input_end(), Span::MAX_OFFSET);
    }

    #[test]
    fn span_set_trim_and_null_marking_preserve_sources_and_flags() {
        let input = b"nilnil  a   ";
        let scratch = b"nil";
        let mut set = SpanSet::new();
        set.push(Source::Input, 0..3, false);
        set.push(Source::Input, 3..6, true);
        set.push(Source::Scratch, 0..3, false);
        set.push(Source::Input, 6..12, false);
        set.push(Source::Input, 6..12, true);

        set.mark_input_nulls(input, |field| field == b"nil");
        let resolved = set.resolved(input, scratch);
        assert_eq!(
            resolved.nulls().collect::<alloc::vec::Vec<_>>(),
            [true, false, false, false, false]
        );
        assert_eq!(resolved.get(1), Some(b"nil".as_slice()));
        assert_eq!(resolved.get(2), Some(b"nil".as_slice()));

        set.trim_ascii_where(input, scratch, |quoted| !quoted);
        assert_eq!((set.input_end(), set.scratch_end()), (12, 3));
        let resolved = set.resolved(input, scratch);
        assert_eq!(resolved.get(3), Some(b"a".as_slice()));
        assert_eq!(resolved.get(4), Some(b"  a   ".as_slice()));
        assert_eq!(resolved.get(1), Some(b"nil".as_slice()));
        assert_eq!(resolved.is_null(1), Some(false));
        assert_eq!(resolved.is_null(0), Some(true));
        assert_eq!(resolved.field_is_null(1), false);
        assert_eq!(resolved.field_is_null(0), true);
        assert_eq!(resolved.field_is_null(999), false);

        let mut trimmed_bounds = SpanSet::new();
        trimmed_bounds.push(Source::Input, 0..4, false);
        trimmed_bounds.trim_ascii_where(b"a   ", b"", |_| true);
        assert_eq!(trimmed_bounds.input_end(), 1);
        assert_eq!(
            trimmed_bounds.resolved(b"a", b"").get(0),
            Some(b"a".as_slice())
        );
    }

    #[test]
    fn span_storage_enforces_input_scratch_and_capacity_invariants() {
        let mut storage = SpanStorage::with_capacity(3);
        assert_eq!(storage.capacity(), 3);
        assert!(storage.accepts_input(b""));
        assert!(!storage.accepts_input(b"x"));

        assert!(storage.begin(b"abcd", 4));
        assert!(!storage.try_push_input_bounded(0..4, false, 2, 3));
        assert!(storage.try_push_input_bounded(0..4, false, 1, 4));
        assert_eq!(storage.get(b"abcd", 0), Some(b"abcd".as_slice()));
        assert!(!storage.try_push_input_bounded(0..0, false, 1, 0));
        storage.scratch_extend_from_slice(b"wxyz");
        assert!(!storage.try_push_scratch_bounded(0..4, false, 1, 4));

        storage.clear_spans();
        assert!(storage.try_push_scratch_bounded(0..4, true, 1, 4));
        assert_eq!(storage.get(b"", 0), Some(b"wxyz".as_slice()));

        assert!(storage.begin(b"ab", 2));
        storage.scratch_extend_from_slice(b"xyz");
        assert!(!storage.try_push_scratch_bounded(0..2, false, 2, 1));
        assert!(!storage.try_push_scratch_bounded(0..3, false, 1, 3));
        assert!(storage.try_push_scratch_bounded(0..2, false, 1, 2));
        assert_eq!(storage.iter().next().expect("one span").range(), 0..2);

        storage.clear_spans();
        storage.push_null(2);
        assert_eq!(storage.iter().next().expect("one NULL").range(), 2..2);

        storage.reserve(100);
        let old_capacity = storage.capacity();
        storage.shrink_to(2);
        assert!(storage.capacity() < old_capacity);
        assert!(storage.capacity() >= storage.len().max(2));

        storage.scratch_extend_from_slice(&[b'x'; 100]);
        let (_, scratch) = storage.parts_mut();
        scratch.reserve(1000);
        let old_scratch_capacity = storage.scratch_capacity();
        storage.shrink_scratch_to(3);
        assert!(storage.scratch_capacity() < old_scratch_capacity);
        assert!(storage.scratch_capacity() >= storage.scratch_len());

        storage.clear();
        assert_eq!((storage.len(), storage.scratch_len()), (0, 0));
        assert!(storage.accepts_input(b""));
        assert!(!storage.accepts_input(b"x"));
    }

    #[test]
    fn span_storage_trim_and_null_predicates_select_only_intended_fields() {
        let input = b"nil  a    b  ";
        let mut storage = SpanStorage::with_capacity(4);
        assert!(storage.begin(input, input.len()));
        assert!(storage.try_push_input_bounded(0..3, false, 4, 10));
        assert!(storage.try_push_input_bounded(3..8, false, 4, 10));
        assert!(storage.try_push_input_bounded(8..13, true, 4, 10));
        storage.scratch_extend_from_slice(b"nil");
        assert!(storage.try_push_scratch_bounded(0..3, false, 4, 10));

        storage.mark_input_nulls(input, |field| field == b"nil");
        let spans: alloc::vec::Vec<_> = storage.iter().copied().collect();
        assert!(spans[0].is_null());
        assert!(!spans[1].is_null());
        assert!(!spans[2].is_null());
        assert!(!spans[3].is_null());

        storage.trim_ascii_where(input, |quoted| !quoted);
        let resolved = storage.resolved(input);
        assert_eq!(resolved.get(1), Some(b"a".as_slice()));
        assert_eq!(resolved.get(2), Some(b"  b  ".as_slice()));
        assert_eq!(resolved.get(3), Some(b"nil".as_slice()));
    }

    #[test]
    fn trim_and_storage_boundaries_are_observable() {
        let mut one_space = Span::from_valid_range(Source::Input, 0..1, false);
        one_space.trim_ascii(b" ", b"");
        assert_eq!(one_space.range(), 1..1);

        let mut reserve = SpanStorage::with_capacity(0);
        assert_eq!(reserve.capacity(), 0);
        reserve.reserve(64);
        assert!(reserve.capacity() >= 64);

        reserve.scratch_push(b'x');
        let (_, scratch) = reserve.parts_mut();
        assert_eq!(scratch.as_slice(), b"x");

        let mut entries = SpanSet::new();
        entries.push(Source::Input, 0..1, false);
        entries.push_null(Source::Input, 1);
        let resolved = entries.resolved(b"x", b"");
        assert_eq!(resolved.get_entry(0), Some((b"x".as_slice(), false)));
        assert_eq!(resolved.get_entry(1), Some((b"".as_slice(), true)));

        let mut scratch_bounds = SpanSet::new();
        scratch_bounds.push(Source::Scratch, 0..2, false);
        scratch_bounds.push(Source::Scratch, 0..7, false);
        scratch_bounds.trim_ascii_where(b"", b"1234567", |_| false);
        assert_eq!(scratch_bounds.scratch_end(), 7);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn span_storage_truncate_uses_the_exact_requested_length() {
        let mut storage = SpanStorage::with_capacity(0);
        assert!(storage.begin(b"abc", 3));
        assert!(storage.try_push_input_bounded(0..1, false, 3, 1));
        assert!(storage.try_push_input_bounded(1..2, false, 3, 1));
        assert!(storage.try_push_input_bounded(2..3, false, 3, 1));
        storage.truncate(1);
        assert_eq!(storage.len(), 1);
        assert_eq!(storage.get(b"abc", 0), Some(b"a".as_slice()));
        assert_eq!(storage.get(b"abc", 1), None);
    }
}
