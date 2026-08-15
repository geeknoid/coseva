//! Materializing the current record into the caller's chosen shape.

use super::*;

const FUSED_SCAN_LIMIT: usize = 96;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SeekScanKind {
    Fused,
    Separate,
}

impl Engine {
    #[inline]
    fn seek_scan_kind(skipped: &[u8]) -> SeekScanKind {
        if skipped.len() <= FUSED_SCAN_LIMIT {
            SeekScanKind::Fused
        } else {
            SeekScanKind::Separate
        }
    }

    #[inline]
    fn clear_staged_selection(&mut self) {
        let _ = mem::take(&mut self.staged_form_owned);
        let _ = mem::take(&mut self.staged_valid);
    }

    #[inline]
    fn distinguishes_nulls(&self) -> bool {
        self.nulls != Nulls::None
    }

    #[inline]
    fn record_error_location(
        input: &[u8],
        line_base: u64,
        line_origin: usize,
        start: usize,
        record: u64,
    ) -> Location {
        Location {
            byte: start,
            line: physical_line(input, line_base, line_origin, start),
            record,
            field: 0,
        }
    }

    #[inline]
    fn cached_identity_matches(
        cached_names: &'static [&'static str],
        cached_aliases: FieldAliases,
        names: &'static [&'static str],
        aliases: FieldAliases,
    ) -> bool {
        ptr::eq(cached_names.as_ptr(), names.as_ptr())
            && cached_names.len() == names.len()
            && cached_aliases.len() == aliases.len()
            && (aliases.is_empty() || ptr::eq(cached_aliases.as_ptr(), aliases.as_ptr()))
    }

    #[inline]
    fn should_reclaim_engine_buffer(capacity: usize, live: usize) -> bool {
        crate::reclaim::should_reclaim(capacity, live)
    }

    #[inline]
    pub(super) fn fold_plain_record_terminator(
        &mut self,
        record_start: usize,
        terminated: bool,
        terminator: u8,
    ) {
        if self.folded_upto != record_start || terminator != b'\n' {
            return;
        }
        self.folded_lines += u64::from(terminated);
        self.folded_upto = self.location;
    }

    /// Fold a plain record's newlines into the running count.
    ///
    /// The plain kernel only accepts records without quotes, so when the
    /// terminator is a line feed the record can hold no other, and the whole
    /// count is whether it ended on one. Folding is skipped unless the record
    /// begins exactly where the last fold ended, which keeps the count honest
    /// across a record that is parsed twice, a comment run that is stepped
    /// over, and any path that reaches the location by other means: the tally
    /// then simply stops describing the drop and the scan takes over.
    #[inline]
    pub(super) fn fold_plain_record<F: CsvFormat>(
        &mut self,
        record_start: usize,
        terminated: bool,
    ) {
        self.fold_plain_record_terminator(record_start, terminated, self.fmt_terminator::<F>());
    }

    #[inline]
    fn check_materialized(&mut self) -> Option<(Range<usize>, u64)> {
        if self.cursor_start == NO_OFFSET {
            not_positioned();
        }
        // If the record is already materialized, reuse its saved extent.
        if self.cursor_end != NO_OFFSET && !self.staged_valid {
            return Some((self.cursor_start..self.cursor_end, self.cursor_index));
        }

        // This reader wants spans, so future window records should stage
        // spans too.
        self.clear_staged_selection();
        None
    }

    /// Ensure the current record is parsed with every field materialized.
    #[inline]
    pub(super) fn materialize_full<F: CsvFormat>(
        &mut self,
        input: &[u8],
    ) -> Result<(Range<usize>, u64), Error> {
        if let Some(res) = self.check_materialized() {
            return Ok(res);
        }

        self.rewind_to_current();
        let (range, index) = self.parse_positioned_record::<F>(input, false)?;
        self.cursor_end = range.end;
        Ok((range, index))
    }

    /// Borrow the current record.
    ///
    /// # Panics
    ///
    /// Panics if [`Self::advance`] has not reported a record.
    ///
    /// # Errors
    ///
    /// Returns a positioned error for malformed input or exceeded limits.
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "borrowed record construction must scalarize the validated span view"
    )]
    pub(crate) fn record<'a, F: CsvFormat>(
        &'a mut self,
        input: &'a [u8],
    ) -> Result<Record<'a>, Error> {
        let (range, index) = self.materialize_full::<F>(input)?;
        Ok(Record::new(self.spans.resolved(input), range, index)
            .with_null_aware(self.distinguishes_nulls()))
    }

    /// Advance the parser toward the next occurrence of `literal`.
    ///
    /// Returns the hit offset, or the end of input if none remains. The parser
    /// only repositions when the skipped span has no quotes, so record
    /// terminators stay unambiguous; the boolean reports whether anything was
    /// actually skipped.
    pub(super) fn seek_candidate(&mut self, input: &[u8], literal: &[u8]) -> (usize, bool) {
        let from = self.location;
        let hit = find_literal(literal, &input[from..]).map_or(input.len(), |at| from + at);
        let skipped = &input[from..hit];
        let quote = self.dialect.quote;
        let terminator = self.dialect.record_ending.byte();

        // A quote in the skipped span makes terminator counting unreliable,
        // so the caller walks the intervening records instead.
        let (records, last) = match Self::seek_scan_kind(skipped) {
            SeekScanKind::Fused => {
                let mut records = 0;
                let mut last = None;
                for (at, &byte) in skipped.iter().enumerate() {
                    if byte == quote {
                        return (hit, false);
                    }
                    if byte == terminator {
                        records += 1;
                        last = Some(at);
                    }
                }
                (records, last)
            }
            SeekScanKind::Separate => {
                if find1(quote, skipped).is_some() {
                    return (hit, false);
                }
                (count1(terminator, skipped), rfind1(terminator, skipped))
            }
        };

        if let Some(at) = last {
            // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(arith.add_to_sub, reason = "mutation causes non-termination or unbounded resource use")
            self.location = from + at + 1;
        }
        self.record_index += records as u64;

        (hit, last.is_some())
    }

    /// Copy the current record into reusable owned byte storage.
    ///
    /// # Panics
    ///
    /// Panics if [`Self::advance`] has not reported a record.
    ///
    /// # Errors
    ///
    /// Returns a positioned error for malformed input or exceeded limits.
    #[inline]
    fn try_take_staged_byte_record(&mut self, input: &[u8], output: &mut ByteRecord) -> bool {
        if self.cursor_start == NO_OFFSET {
            not_positioned();
        }

        if self.cursor_end != NO_OFFSET {
            if self.staged_valid
                && let Some(staged) = self.staged_record.as_deref_mut()
            {
                let _ = mem::take(&mut self.staged_valid);
                mem::swap(output, staged);
                self.cursor_end = NO_OFFSET;
                return true;
            }

            self.staged_form_owned = true;
            let record = Record::new(
                self.spans.resolved(input),
                self.cursor_start..self.cursor_end,
                self.cursor_index,
            )
            .with_null_aware(self.distinguishes_nulls());
            output.replace_from(&record);
            return true;
        }
        false
    }

    pub(crate) fn read_byte_record_into<F: CsvFormat>(
        &mut self,
        input: &[u8],
        output: &mut ByteRecord,
    ) -> Result<(), Error> {
        if self.try_take_staged_byte_record(input, output) {
            return Ok(());
        }

        self.read_owned::<F>(input, output)
    }

    /// Parse the positioned record straight into `output`.
    ///
    /// Keeping this wrapper as the only call site preserves inlining of the
    /// large parse body; a second caller measurably slows every reader.
    #[inline]
    pub(super) fn read_owned<F: CsvFormat>(
        &mut self,
        input: &[u8],
        output: &mut ByteRecord,
    ) -> Result<(), Error> {
        self.rewind_to_current();
        self.read_owned_positioned::<F>(input, output)
    }

    #[inline]
    pub(super) fn read_owned_positioned<F: CsvFormat>(
        &mut self,
        input: &[u8],
        output: &mut ByteRecord,
    ) -> Result<(), Error> {
        if !self.read_physical_storage::<F>(input, output.storage_mut(), false)? {
            not_positioned();
        }
        Ok(())
    }

    /// Read the current record into reusable validated UTF-8 storage.
    ///
    /// The record lends its own buffers to the byte-record parse, so the
    /// fields are laid down once and then validated where they lie. Nothing
    /// is copied between an intermediate byte record and the text record.
    ///
    /// # Panics
    ///
    /// Panics if [`Self::advance`] has not reported a record.
    ///
    /// # Errors
    ///
    /// Returns a parse error or the first invalid UTF-8 field.
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the transactional refill closure must disappear from the owned-record hot path"
    )]
    pub(crate) fn read_text_record_into<F: CsvFormat>(
        &mut self,
        input: &[u8],
        output: &mut TextRecord,
    ) -> Result<(), Error> {
        let line_base = self.line_base;
        let line_origin = self.line_origin;
        let mut parsed_location = None;
        match output.refill_with_validity(|storage| {
            let validity = self.read_text_storage::<F>(input, storage)?;
            parsed_location = Some((storage.byte_range(), storage.index()));
            Ok(validity)
        }) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error),
            Err(Utf8RecordError::InvalidField { index, error }) => {
                let (byte_range, record_index) =
                    parsed_location.expect("record metadata is set before UTF-8 validation");
                let start = byte_range.start;
                Err(
                    Error::utf8(error, index, Location::UNKNOWN).at(Self::record_error_location(
                        input,
                        line_base,
                        line_origin,
                        start,
                        record_index,
                    )),
                )
            }
        }
    }

    #[inline]
    fn try_take_staged_text_storage(&mut self, input: &[u8], output: &mut RecordStorage) -> bool {
        if self.cursor_start == NO_OFFSET {
            not_positioned();
        }

        if self.cursor_end != NO_OFFSET {
            if self.staged_valid
                && let Some(staged) = self.staged_record.as_deref_mut()
            {
                let _ = mem::take(&mut self.staged_valid);
                mem::swap(output, staged.storage_mut());
                self.cursor_end = NO_OFFSET;
                return true;
            }

            self.staged_form_owned = true;
            let record = Record::new(
                self.spans.resolved(input),
                self.cursor_start..self.cursor_end,
                self.cursor_index,
            )
            .with_null_aware(self.distinguishes_nulls());
            output.reserve(record.len(), record.byte_range().len());
            for index in 0..record.len() {
                let (field, is_null) = record.spans.get_entry(index).expect("index is in range");
                if is_null {
                    output.append_null_field();
                } else {
                    output.append_field(field);
                }
            }
            output.set_null_aware(record.null_aware);
            output.set_location(record.byte_range(), record.index());
            return true;
        }
        false
    }

    fn read_text_storage<F: CsvFormat>(
        &mut self,
        input: &[u8],
        output: &mut RecordStorage,
    ) -> Result<coseva_unsafe::storage::TextValidity, Error> {
        output.reset_text_validity();
        if self.try_take_staged_text_storage(input, output) {
            output.reset_text_validity();
            return Ok(output.text_validity());
        }

        self.rewind_to_current();
        if !self.read_physical_text_storage::<F>(input, output, false)? {
            not_positioned();
        }
        Ok(output.text_validity())
    }

    /// Report whether decoding `names` needs no header permutation.
    ///
    /// Avoids [`Self::resolve_optional_typed_mapping`], whose owned
    /// [`TypedMapping`] would clone and drop `Arc` variants even for the
    /// identity case.
    #[inline]
    fn fused_mapping_ready(
        &mut self,
        input: &[u8],
        names: &'static [&'static str],
        aliases: FieldAliases,
    ) -> Result<bool, Error> {
        if !self.headers_initialized {
            // Cold: discover headers and populate the cache.
            self.resolve_typed_mapping(input, names, aliases)?;
        }
        if self.header_record.is_none() {
            return Ok(true);
        }
        let Some((cached_names, cached_aliases, TypedMapping::Identity)) =
            self.typed_mapping.as_ref()
        else {
            return Ok(false);
        };
        Ok(Self::cached_identity_matches(
            cached_names,
            cached_aliases,
            names,
            aliases,
        ))
    }

    /// Decode the current record straight into `T` with no mapping indirection.
    fn decode_fused<'record, T, F: CsvFormat>(
        &'record mut self,
        input: &'record [u8],
    ) -> Result<T, Error>
    where
        T: CsvDecode<'record>,
    {
        let line_base = self.line_base;
        let line_origin = self.line_origin;
        let null_aware = self.distinguishes_nulls();
        let (range, record_index) = self.materialize_full::<F>(input)?;
        let start = range.start;
        let fields = FusedFields::new(self.spans.resolved(input), null_aware);
        T::fused_decode(&fields).map_err(|error| {
            error.at(Self::record_error_location(
                input,
                line_base,
                line_origin,
                start,
                record_index,
            ))
        })
    }

    /// Decode the next record, permitting fields borrowed from parser storage.
    ///
    /// # Errors
    ///
    /// Returns a parse or typed conversion error.
    pub(crate) fn decoded<'record, T, F: CsvFormat>(
        &'record mut self,
        input: &'record [u8],
    ) -> Result<T, Error>
    where
        T: CsvDecode<'record>,
    {
        // `FUSED_ARITY` is per-instantiation, so opt-out targets compile this
        // test and fused arm away.
        if T::FUSED_ARITY.is_some()
            && self.fused_mapping_ready(input, T::field_names(), T::field_aliases())?
        {
            return self.decode_fused::<T, F>(input);
        }
        let mapping =
            self.resolve_optional_typed_mapping(input, T::field_names(), T::field_aliases())?;
        self.decode_with_mapping::<_, F>(input, &mapping, DecodeNew::<T>::new())
    }

    /// Decode the next record into a caller-owned value, reusing its
    /// allocations.
    ///
    /// Derived [`CsvDecode`] implementations decode heap-bearing fields in
    /// place, so reusing one `output` avoids per-record allocations.
    ///
    /// # Errors
    ///
    /// Returns a parse or typed conversion error. `output` stays valid but may
    /// be partially updated.
    pub(crate) fn decode_into<T, F: CsvFormat>(
        &mut self,
        input: &[u8],
        output: &mut T,
    ) -> Result<(), Error>
    where
        T: CsvDecodeOwned,
    {
        if T::FUSED_ARITY.is_some()
            && self.fused_mapping_ready(input, T::field_names(), T::field_aliases())?
        {
            let line_base = self.line_base;
            let line_origin = self.line_origin;
            let null_aware = self.distinguishes_nulls();
            let (range, record_index) = self.materialize_full::<F>(input)?;
            let start = range.start;
            let fields = FusedFields::new(self.spans.resolved(input), null_aware);
            return output.fused_decode_into(&fields).map_err(|error| {
                error.at(Self::record_error_location(
                    input,
                    line_base,
                    line_origin,
                    start,
                    record_index,
                ))
            });
        }
        let mapping =
            self.resolve_optional_typed_mapping(input, T::field_names(), T::field_aliases())?;
        self.decode_with_mapping::<_, F>(input, &mapping, output)
    }

    #[inline]
    fn prepare_decode_with_mapping(&mut self, input: &[u8]) -> Result<(), Error> {
        if self.failed {
            return Err(self.error(input, ErrorKind::ParserFailed, self.location));
        }
        Ok(())
    }

    /// Decode the current record through `mapping`.
    ///
    /// The record is parsed here rather than by `advance`, so a projected
    /// mapping materializes only the fields the target type names.
    pub(crate) fn decode_with_mapping<'record, S, F: CsvFormat>(
        &'record mut self,
        input: &'record [u8],
        mapping: &TypedMapping,
        sink: S,
    ) -> Result<S::Output, Error>
    where
        S: DecodeSink<'record>,
    {
        // A poisoned parser reports the poisoning rather than the original
        // fault, matching every other second view of a failed line. Reparsing
        // would otherwise reproduce the underlying error and hide the fact that
        // the parser is no longer usable. `parse_positioned_record` does not
        // check this itself, so the guard belongs here.
        self.prepare_decode_with_mapping(input)?;

        let line_base = self.line_base;
        let line_origin = self.line_origin;
        // Projection cannot avoid copies for a lending record. Full vectorized
        // materialization costs 33% to 47% fewer instructions here and avoids
        // reparsing records already located by streaming front ends.
        //
        // Routed through `materialize_full` rather than parsing directly so a
        // record the caller has already viewed is not parsed a second time.
        // `materialize_full` rewinds itself only when it must reparse, so it
        // must not be handed a position dragged back to the record start.
        let (range, record_index) = self.materialize_full::<F>(input)?;
        let start = range.start;
        let record = Record::new(self.spans.resolved(input), range, record_index)
            .with_null_aware(self.distinguishes_nulls());
        match mapping {
            TypedMapping::Identity => sink.absorb(&record),
            TypedMapping::Mapped(mapping) => sink.absorb(&MappedRecord::new(&record, mapping)),
        }
        .map_err(|error| {
            error.at(Self::record_error_location(
                input,
                line_base,
                line_origin,
                start,
                record_index,
            ))
        })
    }

    /// Current byte offset.
    #[must_use]
    pub(crate) const fn byte_offset(&self) -> usize {
        self.location
    }

    /// Current parser location.
    #[must_use]
    pub(crate) fn location(&self, input: &[u8]) -> Location {
        Location {
            byte: self.location,
            line: physical_line(input, self.line_base, self.line_origin, self.location),
            record: self.record_index,
            field: 0,
        }
    }

    #[cfg(feature = "index")]
    pub(crate) fn line_for_offset(&self, input: &[u8], byte: usize) -> u64 {
        physical_line(input, self.line_base, self.line_origin, byte)
    }

    /// Whether parsing has reached EOF or stopped after an error.
    #[must_use]
    pub(crate) const fn is_done(&self, input: &[u8]) -> bool {
        self.failed || self.location >= input.len()
    }

    /// Whether parsing stopped after an error.
    #[cfg(feature = "std")]
    #[must_use]
    pub(crate) const fn has_failed(&self) -> bool {
        self.failed
    }

    /// Hand back scratch capacity grown by an unusually large record.
    ///
    /// Called only from the streaming front ends, on the same already-copying
    /// path that reclaims their window, so a run of ordinary records never
    /// pays for it.
    pub(crate) fn reclaim_scratch(&mut self) {
        let live = self.spans.scratch_len();
        if Self::should_reclaim_engine_buffer(self.spans.scratch_capacity(), live) {
            self.spans.shrink_scratch_to(live.max(8 * 1024));
        }
        let fields = self.spans.len();
        if Self::should_reclaim_engine_buffer(self.spans.capacity(), fields) {
            self.spans.shrink_to(fields.max(8 * 1024));
        }
        self.owned_scratch.reclaim();
    }

    #[cfg(feature = "serde")]
    #[inline]
    fn prepare_deserialize(&mut self, input: &[u8]) -> Result<(), Error> {
        self.ensure_headers_synced(input)?;
        if self.failed {
            return Err(self.error(input, ErrorKind::ParserFailed, self.location));
        }
        Ok(())
    }

    /// Deserialize the next record into `T`, allowing borrows from parser
    /// storage.
    ///
    /// Borrowed `&str`/`[u8]` fields point into the owned scratch buffer and
    /// are invalidated by the next mutable parser call. Header mapping applies
    /// with [`Headers::FirstRecord`] and [`Headers::Provided`].
    ///
    /// # Errors
    ///
    /// Returns deserialization, I/O, or parse errors.
    #[cfg(feature = "serde")]
    #[cfg(test)]
    pub(crate) fn deserialized<'record, T, F: CsvFormat>(
        &'record mut self,
        input: &'record [u8],
    ) -> Result<T, Error>
    where
        T: ::serde::Deserialize<'record>,
    {
        self.deserialized_line::<T, F>(input, false)
    }

    #[cfg(feature = "serde")]
    pub(crate) fn deserialized_line<'record, T, F: CsvFormat>(
        &'record mut self,
        input: &'record [u8],
        bom_rejected: bool,
    ) -> Result<T, Error>
    where
        T: ::serde::Deserialize<'record>,
    {
        if self.failed {
            return Err(self.error(input, ErrorKind::ParserFailed, self.location));
        }
        if !self.serde_ready {
            if bom_rejected {
                return Err(Error::new(ErrorKind::RejectedBom, Location::START));
            }
            self.prepare_deserialize(input)?;
        }

        // Projection cannot avoid copies for a lending record. A scalar
        // projected parse costs 24% to 49% more than full vectorized
        // materialization across 6 to 200 columns selecting two.
        //
        // `materialize_full` reuses a record a front end already parsed and
        // rewinds only when it has to reparse. It must not be handed a position
        // dragged back to the record start, or it returns the saved extent
        // while leaving the parser pointing at a record it has already emitted.
        let line_base = self.line_base;
        let line_origin = self.line_origin;
        let (range, record_index) = self.materialize_full::<F>(input)?;
        let start = range.start;
        let record = Record::new(self.spans.resolved(input), range, record_index)
            .with_null_aware(self.distinguishes_nulls());

        let cache = self.header_record.is_some().then_some(&self.serde_cache);
        match deserialize_full_record(&record, cache) {
            Ok(value) => {
                self.serde_cache.commit();
                Ok(value)
            }
            Err(error) => {
                let location =
                    Self::record_error_location(input, line_base, line_origin, start, record_index);
                Err(error.at(location))
            }
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::DecodeRecord;

    #[cfg(feature = "serde")]
    std::thread_local! {
        static VISITED_SERDE_KEYS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    #[test]
    #[should_panic(expected = "no current record")]
    fn test_materialize_full_unpositioned() {
        let input = b"a,b\n";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        let _ = engine.materialize_full::<Dynamic>(input);
    }

    #[test]
    #[should_panic(expected = "no current record")]
    fn test_read_byte_record_unpositioned() {
        let input = b"a,b\n";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        let mut out = ByteRecord::new();
        let _ = engine.read_byte_record_into::<Dynamic>(input, &mut out);
    }

    #[test]
    #[should_panic(expected = "no current record")]
    fn test_read_text_record_unpositioned() {
        let input = b"a,b\n";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        let mut out = TextRecord::new();
        let _ = engine.read_text_record_into::<Dynamic>(input, &mut out);
    }

    #[test]
    fn test_access_coverage_paths() {
        let input = b"col1,col2\nval1,val2\n";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(engine.advance::<Dynamic>(input).unwrap());
        assert!(!engine.has_failed());

        // Test staged record handoff in read_byte_record_into
        engine.cursor_end = 20;
        engine.staged_valid = true;
        let mut staged = Box::new(ByteRecord::new());
        staged.push_field(b"foo");
        engine.staged_record = Some(staged);
        let mut out = ByteRecord::new();
        assert!(
            engine
                .read_byte_record_into::<Dynamic>(input, &mut out)
                .is_ok()
        );
        assert_eq!(out.get(0), Some(&b"foo"[..]));

        // Test staged record handoff in read_text_record_into
        assert!(engine.advance::<Dynamic>(input).unwrap());
        engine.cursor_end = 20;
        engine.staged_valid = true;
        let mut staged_text = Box::new(ByteRecord::new());
        staged_text.push_field(b"bar");
        engine.staged_record = Some(staged_text);
        let mut text_out = TextRecord::new();
        assert!(
            engine
                .read_text_record_into::<Dynamic>(input, &mut text_out)
                .is_ok()
        );
        assert_eq!(text_out.get(0), Some("bar"));

        // Test reclaim_scratch when grown for both scratch and spans
        for _ in 0..1000 {
            engine.spans.scratch_extend_from_slice(&[b'x'; 200]);
        }
        engine.spans.reserve(100_000);
        engine.spans.clear();
        engine.reclaim_scratch();

        // Test fused_mapping_ready with uninitialized headers
        let mut unheaded_engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(unheaded_engine.advance::<Dynamic>(input).unwrap());
        unheaded_engine.headers_initialized = false;
        let _ = unheaded_engine.fused_mapping_ready(input, &["col1", "col2"], &[]);

        // Test decoded, decode_into, and deserialized on engine
        #[derive(Debug, PartialEq, Eq)]
        struct MyRow {
            col1: String,
            col2: String,
        }

        impl<'record> crate::encoding::CsvDecode<'record> for MyRow {
            fn csv_decode<R>(record: &R) -> Result<Self, crate::error::Error>
            where
                R: crate::encoding::DecodeRecord<'record> + ?Sized,
            {
                let col1 =
                    String::from_utf8_lossy(record.get_field(0).unwrap_or_default()).into_owned();
                let col2 =
                    String::from_utf8_lossy(record.get_field(1).unwrap_or_default()).into_owned();
                Ok(Self { col1, col2 })
            }

            fn field_names() -> &'static [&'static str] {
                &["col1", "col2"]
            }
        }
        let row_input = b"col1,col2\nval1,val2\n";
        let mut row_engine = Engine::from_config(
            row_input,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        assert!(row_engine.advance::<Dynamic>(row_input).unwrap());
        let decoded_row: MyRow = row_engine.decoded::<MyRow, Dynamic>(row_input).unwrap();
        assert_eq!(
            decoded_row,
            MyRow {
                col1: "val1".into(),
                col2: "val2".into()
            }
        );

        let mut row_engine2 = Engine::from_config(
            row_input,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        assert!(row_engine2.advance::<Dynamic>(row_input).unwrap());
        let mut target_row = MyRow {
            col1: String::new(),
            col2: String::new(),
        };
        assert!(
            row_engine2
                .decode_into::<MyRow, Dynamic>(row_input, &mut target_row)
                .is_ok()
        );
        assert_eq!(
            target_row,
            MyRow {
                col1: "val1".into(),
                col2: "val2".into()
            }
        );

        // Test fused decode and fused decode into paths
        #[derive(Debug, PartialEq, Eq)]
        struct FusedRow {
            col1: String,
            col2: String,
        }
        impl<'record> crate::encoding::CsvDecode<'record> for FusedRow {
            const FUSED_ARITY: Option<usize> = Some(2);
            fn csv_decode<R>(record: &R) -> Result<Self, crate::error::Error>
            where
                R: crate::encoding::DecodeRecord<'record> + ?Sized,
            {
                let col1 =
                    String::from_utf8_lossy(record.get_field(0).unwrap_or_default()).into_owned();
                let col2 =
                    String::from_utf8_lossy(record.get_field(1).unwrap_or_default()).into_owned();
                Ok(Self { col1, col2 })
            }
            fn fused_decode(
                fields: &crate::encoding::FusedFields<'record>,
            ) -> Result<Self, crate::error::Error> {
                let col1 =
                    String::from_utf8_lossy(fields.get_field(0).unwrap_or_default()).into_owned();
                let col2 =
                    String::from_utf8_lossy(fields.get_field(1).unwrap_or_default()).into_owned();
                Ok(Self { col1, col2 })
            }
            fn fused_decode_into(
                &mut self,
                fields: &crate::encoding::FusedFields<'record>,
            ) -> Result<(), crate::error::Error> {
                self.col1 =
                    String::from_utf8_lossy(fields.get_field(0).unwrap_or_default()).into_owned();
                self.col2 =
                    String::from_utf8_lossy(fields.get_field(1).unwrap_or_default()).into_owned();
                Ok(())
            }
            fn field_names() -> &'static [&'static str] {
                &["col1", "col2"]
            }
        }
        let mut fused_engine = Engine::from_config(
            row_input,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        assert!(fused_engine.advance::<Dynamic>(row_input).unwrap());
        let fused_res: FusedRow = fused_engine
            .decoded::<FusedRow, Dynamic>(row_input)
            .unwrap();
        assert_eq!(
            fused_res,
            FusedRow {
                col1: "val1".into(),
                col2: "val2".into()
            }
        );

        let mut fused_engine2 = Engine::from_config(
            row_input,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        assert!(fused_engine2.advance::<Dynamic>(row_input).unwrap());
        let mut fused_target = FusedRow {
            col1: String::new(),
            col2: String::new(),
        };
        assert!(
            fused_engine2
                .decode_into::<FusedRow, Dynamic>(row_input, &mut fused_target)
                .is_ok()
        );
        assert_eq!(
            fused_target,
            FusedRow {
                col1: "val1".into(),
                col2: "val2".into()
            }
        );

        // Test failed engine in decode_with_mapping and deserialized
        let mut failed_engine = Engine::from_config(
            row_input,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        failed_engine.failed = true;
        assert!(failed_engine.is_done(row_input));
        let mut sink = MyRow {
            col1: String::new(),
            col2: String::new(),
        };
        assert!(
            failed_engine
                .decode_with_mapping::<_, Dynamic>(row_input, &TypedMapping::Identity, &mut sink)
                .is_err()
        );
        #[cfg(feature = "serde")]
        {
            #[derive(::serde::Deserialize)]
            struct SerdeRow {
                #[expect(dead_code, reason = "test struct")]
                col1: String,
            }
            assert!(
                failed_engine
                    .deserialized::<SerdeRow, Dynamic>(row_input)
                    .is_err()
            );
        }

        // Test failing fused decode and fused decode into
        struct ErrFusedRow;
        impl<'record> crate::encoding::CsvDecode<'record> for ErrFusedRow {
            const FUSED_ARITY: Option<usize> = Some(1);
            fn csv_decode<R>(_record: &R) -> Result<Self, crate::error::Error>
            where
                R: crate::encoding::DecodeRecord<'record> + ?Sized,
            {
                Err(crate::error::Error::detailed(ErrorKind::Decode, "err"))
            }
            fn fused_decode(
                _fields: &crate::encoding::FusedFields<'record>,
            ) -> Result<Self, crate::error::Error> {
                Err(crate::error::Error::detailed(
                    ErrorKind::Decode,
                    "fused err",
                ))
            }
            fn fused_decode_into(
                &mut self,
                _fields: &crate::encoding::FusedFields<'record>,
            ) -> Result<(), crate::error::Error> {
                Err(crate::error::Error::detailed(
                    ErrorKind::Decode,
                    "fused into err",
                ))
            }
            fn field_names() -> &'static [&'static str] {
                &["col1"]
            }
            fn field_aliases() -> &'static [&'static [&'static str]] {
                &[&["c1", "col1"]]
            }
        }
        let mut err_fused_engine = Engine::from_config(
            row_input,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        assert!(err_fused_engine.advance::<Dynamic>(row_input).unwrap());
        assert!(
            err_fused_engine
                .decoded::<ErrFusedRow, Dynamic>(row_input)
                .is_err()
        );

        // Test failing headers in decoded and decode_into for both fused and non-fused
        let bad_hdr_data = b"\"unterminated header\nval1,val2\n";
        let mut bad_engine_fused = Engine::from_config(
            bad_hdr_data,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        bad_engine_fused.cursor_start = 0;
        bad_engine_fused.cursor_end = NO_OFFSET;
        assert!(
            bad_engine_fused
                .decoded::<FusedRow, Dynamic>(bad_hdr_data)
                .is_err()
        );

        let mut bad_engine_fused2 = Engine::from_config(
            bad_hdr_data,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        bad_engine_fused2.cursor_start = 0;
        bad_engine_fused2.cursor_end = NO_OFFSET;
        let mut f_target = FusedRow {
            col1: String::new(),
            col2: String::new(),
        };
        assert!(
            bad_engine_fused2
                .decode_into::<FusedRow, Dynamic>(bad_hdr_data, &mut f_target)
                .is_err()
        );

        let mut bad_engine_non_fused = Engine::from_config(
            bad_hdr_data,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        bad_engine_non_fused.cursor_start = 0;
        bad_engine_non_fused.cursor_end = NO_OFFSET;
        assert!(
            bad_engine_non_fused
                .decoded::<MyRow, Dynamic>(bad_hdr_data)
                .is_err()
        );

        let mut bad_engine_non_fused2 = Engine::from_config(
            bad_hdr_data,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        bad_engine_non_fused2.cursor_start = 0;
        bad_engine_non_fused2.cursor_end = NO_OFFSET;
        let mut nf_target = MyRow {
            col1: String::new(),
            col2: String::new(),
        };
        assert!(
            bad_engine_non_fused2
                .decode_into::<MyRow, Dynamic>(bad_hdr_data, &mut nf_target)
                .is_err()
        );

        // Test non-empty aliases in fused_mapping_ready and mismatch branches
        static FUSED_ALIASES: &[&[&str]] = &[&["c1"], &["c2"]];
        static OTHER_ALIASES: &[&[&str]] = &[&["other1"], &["other2"]];
        #[derive(Debug)]
        struct AliasFusedRow;
        impl<'record> crate::encoding::CsvDecode<'record> for AliasFusedRow {
            const FUSED_ARITY: Option<usize> = Some(2);
            fn csv_decode<R>(_record: &R) -> Result<Self, crate::error::Error>
            where
                R: crate::encoding::DecodeRecord<'record> + ?Sized,
            {
                Ok(Self)
            }
            fn fused_decode(
                _fields: &crate::encoding::FusedFields<'record>,
            ) -> Result<Self, crate::error::Error> {
                Ok(Self)
            }
            fn field_names() -> &'static [&'static str] {
                &["col1", "col2"]
            }
            fn field_aliases() -> &'static [&'static [&'static str]] {
                FUSED_ALIASES
            }
        }
        let mut alias_engine = Engine::from_config(
            row_input,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        assert!(alias_engine.advance::<Dynamic>(row_input).unwrap());
        assert!(
            alias_engine
                .decoded::<AliasFusedRow, Dynamic>(row_input)
                .is_ok()
        );

        // Mismatched alias ptr and mismatched alias len in fused_mapping_ready
        assert!(
            !alias_engine
                .fused_mapping_ready(row_input, &["col1", "col2"], OTHER_ALIASES)
                .unwrap()
        );
        assert!(
            !alias_engine
                .fused_mapping_ready(row_input, &["col1", "col2"], &[&["c1"]])
                .unwrap()
        );

        // Test materialize_full error in decode_fused and decode_with_mapping
        let lim_input = b"col1,col2\ntoolongvalue1,val2\n";
        let mut lim_engine = Engine::from_config(
            lim_input,
            ParserSettings::headed(Dialect::default(), Limits::new(100, 5, 10)),
        );
        assert!(lim_engine.advance::<Dynamic>(lim_input).unwrap());
        assert!(lim_engine.decoded::<FusedRow, Dynamic>(lim_input).is_err());

        // Malformed materialize_full in decode_fused and decode_into
        let malformed_fused_input = b"\"bad\n";
        let mut mal_fused_eng = Engine::from_config(
            malformed_fused_input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(
            mal_fused_eng
                .advance::<Dynamic>(malformed_fused_input)
                .unwrap()
        );
        assert!(
            mal_fused_eng
                .decoded::<FusedRow, Dynamic>(malformed_fused_input)
                .is_err()
        );
        let mut mal_into_eng = Engine::from_config(
            malformed_fused_input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(
            mal_into_eng
                .advance::<Dynamic>(malformed_fused_input)
                .unwrap()
        );
        let mut target_row = MyRow {
            col1: String::new(),
            col2: String::new(),
        };
        assert!(
            mal_into_eng
                .decode_into::<MyRow, Dynamic>(malformed_fused_input, &mut target_row)
                .is_err()
        );

        // Test fold_plain_record branches
        let mut fold_engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        fold_engine.folded_upto = 0;
        fold_engine.location = 5;
        fold_engine.fold_plain_record::<Dynamic>(0, true);
        assert_eq!(fold_engine.folded_lines, 1);
        assert_eq!(fold_engine.folded_upto, 5);
        // Mismatched record_start
        fold_engine.fold_plain_record::<Dynamic>(10, true);
        // Non-newline terminator
        let semi_dialect = Dialect {
            record_ending: RecordEnding::Byte(b';'),
            ..Dialect::CSV
        };
        let mut semi_engine = Engine::from_config(
            input,
            ParserSettings::unheaded(semi_dialect, Limits::DEFAULT),
        );
        semi_engine.folded_upto = 0;
        semi_engine.location = 5;
        semi_engine.fold_plain_record::<Dynamic>(0, true);
        assert_eq!(semi_engine.folded_lines, 0);

        // Test materialize_full cached extent branch
        let mut mat_engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(mat_engine.advance::<Dynamic>(input).unwrap());
        let (r1, _) = mat_engine.materialize_full::<Dynamic>(input).unwrap();
        let (r2, _) = mat_engine.materialize_full::<Dynamic>(input).unwrap();
        assert_eq!(r1, r2);

        // seek_candidate with quotes in skipped input
        let seek_input = b"\"some,quoted\ntext\",123\ntarget,456\n";
        let mut seek_engine = Engine::from_config(
            seek_input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        let (hit, skipped) = seek_engine.seek_candidate(seek_input, b"target");
        assert!(hit < seek_input.len());
        assert!(!skipped);

        // read_text_record_into with invalid UTF-8
        let bad_utf8_input = b"val1,\xFF\xFE\n";
        let mut utf8_engine = Engine::from_config(
            bad_utf8_input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(utf8_engine.advance::<Dynamic>(bad_utf8_input).unwrap());
        let mut text_rec = TextRecord::new();
        assert!(
            utf8_engine
                .read_text_record_into::<Dynamic>(bad_utf8_input, &mut text_rec)
                .is_err()
        );

        // read_byte_record_into when staged_valid == true and staged_record is Some
        let mut staged_eng = Engine::from_config(
            b"a,b\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(staged_eng.advance::<Dynamic>(b"a,b\n").unwrap());
        staged_eng.cursor_end = 4;
        staged_eng.staged_valid = true;
        let mut staged_br = ByteRecord::new();
        staged_br.push_field(b"staged");
        staged_eng.staged_record = Some(Box::new(staged_br));
        let mut out_staged = ByteRecord::new();
        staged_eng
            .read_byte_record_into::<Dynamic>(b"a,b\n", &mut out_staged)
            .unwrap();
        assert_eq!(&out_staged[0], b"staged");

        // read_text_storage when staged_valid == true
        let mut staged_eng2 = Engine::from_config(
            b"a,b\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(staged_eng2.advance::<Dynamic>(b"a,b\n").unwrap());
        staged_eng2.cursor_end = 4;
        staged_eng2.staged_valid = true;
        let mut staged_br2 = ByteRecord::new();
        staged_br2.push_field(b"staged2");
        staged_eng2.staged_record = Some(Box::new(staged_br2));
        let mut out_storage = RecordStorage::new();
        staged_eng2.try_take_staged_text_storage(b"a,b\n", &mut out_storage);
        assert_eq!(out_storage.len(), 1);

        // read_text_storage with null fields
        let mut null_eng = Engine::from_config(
            b"a,\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        null_eng.nulls = Nulls::Mysql;
        assert!(null_eng.advance::<Dynamic>(b"a,\n").unwrap());
        let _ = null_eng.materialize_full::<Dynamic>(b"a,\n").unwrap();
        let mut null_storage = RecordStorage::new();
        null_eng.try_take_staged_text_storage(b"a,\n", &mut null_storage);
        assert_eq!(null_storage.len(), 2);

        // decode_into error when materialize_full fails on fused type
        let mut lim_eng = Engine::from_config(
            b"12345,67890\n",
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 2, 10)),
        );
        assert!(lim_eng.advance::<Dynamic>(b"12345,67890\n").unwrap());
        let mut row = FusedRow {
            col1: String::new(),
            col2: String::new(),
        };
        assert!(
            lim_eng
                .decode_into::<_, Dynamic>(b"12345,67890\n", &mut row)
                .is_err()
        );
    }

    #[test]
    #[should_panic(expected = "no current record")]
    fn test_read_owned_eof() {
        let input = b"";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        engine.cursor_start = 0;
        engine.cursor_end = NO_OFFSET;
        let mut out = ByteRecord::new();
        let _ = engine.read_owned::<Dynamic>(input, &mut out);
    }

    #[test]
    #[should_panic(expected = "no current record")]
    fn test_read_text_storage_eof() {
        let input = b"";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        engine.cursor_start = 0;
        engine.cursor_end = NO_OFFSET;
        let mut storage = RecordStorage::new();
        let _ = engine.read_text_storage::<Dynamic>(input, &mut storage);
    }

    #[test]
    fn seek_candidate_preserves_record_boundaries_across_scan_paths() {
        for skipped_len in [8, 95, 96, 97, 160] {
            let mut input = vec![b'x'; skipped_len];
            input[1] = b'\n';
            input[skipped_len - 2] = b'\n';
            input.extend_from_slice(b"TARGET");

            let mut engine = Engine::from_config(
                &input,
                ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
            );
            let (hit, skipped) = engine.seek_candidate(&input, b"TARGET");

            assert_eq!(hit, skipped_len, "skipped length {skipped_len}");
            assert!(skipped, "skipped length {skipped_len}");
            assert_eq!(engine.byte_offset(), skipped_len - 1);
            assert_eq!(engine.record_index, 2);
        }

        let quoted = b"a\n\"quoted\nTARGET";
        let mut engine = Engine::from_config(
            quoted,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert_eq!(engine.seek_candidate(quoted, b"TARGET"), (10, false));
        assert_eq!(engine.byte_offset(), 0);
        assert_eq!(engine.record_index, 0);

        let absent = b"abc";
        let mut engine = Engine::from_config(
            absent,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert_eq!(
            engine.seek_candidate(absent, b"TARGET"),
            (absent.len(), false)
        );
        assert_eq!(engine.byte_offset(), 0);
        assert_eq!(engine.record_index, 0);
    }

    #[test]
    fn borrowed_record_accessors_encoding_and_iterator_are_exact() {
        let input = b"alpha,\"b,c\",caf\xc3\xa9\n,\xff,last\n";
        let first_end = 18;
        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );

        assert!(engine.advance::<Dynamic>(input).unwrap());
        {
            let record = engine.record::<Dynamic>(input).unwrap();
            assert_eq!(record.byte_range(), 0..first_end);
            assert_eq!(record.index(), 0);
            assert_eq!(record.len(), 3);
            assert_eq!(record.get(0), Some(b"alpha".as_slice()));
            assert_eq!(record.get(1), Some(b"b,c".as_slice()));
            assert_eq!(record.get_str(2).unwrap(), Some("caf\u{e9}"));
            assert_eq!(record.get(0).unwrap().as_ptr(), input.as_ptr());
            assert_eq!(
                record.get(1).unwrap().as_ptr(),
                input[7..].as_ptr(),
                "an unescaped quoted field must borrow the input"
            );

            let mut fields = (&record).into_iter();
            assert_eq!(fields.size_hint(), (3, Some(3)));
            assert_eq!(fields.next(), Some(b"alpha".as_slice()));
            assert_eq!(fields.next(), Some(b"b,c".as_slice()));
            assert_eq!(fields.next(), Some("caf\u{e9}".as_bytes()));
            assert_eq!(fields.next(), None);
            assert_eq!(fields.next(), None);
            assert_eq!(fields.size_hint(), (0, Some(0)));
        }
        assert_eq!(engine.byte_offset(), first_end);
        assert_eq!(engine.location(input).byte, first_end);

        let mut bytes = ByteRecord::new();
        engine
            .read_byte_record_into::<Dynamic>(input, &mut bytes)
            .unwrap();
        assert_eq!(bytes.byte_range(), 0..first_end);
        assert_eq!(bytes.index(), 0);
        assert_eq!(
            bytes.iter().collect::<Vec<_>>(),
            [
                b"alpha".as_slice(),
                b"b,c".as_slice(),
                "caf\u{e9}".as_bytes()
            ]
        );

        assert!(engine.advance::<Dynamic>(input).unwrap());
        {
            let record = engine.record::<Dynamic>(input).unwrap();
            assert_eq!(record.byte_range(), first_end..input.len());
            assert_eq!(record.index(), 1);
            assert_eq!(record.get(0), Some(b"".as_slice()));
            assert!(record.get_str(1).is_err());
            assert_eq!(record.get(2), Some(b"last".as_slice()));
        }
        assert!(!engine.advance::<Dynamic>(input).unwrap());
        assert!(!engine.advance::<Dynamic>(input).unwrap());
        assert!(engine.is_done(input));
        assert_eq!(engine.byte_offset(), input.len());
        assert_eq!(engine.location(input).record, 2);
    }

    #[test]
    fn owned_views_preserve_nulls_ranges_indices_and_utf8_errors() {
        let input = b"\\N,,ok\nnext,row,\xff\n";
        let mut dialect = Dialect::default();
        dialect.escape = Escape::Mysql;
        let mut settings = ParserSettings::unheaded(dialect, Limits::DEFAULT);
        settings.nulls = Nulls::Mysql;
        settings.format_tag = FormatTag::Custom;
        let mut engine = Engine::from_config(input, settings);

        assert!(engine.advance::<Dynamic>(input).unwrap());
        let mut bytes = ByteRecord::new();
        engine
            .read_byte_record_into::<Dynamic>(input, &mut bytes)
            .unwrap();
        assert_eq!(bytes.byte_range(), 0..7);
        assert_eq!(bytes.index(), 0);
        assert_eq!(bytes.is_null(0), Some(true));
        assert_eq!(bytes.is_null(1), Some(false));
        assert_eq!(bytes.get(2), Some(b"ok".as_slice()));

        let mut text = TextRecord::new();
        engine
            .read_text_record_into::<Dynamic>(input, &mut text)
            .unwrap();
        assert_eq!(text.byte_range(), 0..7);
        assert_eq!(text.index(), 0);
        assert_eq!(text.is_null(0), Some(true));
        assert_eq!(text.get(1), Some(""));
        assert_eq!(text.get(2), Some("ok"));

        assert!(engine.advance::<Dynamic>(input).unwrap());
        let error = engine
            .read_text_record_into::<Dynamic>(input, &mut text)
            .unwrap_err();
        assert!(matches!(error.kind(), ErrorKind::InvalidUtf8(_)));
        assert_eq!(error.location().byte, 7);
        assert_eq!(error.location().record, 1);
        assert_eq!(error.location().field, 2);
    }

    #[test]
    fn reclaim_scratch_keeps_the_documented_floor() {
        let input = b"a\n";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        engine.spans.reserve(100_000);
        engine.spans.scratch_extend_from_slice(&vec![b'x'; 100_000]);
        engine.spans.clear();

        engine.reclaim_scratch();

        assert_eq!(engine.spans.capacity(), 8 * 1024);
        assert_eq!(engine.spans.scratch_capacity(), 8 * 1024);
        engine.reclaim_scratch();
        assert_eq!(engine.spans.capacity(), 8 * 1024);
        assert_eq!(engine.spans.scratch_capacity(), 8 * 1024);
    }

    #[test]
    fn materialization_state_machine_reports_each_transition() {
        let input = b"a,b\n";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                engine.check_materialized()
            }))
            .is_err()
        );

        engine.cursor_start = 2;
        engine.cursor_end = 5;
        engine.cursor_index = 7;
        engine.staged_valid = false;
        engine.staged_form_owned = true;
        assert_eq!(engine.check_materialized(), Some((2..5, 7)));
        assert!(engine.staged_form_owned);

        engine.cursor_end = NO_OFFSET;
        engine.staged_valid = true;
        assert_eq!(engine.check_materialized(), None);
        assert!(!engine.staged_valid);
        assert!(!engine.staged_form_owned);

        engine.cursor_end = 5;
        engine.staged_valid = true;
        engine.staged_form_owned = true;
        assert_eq!(engine.check_materialized(), None);
        assert!(!engine.staged_valid);
        assert!(!engine.staged_form_owned);
    }

    #[test]
    fn seek_scan_selection_and_long_quote_fallback_are_exact() {
        assert_eq!(Engine::seek_scan_kind(&[]), SeekScanKind::Fused);
        assert_eq!(
            Engine::seek_scan_kind(&vec![0; FUSED_SCAN_LIMIT]),
            SeekScanKind::Fused
        );
        assert_eq!(
            Engine::seek_scan_kind(&vec![0; FUSED_SCAN_LIMIT + 1]),
            SeekScanKind::Separate
        );

        let mut input = vec![b'x'; FUSED_SCAN_LIMIT + 8];
        input[3] = b'\n';
        input[FUSED_SCAN_LIMIT + 1] = b'"';
        input.extend_from_slice(b"TARGET");
        let mut engine = Engine::from_config(
            &input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert_eq!(
            engine.seek_candidate(&input, b"TARGET"),
            (FUSED_SCAN_LIMIT + 8, false)
        );
        assert_eq!(engine.byte_offset(), 0);
        assert_eq!(engine.record_index, 0);
    }

    #[test]
    fn staged_views_expose_state_nulls_metadata_and_reserved_capacity() {
        let input = b"\\N,a,b,c,d,e,f,g,h,i,j,k,l,m,n,o,p,q,r,s,t,u,v,w,x,y,z\n";
        let mut dialect = Dialect::default();
        dialect.escape = Escape::Mysql;
        let mut settings = ParserSettings::unheaded(dialect, Limits::DEFAULT);
        settings.nulls = Nulls::Mysql;
        settings.format_tag = FormatTag::Custom;
        let mut engine = Engine::from_config(input, settings);

        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut output = ByteRecord::new();
                engine.try_take_staged_byte_record(input, &mut output)
            }))
            .is_err()
        );

        assert!(engine.advance::<Dynamic>(input).unwrap());
        let range = engine.materialize_full::<Dynamic>(input).unwrap().0;
        let record =
            Record::new(engine.spans.resolved(input), range.clone(), 0).with_null_aware(true);
        let mut expected = RecordStorage::with_capacity(record.len(), range.len());
        for index in 0..record.len() {
            let (field, is_null) = record.spans.get_entry(index).unwrap();
            if is_null {
                expected.append_null_field();
            } else {
                expected.append_field(field);
            }
        }
        drop(record);

        let mut output = RecordStorage::new();
        assert!(engine.try_take_staged_text_storage(input, &mut output));
        assert!(engine.staged_form_owned);
        assert_eq!(output.byte_range(), range);
        assert_eq!(output.index(), 0);
        assert!(output.null_aware());
        assert_eq!(output.is_null(0), Some(true));
        assert_eq!(output.field_capacity(), expected.field_capacity());
        assert_eq!(output.byte_capacity(), expected.byte_capacity());

        let mut staged = ByteRecord::new();
        staged.push_field(b"staged");
        engine.staged_record = Some(Box::new(staged));
        engine.staged_valid = true;
        engine.cursor_end = input.len();
        let mut bytes = ByteRecord::new();
        assert!(engine.try_take_staged_byte_record(input, &mut bytes));
        assert_eq!(bytes.get(0), Some(b"staged".as_slice()));
        assert!(!engine.staged_valid);
        assert_eq!(engine.cursor_end, NO_OFFSET);
    }

    #[test]
    fn access_helpers_define_identity_null_error_and_reclaim_boundaries() {
        static NAMES: &[&str] = &["a", "b"];
        static SAME_NAMES: &[&str] = NAMES;
        static ALIASES: &[&[&str]] = &[&["aa"], &["bb"]];
        static SAME_ALIASES: &[&[&str]] = ALIASES;
        let other_names: &'static [&'static str] = Box::leak(vec!["a", "b"].into_boxed_slice());
        let other_aliases: FieldAliases =
            Box::leak(vec![&["aa"][..], &["bb"][..]].into_boxed_slice());

        assert!(Engine::cached_identity_matches(
            NAMES,
            ALIASES,
            SAME_NAMES,
            SAME_ALIASES
        ));
        assert!(!Engine::cached_identity_matches(
            NAMES,
            ALIASES,
            other_names,
            SAME_ALIASES
        ));
        assert!(!Engine::cached_identity_matches(
            NAMES,
            ALIASES,
            SAME_NAMES,
            other_aliases
        ));
        assert!(!Engine::cached_identity_matches(
            NAMES,
            ALIASES,
            &["a"],
            &[&["aa"]]
        ));
        assert!(Engine::cached_identity_matches(NAMES, &[], SAME_NAMES, &[]));

        let input = b"a\nb";
        let location = Engine::record_error_location(input, 1, 0, 2, 9);
        assert_eq!(
            location,
            Location {
                byte: 2,
                line: 2,
                record: 9,
                field: 0,
            }
        );

        let none = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(!none.distinguishes_nulls());
        let mut mysql = none;
        mysql.nulls = Nulls::Mysql;
        assert!(mysql.distinguishes_nulls());

        assert!(!Engine::should_reclaim_engine_buffer(8 * 1024, 0));
        assert!(!Engine::should_reclaim_engine_buffer(32 * 1024, 0));
        assert!(Engine::should_reclaim_engine_buffer(32 * 1024 + 1, 0));
        assert!(!Engine::should_reclaim_engine_buffer(40_000, 10_000));
        assert!(Engine::should_reclaim_engine_buffer(40_001, 10_000));
    }

    #[test]
    fn read_owned_rewinds_to_the_positioned_record() {
        let input = b"first,row\nsecond,row\n";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(engine.advance::<Dynamic>(input).unwrap());
        engine.location = input.len();

        let mut output = ByteRecord::new();
        engine.read_owned::<Dynamic>(input, &mut output).unwrap();

        assert_eq!(
            output.iter().collect::<Vec<_>>(),
            [b"first".as_slice(), b"row".as_slice()]
        );
        assert_eq!(output.byte_range(), 0..10);
        assert_eq!(output.index(), 0);
    }

    #[test]
    fn reclaim_scratch_reclaims_owned_record_buffers() {
        let input = b"a\n";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        engine.owned_scratch = ByteRecord::with_capacity(100_000, 100_000);
        engine.owned_scratch.push_field(b"x");
        let before = (
            engine.owned_scratch.field_capacity(),
            engine.owned_scratch.byte_capacity(),
        );

        engine.reclaim_scratch();

        assert!(engine.owned_scratch.field_capacity() < before.0);
        assert!(engine.owned_scratch.byte_capacity() < before.1);
        assert_eq!(engine.owned_scratch.get(0), Some(b"x".as_slice()));
    }

    #[test]
    fn third_round_error_locations_use_the_exact_origin_and_start() {
        assert_eq!(
            Engine::record_error_location(b"\nX", 5, 0, 1, 7),
            Location {
                byte: 1,
                line: 6,
                record: 7,
                field: 0,
            }
        );
        assert_eq!(
            Engine::record_error_location(b"X\nY", 5, 0, 1, 8),
            Location {
                byte: 1,
                line: 5,
                record: 8,
                field: 0,
            }
        );
    }

    #[test]
    fn third_round_text_staging_requires_position_and_swaps_every_state_value() {
        let input = b"a\n";
        let mut unpositioned = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut output = RecordStorage::new();
                unpositioned.try_take_staged_text_storage(input, &mut output)
            }))
            .is_err()
        );

        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        engine.cursor_start = 0;
        engine.cursor_end = input.len();
        engine.cursor_index = 4;
        engine.staged_valid = true;
        let mut staged = ByteRecord::new();
        staged.push_null();
        staged.push_field(b"new");
        engine.staged_record = Some(Box::new(staged));

        let mut output = RecordStorage::new();
        output.append_field(b"old");
        assert!(engine.try_take_staged_text_storage(input, &mut output));
        assert_eq!(output.len(), 2);
        assert_eq!(output.is_null(0), Some(true));
        assert_eq!(output.get(1), Some(b"new".as_slice()));
        assert!(!engine.staged_valid);
        assert_eq!(engine.cursor_end, NO_OFFSET);
        assert_eq!(
            engine.staged_record.as_ref().unwrap().get(0),
            Some(b"old".as_slice())
        );
    }

    #[test]
    fn third_round_byte_staging_distinguishes_swap_and_copy_paths() {
        let input = b"a,b\n";
        let mut swapped = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        swapped.cursor_start = 0;
        swapped.cursor_end = input.len();
        swapped.staged_valid = true;
        let mut staged = ByteRecord::new();
        staged.push_field(b"staged");
        swapped.staged_record = Some(Box::new(staged));
        let mut output = ByteRecord::new();
        assert!(swapped.try_take_staged_byte_record(input, &mut output));
        assert_eq!(output.get(0), Some(b"staged".as_slice()));
        assert!(!swapped.staged_valid);
        assert_eq!(swapped.cursor_end, NO_OFFSET);

        let mut copied = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(copied.advance::<Dynamic>(input).unwrap());
        copied.materialize_full::<Dynamic>(input).unwrap();
        assert!(!copied.staged_form_owned);
        let mut output = ByteRecord::new();
        assert!(copied.try_take_staged_byte_record(input, &mut output));
        assert!(copied.staged_form_owned);
        assert_eq!(
            output.iter().collect::<Vec<_>>(),
            [b"a".as_slice(), b"b".as_slice()]
        );
    }

    #[test]
    fn third_round_copied_text_storage_preserves_null_awareness() {
        let input = b"\\N,x\n";
        let mut dialect = Dialect::default();
        dialect.escape = Escape::Mysql;
        let mut settings = ParserSettings::unheaded(dialect, Limits::DEFAULT);
        settings.nulls = Nulls::Mysql;
        settings.format_tag = FormatTag::Custom;
        let mut engine = Engine::from_config(input, settings);
        assert!(engine.advance::<Dynamic>(input).unwrap());
        engine.materialize_full::<Dynamic>(input).unwrap();

        let mut output = RecordStorage::new();
        assert!(engine.try_take_staged_text_storage(input, &mut output));
        assert!(output.null_aware());
        assert_eq!(output.is_null(0), Some(true));
        assert_eq!(output.get(1), Some(b"x".as_slice()));

        let input = b"x,y\n";
        let mut dialect = Dialect::default();
        dialect.escape = Escape::Mysql;
        let mut settings = ParserSettings::unheaded(dialect, Limits::DEFAULT);
        settings.nulls = Nulls::Mysql;
        settings.format_tag = FormatTag::Custom;
        let mut engine = Engine::from_config(input, settings);
        assert!(engine.advance::<Dynamic>(input).unwrap());
        engine.materialize_full::<Dynamic>(input).unwrap();
        let mut output = RecordStorage::new();
        assert!(engine.try_take_staged_text_storage(input, &mut output));
        assert!(output.null_aware());
        assert_eq!(output.is_null(0), Some(false));
        assert_eq!(output.is_null(1), Some(false));
    }

    #[test]
    fn third_round_fused_mapping_and_decode_paths_are_observable() {
        static NAMES: &[&str] = &["value"];

        let headed_input = b"value\nx\n";
        let mut cold = Engine::from_config(
            headed_input,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        assert!(!cold.headers_initialized);
        assert!(cold.fused_mapping_ready(headed_input, NAMES, &[]).unwrap());
        assert!(cold.headers_initialized);
        assert!(cold.header_record.is_some());
        assert!(matches!(
            cold.typed_mapping,
            Some((_, _, TypedMapping::Identity))
        ));

        let mut unheaded = Engine::from_config(
            b"x\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(unheaded.header_record.is_none());
        assert!(unheaded.fused_mapping_ready(b"x\n", NAMES, &[]).unwrap());

        #[derive(Debug, Eq, PartialEq)]
        struct PathProbe(u8);

        impl<'record> crate::encoding::CsvDecode<'record> for PathProbe {
            const FUSED_ARITY: Option<usize> = Some(1);

            fn csv_decode<R>(_record: &R) -> Result<Self, Error>
            where
                R: crate::encoding::DecodeRecord<'record> + ?Sized,
            {
                Ok(Self(1))
            }

            fn csv_decode_into<R>(&mut self, _record: &R) -> Result<(), Error>
            where
                R: crate::encoding::DecodeRecord<'record> + ?Sized,
            {
                self.0 = 2;
                Ok(())
            }

            fn fused_decode(_fields: &FusedFields<'record>) -> Result<Self, Error> {
                Ok(Self(3))
            }

            fn fused_decode_into(&mut self, _fields: &FusedFields<'record>) -> Result<(), Error> {
                self.0 = 4;
                Ok(())
            }

            fn field_names() -> &'static [&'static str] {
                NAMES
            }
        }

        let input = b"x\n";
        let mut decoded = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(decoded.advance::<Dynamic>(input).unwrap());
        assert_eq!(
            decoded.decoded::<PathProbe, Dynamic>(input).unwrap(),
            PathProbe(3)
        );

        let mut decoded_into = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(decoded_into.advance::<Dynamic>(input).unwrap());
        let mut output = PathProbe(0);
        decoded_into
            .decode_into::<PathProbe, Dynamic>(input, &mut output)
            .unwrap();
        assert_eq!(output, PathProbe(4));
    }

    #[test]
    fn third_round_reclaim_uses_exact_capacities_and_live_field_count() {
        let input = b"";

        let mut scratch = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        scratch
            .spans
            .scratch_extend_from_slice(&vec![b'x'; 16 * 1024]);
        scratch.spans.clear();
        let scratch_capacity = scratch.spans.scratch_capacity();
        assert!((8 * 1024..=32 * 1024).contains(&scratch_capacity));
        scratch.reclaim_scratch();
        assert_eq!(scratch.spans.scratch_capacity(), scratch_capacity);

        let mut exact = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        exact.spans = SpanStorage::with_capacity(32 * 1024);
        assert_eq!(exact.spans.capacity(), 32 * 1024);
        exact.reclaim_scratch();
        assert_eq!(exact.spans.capacity(), 32 * 1024);

        let mut over = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        over.spans = SpanStorage::with_capacity(32 * 1024 + 1);
        assert_eq!(over.spans.capacity(), 32 * 1024 + 1);
        over.reclaim_scratch();
        assert_eq!(over.spans.capacity(), 8 * 1024);

        let mut live = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        live.spans = SpanStorage::with_capacity(32 * 1024 + 1);
        for _ in 0..8 * 1024 {
            assert!(live.spans.try_push_input_bounded(0..0, false, 9_000, 0));
        }
        assert_eq!(live.spans.len(), 8 * 1024);
        assert_eq!(live.spans.capacity(), 32 * 1024 + 1);
        live.reclaim_scratch();
        assert_eq!(live.spans.capacity(), 8 * 1024);
    }

    #[cfg(feature = "index")]
    #[test]
    fn indexed_line_accessor_uses_the_requested_byte() {
        let input = b"a\nb\nc";
        let engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert_eq!(engine.line_for_offset(input, 0), 1);
        assert_eq!(engine.line_for_offset(input, 1), 1);
        assert_eq!(engine.line_for_offset(input, 2), 2);
        assert_eq!(engine.line_for_offset(input, 4), 3);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn successful_deserialization_commits_learned_ignored_columns() {
        use ::serde::de::{IgnoredAny, MapAccess, Visitor};
        use ::serde::{Deserialize, Deserializer};
        use core::fmt;

        struct Probe;

        impl<'de> Deserialize<'de> for Probe {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                struct ProbeVisitor;

                impl<'de> Visitor<'de> for ProbeVisitor {
                    type Value = Probe;

                    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                        formatter.write_str("a CSV probe")
                    }

                    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
                    where
                        A: MapAccess<'de>,
                    {
                        while let Some(key) = map.next_key::<String>()? {
                            VISITED_SERDE_KEYS.with(|count| count.set(count.get() + 1));
                            if key == "kept" {
                                map.next_value::<String>()?;
                            } else {
                                map.next_value::<IgnoredAny>()?;
                            }
                        }
                        Ok(Probe)
                    }
                }

                deserializer.deserialize_struct("Probe", &["kept"], ProbeVisitor)
            }
        }

        let input = b"kept,ignored\none,x\ntwo,y\n";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        assert!(engine.advance::<Dynamic>(input).unwrap());

        VISITED_SERDE_KEYS.with(|count| count.set(0));
        engine.deserialized::<Probe, Dynamic>(input).unwrap();
        assert_eq!(VISITED_SERDE_KEYS.with(std::cell::Cell::get), 2);

        assert!(engine.advance::<Dynamic>(input).unwrap());
        VISITED_SERDE_KEYS.with(|count| count.set(0));
        engine.deserialized::<Probe, Dynamic>(input).unwrap();
        assert_eq!(VISITED_SERDE_KEYS.with(std::cell::Cell::get), 1);
    }
}
