//! Header discovery, lookup, and typed-mapping caching.

use super::*;

impl Engine {
    /// Establish the headers and bring the Serde header cache up to date.
    ///
    /// Runs once per record on the Serde path, so the whole check has to fold
    /// into a single test. `serde_ready` implies `headers_initialized`, which
    /// is why this does not simply call `ensure_headers` and then test a second
    /// flag: on the second and every later record neither the header check nor
    /// the sync check is reached.
    #[cfg(feature = "serde")]
    #[inline]
    pub(crate) fn ensure_headers_synced(&mut self, input: &[u8]) -> Result<(), Error> {
        if self.serde_ready {
            return Ok(());
        }
        self.sync_headers(input)
    }

    #[cfg(feature = "serde")]
    #[cold]
    #[inline(never)]
    fn sync_headers(&mut self, input: &[u8]) -> Result<(), Error> {
        self.ensure_headers(input)?;
        self.serde_cache.sync(self.header_record.as_ref());
        self.serde_ready = true;
        Ok(())
    }

    pub(crate) fn ensure_headers(&mut self, input: &[u8]) -> Result<(), Error> {
        if self.headers_initialized {
            return Ok(());
        }
        self.headers_initialized = true;
        match self.consume_first_record {
            false => {}
            true => {
                self.header_record = self
                    .next_physical_record(input, true)?
                    .map(|record| ByteRecord::copied_from(&record));
                self.on_headers_changed();
            }
        }
        Ok(())
    }

    /// Discard everything derived from the header record.
    ///
    /// Called whenever the headers change. The lookup is only invalidated here,
    /// not rebuilt; see `header_lookup` for why.
    pub(super) fn on_headers_changed(&mut self) {
        self.header_lookup.clear();
        self.header_lookup_ready = bool::default();
        let _ = self.typed_mapping.take();
        let _ = self.filter_column.take();
        #[cfg(feature = "serde")]
        {
            self.serde_ready = bool::default();
        }
    }

    /// The name-to-column lookup, building it if this is the first read since
    /// the headers changed.
    pub(super) fn header_slots(&mut self, name: &[u8]) -> Option<&HeaderSlots> {
        self.ensure_header_lookup();
        self.header_lookup.get(self.header_record.as_ref()?, name)
    }

    /// Build the name-to-column lookup over the current headers, once per header
    /// change.
    ///
    /// The map is built on demand rather than eagerly; see the `header_lookup`
    /// field for why. Typed decode reaches this only when a wide type crosses
    /// the indexing threshold, so a narrow struct never builds it.
    pub(super) fn ensure_header_lookup(&mut self) {
        if !self.header_lookup_ready {
            self.header_lookup_ready = true;
            if let Some(headers) = &self.header_record {
                self.header_lookup.rebuild(headers);
            }
        }
    }

    /// The cached column for `name`, if one was resolved for this header
    /// record.
    ///
    /// Filtering asks once per record it accepts, and the answer cannot change
    /// while the headers stand, so the hash, collision walk and comparison
    /// against the header record happen once per run instead.
    #[inline]
    pub(crate) fn cached_filter_column(&self, name: &[u8]) -> Option<usize> {
        let (cached, column) = self.filter_column.as_ref()?;
        (cached.as_slice() == name).then_some(*column)
    }

    /// Remember `column` as the answer for `name`.
    ///
    /// Reached once per run, or once more whenever the predicate or the
    /// headers change.
    #[cold]
    pub(crate) fn store_filter_column(&mut self, name: &[u8], column: usize) {
        match &mut self.filter_column {
            Some((cached, slot)) => {
                cached.clear();
                cached.extend_from_slice(name);
                *slot = column;
            }
            slot => *slot = Some((name.to_vec(), column)),
        }
    }

    pub(crate) fn resolve_typed_mapping(
        &mut self,
        input: &[u8],
        names: &'static [&'static str],
        aliases: FieldAliases,
    ) -> Result<TypedMapping, Error> {
        self.ensure_headers(input)?;
        if let Some(mapping) = self.cached_typed_mapping(names, aliases) {
            return Ok(mapping);
        }
        let mapping = self
            .compute_typed_mapping(names, aliases)
            .map_err(|error| error.at(self.location(input)))?;
        self.typed_mapping = Some((names, aliases, mapping.clone()));
        Ok(mapping)
    }

    /// Resolve a fresh typed mapping against the current headers.
    ///
    /// A wide type against a wide header resolves through the reusable header
    /// lookup rather than scanning every header once per name; a narrow type or
    /// a sparse projection keeps the allocation-free scan. Both paths preserve
    /// the same duplicate, alias, missing-column, and ambiguity semantics.
    fn compute_typed_mapping(
        &mut self,
        names: &'static [&'static str],
        aliases: FieldAliases,
    ) -> Result<TypedMapping, Error> {
        let Some(header_len) = self.header_record.as_ref().map(ByteRecord::len) else {
            return Ok(TypedMapping::Identity);
        };
        let source = if wide_mapping(names.len(), header_len) {
            self.ensure_header_lookup();
            let headers = self.header_record.as_ref().expect("header record present");
            resolve_decode_mapping_indexed(headers, &self.header_lookup, names, aliases)?
        } else {
            let headers = self.header_record.as_ref().expect("header record present");
            resolve_decode_mapping(headers, names, aliases)?
        };
        Ok(typed_mapping_from(source, header_len))
    }

    #[inline]
    pub(crate) fn resolve_optional_typed_mapping(
        &mut self,
        input: &[u8],
        names: &'static [&'static str],
        aliases: FieldAliases,
    ) -> Result<TypedMapping, Error> {
        // Decoding resolves a mapping per record even though it only changes
        // when the headers do, so the cache is consulted before anything else
        // and `ensure_headers` is not repeated on the way to the slow path.
        if self.headers_initialized
            && let Some(mapping) = self.cached_typed_mapping(names, aliases)
        {
            return Ok(mapping);
        }
        self.ensure_headers(input)?;
        if self.header_record.is_none() {
            Ok(TypedMapping::Identity)
        } else {
            self.resolve_typed_mapping(input, names, aliases)
        }
    }

    /// Return the cached mapping when it was built for these names.
    ///
    /// The aliases join the key because two types can share one deduplicated
    /// names constant while resolving different columns through it.
    #[inline]
    fn cached_typed_mapping(
        &mut self,
        names: &'static [&'static str],
        aliases: FieldAliases,
    ) -> Option<TypedMapping> {
        self.recache_typed_mapping(names, aliases)
    }

    /// Re-key the cached mapping when equal names arrived at a new address.
    #[cold]
    fn recache_typed_mapping(
        &mut self,
        names: &'static [&'static str],
        aliases: FieldAliases,
    ) -> Option<TypedMapping> {
        let (cached_names, cached_aliases, mapping) = match self.typed_mapping.as_mut() {
            Some(entry) => entry,
            None => return None,
        };
        if *cached_names != names || *cached_aliases != aliases {
            return None;
        }
        *cached_names = names;
        *cached_aliases = aliases;
        Some(mapping.clone())
    }

    /// Return the configured or discovered headers.
    ///
    /// # Errors
    ///
    /// Returns a parse error when discovering first-record headers fails.
    pub(crate) fn headers(&mut self, input: &[u8]) -> Result<Option<&ByteRecord>, Error> {
        self.ensure_headers(input)?;
        Ok(self.header_record.as_ref())
    }

    /// Resolve the first header with the requested name.
    ///
    /// # Errors
    ///
    /// Returns a parse error when discovering first-record headers fails.
    pub(crate) fn header_index(
        &mut self,
        input: &[u8],
        name: impl AsRef<[u8]>,
    ) -> Result<Option<usize>, Error> {
        self.ensure_headers(input)?;
        Ok(self.header_slots(name.as_ref()).map(HeaderSlots::first))
    }

    /// Resolve every duplicate header with the requested name.
    ///
    /// # Errors
    ///
    /// Returns a parse error when discovering first-record headers fails.
    pub(crate) fn header_indices(
        &mut self,
        input: &[u8],
        name: impl AsRef<[u8]>,
    ) -> Result<&[usize], Error> {
        self.ensure_headers(input)?;
        Ok(self
            .header_slots(name.as_ref())
            .map_or(&[], HeaderSlots::as_slice))
    }

    /// Whether this parser uses discovered or caller-provided headers.
    #[must_use]
    pub(crate) fn has_headers(&self) -> bool {
        self.consume_first_record || self.header_record.is_some()
    }

    /// Replace the header record without consuming input.
    ///
    /// Subsequent named decoding uses this record, and the next input record
    /// is treated as data.
    pub(crate) fn set_headers(&mut self, headers: ByteRecord) {
        if self.field_count == FieldCount::MatchFirst {
            self.expected_fields = Some(headers.len());
        }
        self.consume_first_record = bool::default();
        self.header_record = Some(headers);
        self.headers_initialized = true;
        self.on_headers_changed();
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    fn record(names: &[&str]) -> ByteRecord {
        names.iter().map(|name| name.as_bytes()).collect()
    }

    #[test]
    fn test_headers_coverage_paths() {
        let input = b"a,b\n1,2\n";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        // cached_typed_mapping when typed_mapping is None
        let mut fresh_engine = Engine::from_config(
            input,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        assert!(fresh_engine.cached_typed_mapping(NAMES_A, &[]).is_none());
        engine.store_filter_column(b"col1", 0);
        engine.store_filter_column(b"col2", 1);
        assert_eq!(engine.cached_filter_column(b"col2"), Some(1));

        // set_headers with MatchFirst
        let mut settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        settings.field_count = FieldCount::MatchFirst;
        let mut engine2 = Engine::from_config(input, settings);
        let mut headers = ByteRecord::new();
        headers.push_field(b"h1");
        headers.push_field(b"h2");
        engine2.set_headers(headers);
        assert_eq!(engine2.expected_fields, Some(2));

        // recache_typed_mapping with equal names at different static address
        static NAMES_A: &[&str] = &["a", "b"];
        static NAMES_B: &[&str] = &["a", "b"];
        static NAMES_C: &[&str] = &["c", "d"];
        static ALIASES_A: &[&[&str]] = &[&["alt_a"], &[]];
        static ALIASES_B: &[&[&str]] = &[&["alt_a"], &[]];
        let _ = engine.resolve_typed_mapping(input, NAMES_A, ALIASES_A);
        // Call with same aliases
        let _ = engine.resolve_typed_mapping(input, NAMES_A, ALIASES_A);
        // Call with empty aliases
        let _ = engine.resolve_typed_mapping(input, NAMES_A, &[]);
        let _ = engine.resolve_typed_mapping(input, NAMES_A, &[]);
        // Call with NAMES_B which has same contents but different pointer
        let _ = engine.resolve_typed_mapping(input, NAMES_B, ALIASES_B);
        // Call with NAMES_C which has different contents
        let _ = engine.resolve_typed_mapping(input, NAMES_C, &[]);
        // Test recache_typed_mapping when typed_mapping is None
        let mut fresh_engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(fresh_engine.recache_typed_mapping(NAMES_A, &[]).is_none());

        // headers, header_index, header_indices
        assert!(engine.headers(input).unwrap().is_some());
        assert_eq!(engine.header_index(input, "a").unwrap(), Some(0));
        assert_eq!(engine.header_indices(input, "a").unwrap(), &[0]);
        let mut unheaded = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        unheaded.headers_initialized = false;
        assert!(unheaded.ensure_headers(input).is_ok());
        assert_eq!(
            unheaded.compute_typed_mapping(NAMES_A, &[]).unwrap(),
            TypedMapping::Identity
        );
        assert_eq!(
            unheaded
                .resolve_optional_typed_mapping(input, NAMES_A, &[])
                .unwrap(),
            TypedMapping::Identity
        );

        // header_index, header_indices, has_headers
        assert_eq!(engine.header_index(input, "a").unwrap(), Some(0));
        assert_eq!(engine.header_indices(input, "a").unwrap(), &[0]);
        assert_eq!(
            engine.header_indices(input, "missing").unwrap(),
            &[] as &[usize]
        );
        assert!(engine.has_headers());

        #[cfg(feature = "serde")]
        {
            let mut fresh_serde_eng = Engine::from_config(
                input,
                ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
            );
            assert!(fresh_serde_eng.ensure_headers_synced(input).is_ok());
            // call again when serde_ready is true
            assert!(fresh_serde_eng.ensure_headers_synced(input).is_ok());
        }

        // wide_mapping test in compute_typed_mapping
        let mut wide_hdr_eng = Engine::from_config(
            b"",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        let mut wide_hdr = ByteRecord::new();
        for i in 0..20 {
            wide_hdr.push_field(format!("col_{i}").as_bytes());
        }
        wide_hdr_eng.header_record = Some(wide_hdr);
        static WIDE_NAMES: &[&str] = &[
            "col_0", "col_1", "col_2", "col_3", "col_4", "col_5", "col_6", "col_7", "col_8",
            "col_9",
        ];
        assert!(wide_hdr_eng.compute_typed_mapping(WIDE_NAMES, &[]).is_ok());

        // resolve_optional_typed_mapping when headers_initialized is false and header_record is present
        let mut uninit_eng = Engine::from_config(
            input,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        uninit_eng.headers_initialized = false;
        assert!(
            uninit_eng
                .resolve_optional_typed_mapping(input, NAMES_A, &[])
                .is_ok()
        );

        // Error paths when ensure_headers fails
        let bad_hdr_input = b"\"unterminated header";
        let bad_engine = || {
            Engine::from_config(
                bad_hdr_input,
                ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
            )
        };
        assert!(bad_engine().headers(bad_hdr_input).is_err());
        assert!(bad_engine().header_index(bad_hdr_input, "a").is_err());
        assert!(bad_engine().header_indices(bad_hdr_input, "a").is_err());
        assert!(
            bad_engine()
                .resolve_typed_mapping(bad_hdr_input, NAMES_A, &[])
                .is_err()
        );
        assert!(
            bad_engine()
                .resolve_optional_typed_mapping(bad_hdr_input, NAMES_A, &[])
                .is_err()
        );
        #[cfg(feature = "serde")]
        {
            assert!(bad_engine().ensure_headers_synced(bad_hdr_input).is_err());
        }
    }

    #[test]
    fn header_changes_invalidate_every_derived_cache() {
        static NAMES: &[&str] = &["old"];

        let old_headers = record(&["old"]);
        let mut engine = Engine::from_config(
            b"",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        engine.header_lookup.rebuild(&old_headers);
        engine.header_lookup_ready = true;
        engine.typed_mapping = Some((NAMES, &[], TypedMapping::Identity));
        engine.store_filter_column(b"old", 0);
        #[cfg(feature = "serde")]
        {
            engine.serde_ready = true;
        }

        engine.on_headers_changed();

        assert!(engine.header_lookup.get(&old_headers, b"old").is_none());
        assert!(!engine.header_lookup_ready);
        assert!(engine.typed_mapping.is_none());
        assert!(engine.filter_column.is_none());
        #[cfg(feature = "serde")]
        assert!(!engine.serde_ready);
    }

    #[test]
    fn discovered_headers_initialize_without_leaving_stale_caches() {
        static NAMES: &[&str] = &["stale"];

        let input = b"left,right\n1,2\n";
        let stale_headers = record(&["stale"]);
        let mut engine = Engine::from_config(
            input,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        engine.header_lookup.rebuild(&stale_headers);
        engine.header_lookup_ready = true;
        engine.typed_mapping = Some((NAMES, &[], TypedMapping::Identity));
        engine.store_filter_column(b"stale", 0);

        engine.ensure_headers(input).expect("header record parses");

        let headers = engine.header_record.as_ref().expect("headers discovered");
        assert_eq!(headers.get(0), Some(&b"left"[..]));
        assert_eq!(headers.get(1), Some(&b"right"[..]));
        assert!(engine.header_lookup.get(&stale_headers, b"stale").is_none());
        assert!(!engine.header_lookup_ready);
        assert!(engine.typed_mapping.is_none());
        assert!(engine.filter_column.is_none());
    }

    #[test]
    fn unheaded_header_initialization_does_not_consume_the_first_record() {
        let input = b"a,b\n1,2\n";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );

        engine
            .ensure_headers(input)
            .expect("unheaded setup succeeds");

        assert!(engine.headers_initialized);
        assert!(engine.header_record.is_none());
        assert_eq!(engine.location, 0);
    }

    #[test]
    fn header_lookup_builds_once_and_records_readiness() {
        let mut engine = Engine::from_config(
            b"",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        engine.set_headers(record(&["alpha"]));

        engine.ensure_header_lookup();
        assert!(engine.header_lookup_ready);
        assert_eq!(
            engine
                .header_lookup
                .get(engine.header_record.as_ref().expect("headers"), b"alpha")
                .map(HeaderSlots::as_slice),
            Some(&[0][..])
        );

        engine.header_lookup.clear();
        engine.ensure_header_lookup();
        assert!(
            engine
                .header_lookup
                .get(engine.header_record.as_ref().expect("headers"), b"alpha")
                .is_none(),
            "a ready lookup must not be rebuilt"
        );
    }

    #[test]
    fn replacing_a_cached_filter_column_reuses_its_name_allocation() {
        let mut engine = Engine::from_config(
            b"",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        engine.store_filter_column(b"a deliberately long cached column name", 0);
        let original = engine
            .filter_column
            .as_ref()
            .expect("filter cache")
            .0
            .as_ptr();

        engine.store_filter_column(b"short", 3);

        let (name, column) = engine.filter_column.as_ref().expect("filter cache");
        assert_eq!(name, b"short");
        assert_eq!(*column, 3);
        assert_eq!(name.as_ptr(), original);
    }

    #[test]
    fn typed_mapping_cache_records_hits_and_rekeys_equal_content() {
        static ALIAS: &[&str] = &["legacy"];
        static EMPTY: &[&str] = &[];

        let first_names: &'static [&'static str] = Box::leak(vec!["a", "b"].into_boxed_slice());
        let second_names: &'static [&'static str] = Box::leak(vec!["a", "b"].into_boxed_slice());
        let aliases_one: FieldAliases = Box::leak(vec![ALIAS, EMPTY].into_boxed_slice());
        let aliases_two: FieldAliases = Box::leak(vec![ALIAS, EMPTY].into_boxed_slice());
        let mut engine = Engine::from_config(
            b"",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        engine.set_headers(record(&["a", "b"]));

        let mapping = engine
            .resolve_typed_mapping(b"", first_names, &[])
            .expect("mapping resolves");
        assert_eq!(mapping, TypedMapping::Identity);
        assert!(engine.typed_mapping.is_some());
        assert_eq!(
            engine.cached_typed_mapping(first_names, &[]),
            Some(TypedMapping::Identity)
        );

        engine.typed_mapping = Some((first_names, aliases_one, TypedMapping::Identity));
        assert_eq!(
            engine.cached_typed_mapping(second_names, aliases_two),
            Some(TypedMapping::Identity)
        );
        let (cached_names, cached_aliases, _) =
            engine.typed_mapping.as_ref().expect("mapping recached");
        assert!(ptr::eq(cached_names.as_ptr(), second_names.as_ptr()));
        assert!(ptr::eq(cached_aliases.as_ptr(), aliases_two.as_ptr()));

        let different: &'static [&'static str] =
            Box::leak(vec!["a", "different"].into_boxed_slice());
        assert!(
            engine
                .cached_typed_mapping(different, aliases_two)
                .is_none()
        );
    }

    #[test]
    fn optional_mapping_without_headers_does_not_create_a_cache_entry() {
        static NAMES: &[&str] = &["a"];

        let mut engine = Engine::from_config(
            b"a\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert_eq!(
            engine
                .resolve_optional_typed_mapping(b"a\n", NAMES, &[])
                .expect("unheaded mapping"),
            TypedMapping::Identity
        );
        assert!(engine.typed_mapping.is_none());
    }

    #[test]
    fn wide_mapping_path_follows_both_threshold_boundaries() {
        fn numbered_headers(count: usize) -> ByteRecord {
            let mut headers = ByteRecord::new();
            for index in 0..count {
                headers.push_field(format!("c{index:04}").as_bytes());
            }
            headers
        }

        fn numbered_names(count: usize) -> &'static [&'static str] {
            let names = (0..count)
                .map(|index| {
                    let name = format!("c{index:04}");
                    &*Box::leak(name.into_boxed_str())
                })
                .collect::<Vec<_>>();
            Box::leak(names.into_boxed_slice())
        }

        let mut above = Engine::from_config(
            b"",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        above.set_headers(numbered_headers(1025));
        above
            .compute_typed_mapping(numbered_names(1), &[])
            .expect("one name across 1025 headers resolves");
        assert!(above.header_lookup_ready);

        let mut at = Engine::from_config(
            b"",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        at.set_headers(numbered_headers(32));
        at.compute_typed_mapping(numbered_names(32), &[])
            .expect("32 by 32 mapping resolves");
        assert!(!at.header_lookup_ready);
    }

    #[test]
    fn set_headers_sets_policy_state_and_only_matches_width_when_requested() {
        let input = b"data\n";
        let mut flexible_settings = ParserSettings::headed(Dialect::default(), Limits::DEFAULT);
        flexible_settings.field_count = FieldCount::Flexible;
        let mut flexible = Engine::from_config(input, flexible_settings);
        flexible.set_headers(record(&["a", "b"]));
        assert_eq!(flexible.expected_fields, None);
        assert!(!flexible.consume_first_record);
        assert!(flexible.headers_initialized);

        let mut matching_settings = ParserSettings::headed(Dialect::default(), Limits::DEFAULT);
        matching_settings.field_count = FieldCount::MatchFirst;
        let mut matching = Engine::from_config(input, matching_settings);
        matching.set_headers(record(&["a", "b", "c"]));
        assert_eq!(matching.expected_fields, Some(3));
        assert!(!matching.consume_first_record);
        assert!(matching.headers_initialized);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_header_sync_short_circuits_and_marks_readiness() {
        let bad_input = b"\"unterminated";
        let mut already_ready = Engine::from_config(
            bad_input,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        already_ready.serde_ready = true;
        assert!(
            already_ready.ensure_headers_synced(bad_input).is_ok(),
            "a ready Serde cache must not parse headers again"
        );

        let input = b"a,b\n1,2\n";
        let mut fresh = Engine::from_config(
            input,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        fresh
            .ensure_headers_synced(input)
            .expect("fresh Serde headers sync");
        assert!(fresh.serde_ready);
        assert!(fresh.headers_initialized);
    }
}
