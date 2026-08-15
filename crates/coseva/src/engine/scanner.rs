//! Structural block scanning over a parser window.

use super::*;
use crate::search::{StructuralBlock, StructuralBlocks};

pub(super) struct StructuralScanner<'input> {
    inner: StructuralBlocks<'input>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TypedMapping {
    Identity,
    Mapped(Arc<[usize]>),
}

/// Fold a resolved source mapping into a [`TypedMapping`], collapsing the
/// identity case so an in-order target that names every column copies nothing.
///
/// The resolution itself — scan or indexed — happens in the engine, which owns
/// the reusable header lookup the indexed path needs.
pub(super) fn typed_mapping_from(source_mapping: Vec<usize>, header_len: usize) -> TypedMapping {
    if source_mapping.len() == header_len
        && source_mapping
            .iter()
            .enumerate()
            .all(|(target, &source)| target == source)
    {
        return TypedMapping::Identity;
    }
    // A target that names a subset of the columns is no different here from one
    // that names all of them in another order: both decode through a lending
    // record, which copies nothing, so the columns nobody asked for cost only
    // the scan that found their boundaries.
    TypedMapping::Mapped(source_mapping.into())
}

impl<'input> StructuralScanner<'input> {
    pub(super) fn resume(
        input: &'input [u8],
        delimiter: u8,
        quote: u8,
        record_ending: u8,
        position: usize,
        cache: BlockCache,
    ) -> Self {
        Self {
            inner: StructuralBlocks::resume(
                input,
                delimiter,
                quote,
                record_ending,
                position,
                cache,
            ),
        }
    }
}

impl<'input> Iterator for StructuralScanner<'input> {
    type Item = StructuralBlock<'input>;

    /// `#[inline]` is measured: without it the scanner isn't inlined into
    /// the record parser, costing ~10% of parsing instructions.
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod mapping_tests {
    use super::*;

    /// A header record over `names`, copying nothing the test cannot spare.
    fn headers(names: &[&str]) -> ByteRecord {
        names.iter().map(|name| name.as_bytes()).collect()
    }

    /// The name-to-column lookup the indexed resolver reads, built the way the
    /// engine builds it.
    fn lookup_for(record: &ByteRecord) -> HeaderLookup {
        let mut lookup = HeaderLookup::default();
        lookup.rebuild(record);
        lookup
    }

    /// Leak `names` so a runtime-built name list satisfies the `'static` bound
    /// the resolvers require. Tests are short-lived, so the leak is harmless.
    fn leak_names(names: Vec<String>) -> &'static [&'static str] {
        let leaked: Vec<&'static str> = names
            .into_iter()
            .map(|name| &*Box::leak(name.into_boxed_str()))
            .collect();
        Box::leak(leaked.into_boxed_slice())
    }

    /// `cNNN` for `0..count`, the same fixed-width names the width benchmarks
    /// use, so a column keeps its name at every width.
    fn column_names(count: usize) -> Vec<String> {
        (0..count).map(|index| format!("c{index:03}")).collect()
    }

    /// Assert the scan and indexed resolvers agree, so a wide typed mapping can
    /// take the indexed path without changing a mapping or an error.
    #[track_caller]
    #[expect(
        clippy::panic,
        reason = "a resolver disagreement is a test failure and must fail loudly"
    )]
    fn assert_parity(
        record: &ByteRecord,
        names: &'static [&'static str],
        aliases: FieldAliases,
    ) -> Result<Vec<usize>, Error> {
        let lookup = lookup_for(record);
        let scanned = resolve_decode_mapping(record, names, aliases);
        let indexed = resolve_decode_mapping_indexed(record, &lookup, names, aliases);
        match (scanned, indexed) {
            (Ok(scan), Ok(index)) => {
                assert_eq!(scan, index, "scan and indexed mappings differ");
                Ok(scan)
            }
            (Err(scan), Err(index)) => {
                assert_eq!(
                    scan.kind(),
                    index.kind(),
                    "scan and indexed error kinds differ"
                );
                Err(scan)
            }
            (scan, index) => panic!("resolvers disagreed: {scan:?} vs {index:?}"),
        }
    }

    #[test]
    fn threshold_keeps_sparse_projections_on_the_scan() {
        // The projected benchmarks must keep scanning: two of a hundred and
        // five of a hundred stay below the threshold, while naming a wide
        // header in full crosses it.
        assert!(!wide_mapping(2, 100));
        assert!(!wide_mapping(5, 100));
        assert!(!wide_mapping(2, 200));
        assert!(wide_mapping(64, 64));
        assert!(wide_mapping(100, 100));
        assert!(wide_mapping(200, 200));
    }

    #[test]
    fn identity_and_reordered_wide_mappings_match_the_scan() {
        for width in [64_usize, 100, 200] {
            let record = headers(
                &column_names(width)
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            );

            // Every column in order.
            let identity = leak_names(column_names(width));
            assert_eq!(
                assert_parity(&record, identity, &[]).expect("identity resolves"),
                (0..width).collect::<Vec<_>>(),
            );

            // Every column, reversed, so nothing decodes as the identity and
            // every name resolves to a different column than its position.
            let mut reversed = column_names(width);
            reversed.reverse();
            let reversed = leak_names(reversed);
            assert_eq!(
                assert_parity(&record, reversed, &[]).expect("reversed resolves"),
                (0..width).rev().collect::<Vec<_>>(),
            );

            // First and last only, the two-column shape that must still resolve
            // correctly at a width the indexed path handles.
            let ends = leak_names(vec!["c000".to_owned(), format!("c{:03}", width - 1)]);
            assert_eq!(
                assert_parity(&record, ends, &[]).expect("ends resolve"),
                vec![0, width - 1],
            );
        }
    }

    #[test]
    fn missing_and_ambiguous_wide_names_error_like_the_scan() {
        let record = headers(
            &column_names(100)
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        );

        // A name no column carries is a decode error on both paths.
        let mut with_missing = column_names(100);
        with_missing[10] = "absent".to_owned();
        let with_missing = leak_names(with_missing);
        let error = assert_parity(&record, with_missing, &[]).expect_err("missing name errors");
        assert_eq!(error.kind(), ErrorKind::Decode);

        // A duplicated header makes the name that reaches it ambiguous. Column
        // 50 is renamed to repeat column 49's name, so resolving `c049` finds
        // two columns on both paths.
        let mut dup_names = column_names(100);
        dup_names[50] = "c049".to_owned();
        let dup_record = headers(&dup_names.iter().map(String::as_str).collect::<Vec<_>>());
        let names = leak_names(column_names(100));
        let error =
            assert_parity(&dup_record, names, &[]).expect_err("a duplicate header is ambiguous");
        assert_eq!(error.kind(), ErrorKind::Decode);
    }

    #[test]
    fn aliases_resolve_and_stay_ambiguous_on_both_paths() {
        // A name absent from the header but matched by an alias resolves to the
        // aliased column, at a width that uses the indexed path.
        let mut header_names = column_names(80);
        header_names[7] = "legacy".to_owned();
        let record = headers(&header_names.iter().map(String::as_str).collect::<Vec<_>>());

        let mut names = column_names(80);
        names[7] = "modern".to_owned();
        let names = leak_names(names);

        let mut aliases: Vec<&'static [&'static str]> = vec![&[]; 80];
        aliases[7] = Box::leak(vec!["legacy"].into_boxed_slice());
        let aliases: FieldAliases = Box::leak(aliases.into_boxed_slice());

        let mapping = assert_parity(&record, names, aliases).expect("alias resolves");
        assert_eq!(mapping[7], 7);

        // When both the name and its alias are present as distinct columns, the
        // union is two columns and the field is ambiguous on both paths.
        let mut both = header_names.clone();
        both[7] = "modern".to_owned();
        both.push("legacy".to_owned());
        let both_record = headers(&both.iter().map(String::as_str).collect::<Vec<_>>());
        let error =
            assert_parity(&both_record, names, aliases).expect_err("name and alias both present");
        assert_eq!(error.kind(), ErrorKind::Decode);
    }

    #[test]
    fn a_name_equal_to_its_own_alias_is_not_treated_as_a_second_match() {
        // The indexed path must fold the repeat away rather than count the same
        // column twice and reject a legitimately unique name.
        let record = headers(&["only"]);
        let names: &'static [&'static str] = &["only"];
        let aliases: FieldAliases = &[&["only"]];
        let mapping = assert_parity(&record, names, aliases).expect("self-alias resolves");
        assert_eq!(mapping, vec![0]);
    }

    #[test]
    fn typed_mapping_only_collapses_a_complete_identity() {
        assert_eq!(typed_mapping_from(vec![0, 1, 2], 3), TypedMapping::Identity);
        assert_eq!(
            typed_mapping_from(vec![0, 2, 1], 3),
            TypedMapping::Mapped(Arc::from([0, 2, 1]))
        );
        assert_eq!(
            typed_mapping_from(vec![0, 1], 3),
            TypedMapping::Mapped(Arc::from([0, 1]))
        );
    }

    #[test]
    fn resumed_scanning_progresses_from_the_requested_offset() {
        let input = b"aa,bb\"cc\ndd,ee\n012345678901234567890123456789,tail";
        let mut scanner = StructuralScanner::resume(input, b',', b'"', b'\n', 5, BlockCache::new());
        let mut matches = Vec::new();

        for mut block in &mut scanner {
            while let Some(found) = block.next_match() {
                matches.push(found);
            }
        }

        assert_eq!(
            matches,
            vec![(5, b'"'), (8, b'\n'), (11, b','), (14, b'\n'), (45, b',')]
        );
    }
}
