//! Reusable field projections.
//!
//! A [`FieldProjection`] resolves the columns a workload cares about once, and
//! is then applied to every record with `project`. Resolution against headers
//! happens a single time, so the per-record cost is one indexed lookup per
//! selected field.

#[cfg(all(not(feature = "std"), not(test)))]
use alloc::boxed::Box;
#[cfg(not(feature = "std"))]
use alloc::collections::BTreeMap;
#[cfg(all(not(feature = "std"), not(test)))]
use alloc::vec::Vec;
#[cfg(all(not(feature = "std"), not(test)))]
use alloc::{format, string::String};
use core::slice;
#[cfg(feature = "std")]
use std::collections::HashMap;

use crate::byte_record::ByteRecord;
use crate::engine::{hash_name, wide_mapping};
use crate::error::{Error, ErrorKind};
use crate::span::ResolvedSpans;
use crate::text_record::TextRecord;

/// A resolved positional field projection.
///
/// ```
/// use coseva::{ByteRecord, FieldProjection};
///
/// let headers = ByteRecord::from(vec![b"city".to_vec(), b"state".to_vec(), b"pop".to_vec()]);
/// let projection = FieldProjection::from_headers(&headers, ["pop", "city"])?;
///
/// let record = ByteRecord::from(vec![b"Boston".to_vec(), b"MA".to_vec(), b"650706".to_vec()]);
/// assert_eq!(
///     record.project(&projection).collect::<Vec<_>>(),
///     [Some(&b"650706"[..]), Some(&b"Boston"[..])],
/// );
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FieldProjection {
    indices: Box<[usize]>,
}

impl FieldProjection {
    /// Select fields by zero-based location.
    ///
    /// ```
    /// use coseva::{ByteRecord, FieldProjection};
    ///
    /// let projection = FieldProjection::new([2, 0]);
    /// let record = ByteRecord::from(vec![b"Boston".to_vec(), b"MA".to_vec(), b"650706".to_vec()]);
    /// assert_eq!(
    ///     record.project(&projection).collect::<Vec<_>>(),
    ///     [Some(&b"650706"[..]), Some(&b"Boston"[..])],
    /// );
    /// ```
    #[must_use]
    pub fn new(indices: impl Into<Box<[usize]>>) -> Self {
        Self {
            indices: indices.into(),
        }
    }

    /// Resolve names against a sequence of headers.
    ///
    /// The headers may come from any byte-like sequence, so `&ByteRecord`,
    /// `&TextRecord`, and a plain array of names all work.
    ///
    /// Each requested name must occur exactly once. Use positional projection
    /// when duplicate headers are intentional.
    ///
    /// One- and two-name projections over bounded header iterators scan
    /// directly, so they allocate only the returned projection.
    ///
    /// ```
    /// use coseva::{ByteRecord, FieldProjection};
    ///
    /// let headers = ["city", "state", "pop"];
    /// let projection = FieldProjection::from_headers(headers, ["pop", "city"])?;
    /// assert_eq!(projection.indices(), &[2, 0]);
    ///
    /// // A name that is not present is an error.
    /// assert!(FieldProjection::from_headers(headers, ["area"]).is_err());
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error for missing or duplicate requested headers.
    pub fn from_headers<H, N>(headers: H, names: N) -> Result<Self, Error>
    where
        H: IntoIterator,
        H::Item: AsRef<[u8]>,
        N: IntoIterator,
        N::Item: AsRef<[u8]>,
    {
        let headers = headers.into_iter();
        let mut names = names.into_iter();
        if let (Some(header_count), Some(name_count)) = (exact_bound(&headers), exact_bound(&names))
        {
            match name_count {
                0 => return Ok(Self::default()),
                1 if !wide_mapping(1, header_count) => {
                    let name = names
                        .next()
                        .expect("an iterator's exact lower bound must be valid");
                    let index = resolve_one_name(headers, name.as_ref())?;
                    return Ok(Self {
                        indices: Box::new([index]),
                    });
                }
                2 if !wide_mapping(2, header_count) => {
                    let first_name = names
                        .next()
                        .expect("an iterator's exact lower bound must be valid");
                    let second_name = names
                        .next()
                        .expect("an iterator's exact lower bound must be valid");
                    let indices =
                        resolve_two_names(headers, first_name.as_ref(), second_name.as_ref())?;
                    return Ok(Self {
                        indices: Box::new(indices),
                    });
                }
                _ => {}
            }
        }

        let headers_vec: Vec<_> = headers.collect();
        let names_vec: Vec<_> = names.collect();
        from_headers_bytes(&headers_vec, &names_vec)
    }

    /// Selected source positions.
    #[must_use]
    pub const fn indices(&self) -> &[usize] {
        &self.indices
    }

    /// Number of selected fields.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.indices.len()
    }

    /// Whether no fields are selected.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }
}

fn exact_bound<I: Iterator>(iter: &I) -> Option<usize> {
    let (lower, upper) = iter.size_hint();
    (upper == Some(lower)).then_some(lower)
}

fn from_headers_bytes<H, N>(headers: &[H], names: &[N]) -> Result<FieldProjection, Error>
where
    H: AsRef<[u8]>,
    N: AsRef<[u8]>,
{
    match resolution_strategy(headers, names) {
        ResolutionStrategy::Empty => Ok(FieldProjection::default()),
        ResolutionStrategy::One => {
            let index = resolve_one_name(headers.iter(), names[0].as_ref())?;
            Ok(FieldProjection {
                indices: Box::new([index]),
            })
        }
        ResolutionStrategy::Two => {
            let indices = resolve_two_names(headers.iter(), names[0].as_ref(), names[1].as_ref())?;
            Ok(FieldProjection {
                indices: Box::new(indices),
            })
        }
        ResolutionStrategy::Scan => Ok(FieldProjection::new(resolve_names_scan(headers, names)?)),
        ResolutionStrategy::Indexed => {
            Ok(FieldProjection::new(resolve_names_indexed(headers, names)?))
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResolutionStrategy {
    Empty,
    One,
    Two,
    Scan,
    Indexed,
}

fn resolution_strategy<H, N>(headers: &[H], names: &[N]) -> ResolutionStrategy {
    match names.len() {
        0 => ResolutionStrategy::Empty,
        1 if !wide_mapping(1, headers.len()) => ResolutionStrategy::One,
        2 if !wide_mapping(2, headers.len()) => ResolutionStrategy::Two,
        _ if wide_mapping(names.len(), headers.len()) => ResolutionStrategy::Indexed,
        _ => ResolutionStrategy::Scan,
    }
}

fn resolve_one_name<H>(headers: H, name: &[u8]) -> Result<usize, Error>
where
    H: Iterator,
    H::Item: AsRef<[u8]>,
{
    let mut found = None;
    for (index, header) in headers.enumerate() {
        if header.as_ref() == name && found.replace(index).is_some() {
            return Err(header_error(ErrorKind::DuplicateHeader, name));
        }
    }
    found.ok_or_else(|| header_error(ErrorKind::MissingHeader, name))
}

fn resolve_two_names<H>(
    headers: H,
    first_name: &[u8],
    second_name: &[u8],
) -> Result<[usize; 2], Error>
where
    H: Iterator,
    H::Item: AsRef<[u8]>,
{
    let mut first = None;
    let mut second = None;
    let mut first_duplicate = false;
    let mut second_duplicate = false;
    for (index, header) in headers.enumerate() {
        let header = header.as_ref();
        if header == first_name && first.replace(index).is_some() {
            first_duplicate = true;
        }
        if header == second_name && second.replace(index).is_some() {
            second_duplicate = true;
        }
    }

    Ok([
        resolved_name(first, first_duplicate, first_name)?,
        resolved_name(second, second_duplicate, second_name)?,
    ])
}

fn resolved_name(index: Option<usize>, duplicate: bool, name: &[u8]) -> Result<usize, Error> {
    if duplicate {
        Err(header_error(ErrorKind::DuplicateHeader, name))
    } else {
        index.ok_or_else(|| header_error(ErrorKind::MissingHeader, name))
    }
}

fn header_error(kind: ErrorKind, name: &[u8]) -> Error {
    let qualifier = match kind {
        ErrorKind::DuplicateHeader => "duplicate",
        _ => "missing",
    };
    Error::detailed(
        kind,
        format!(
            "{qualifier} projected header {:?}",
            String::from_utf8_lossy(name)
        ),
    )
}

/// Resolve each requested name by scanning the headers once per name.
///
/// Linear in `names × headers` and allocation-free, the right choice for a
/// sparse projection. Each name must occur exactly once: no match is a missing
/// header and more than one is a duplicate.
fn resolve_names_scan<H, N>(headers: &[H], names: &[N]) -> Result<Vec<usize>, Error>
where
    H: AsRef<[u8]>,
    N: AsRef<[u8]>,
{
    let mut indices = Vec::with_capacity(names.len());
    for name in names {
        let name = name.as_ref();
        let mut matches = headers
            .iter()
            .enumerate()
            .filter_map(|(index, header)| (header.as_ref() == name).then_some(index));
        let first = matches
            .next()
            .ok_or_else(|| header_error(ErrorKind::MissingHeader, name))?;
        if matches.next().is_some() {
            return Err(header_error(ErrorKind::DuplicateHeader, name));
        }
        indices.push(first);
    }
    Ok(indices)
}

/// Resolve each requested name through a temporary header lookup, for wide
/// projections where scanning every header per name is quadratic.
///
/// Produces the identical result to [`resolve_names_scan`]: the bucket holds
/// every column whose name hashes alike, in ascending order, and the byte
/// comparison filters out any hash collision, so the missing and duplicate
/// rules are exactly the scan's. The hashing is the engine's own header hasher,
/// so the two paths agree on how a name is keyed.
fn resolve_names_indexed<H, N>(headers: &[H], names: &[N]) -> Result<Vec<usize>, Error>
where
    H: AsRef<[u8]>,
    N: AsRef<[u8]>,
{
    #[cfg(feature = "std")]
    let mut buckets: HashMap<u64, Vec<usize>> = HashMap::with_capacity(headers.len());
    #[cfg(not(feature = "std"))]
    let mut buckets: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (index, header) in headers.iter().enumerate() {
        buckets
            .entry(hash_name(header.as_ref()))
            .or_default()
            .push(index);
    }

    let mut indices = Vec::with_capacity(names.len());
    for name in names {
        let name = name.as_ref();
        let mut matches = buckets
            .get(&hash_name(name))
            .into_iter()
            .flatten()
            .copied()
            .filter(|&index| headers[index].as_ref() == name);
        let first = matches
            .next()
            .ok_or_else(|| header_error(ErrorKind::MissingHeader, name))?;
        if matches.next().is_some() {
            return Err(header_error(ErrorKind::DuplicateHeader, name));
        }
        indices.push(first);
    }
    Ok(indices)
}

/// Where a byte projection reads its fields from.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ByteSource<'record> {
    /// A lending record, held as its parts so the yielded fields keep the
    /// lifetime of the parsed data rather than that of the record value.
    Spans(ResolvedSpans<'record>),
    /// An owned record, whose fields live as long as the borrow of it.
    Record(&'record ByteRecord),
}

impl<'record> ByteSource<'record> {
    fn get(self, index: usize) -> Option<&'record [u8]> {
        match self {
            Self::Spans(spans) => spans.get(index),
            Self::Record(record) => record.get(index),
        }
    }
}

/// Iterator over the byte fields selected by a [`FieldProjection`].
///
/// Yields one item per selected position, in projection order. A position past
/// the end of the record yields `None`, so short records never silently shift
/// the remaining fields.
/// For a worked example, see [`FieldProjection`].
#[derive(Clone, Debug)]
pub struct ProjectedFields<'projection, 'record> {
    indices: slice::Iter<'projection, usize>,
    source: ByteSource<'record>,
}

impl<'projection, 'record> ProjectedFields<'projection, 'record> {
    pub(crate) fn new(
        projection: &'projection FieldProjection,
        source: ByteSource<'record>,
    ) -> Self {
        Self {
            indices: projection.indices.iter(),
            source,
        }
    }
}

impl<'record> Iterator for ProjectedFields<'_, 'record> {
    type Item = Option<&'record [u8]>;

    // gamma::skip(fn_value.some, reason = "returning a value without consuming an index made collection unbounded and exceeded the memory limit")
    fn next(&mut self) -> Option<Self::Item> {
        let &index = self.indices.next()?;
        Some(self.source.get(index))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.indices.size_hint()
    }
}

impl DoubleEndedIterator for ProjectedFields<'_, '_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let &index = self.indices.next_back()?;
        Some(self.source.get(index))
    }
}

impl ExactSizeIterator for ProjectedFields<'_, '_> {}

/// Iterator over the string fields selected by a [`FieldProjection`].
///
/// Yields one item per selected position, in projection order. A position past
/// the end of the record yields `None`, so short records never silently shift
/// the remaining fields.
/// For a worked example, see [`FieldProjection`].
#[derive(Clone, Debug)]
pub struct ProjectedTextFields<'projection, 'record> {
    indices: slice::Iter<'projection, usize>,
    record: &'record TextRecord,
}

impl<'projection, 'record> ProjectedTextFields<'projection, 'record> {
    pub(crate) fn new(
        projection: &'projection FieldProjection,
        record: &'record TextRecord,
    ) -> Self {
        Self {
            indices: projection.indices.iter(),
            record,
        }
    }
}

impl<'record> Iterator for ProjectedTextFields<'_, 'record> {
    type Item = Option<&'record str>;

    // gamma::skip(fn_value.some, reason = "returning a value without consuming an index made collection unbounded and exceeded the memory limit")
    fn next(&mut self) -> Option<Self::Item> {
        let &index = self.indices.next()?;
        Some(self.record.get(index))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.indices.size_hint()
    }
}

impl DoubleEndedIterator for ProjectedTextFields<'_, '_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let &index = self.indices.next_back()?;
        Some(self.record.get(index))
    }
}

impl ExactSizeIterator for ProjectedTextFields<'_, '_> {}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_strategy_holds_each_exact_width_boundary() {
        let strategy = |header_count: usize, name_count: usize| {
            let headers = vec![b"header".as_slice(); header_count];
            let names = vec![b"name".as_slice(); name_count];
            resolution_strategy(&headers, &names)
        };
        assert_eq!(strategy(2, 0), ResolutionStrategy::Empty);
        assert_eq!(strategy(1_024, 1), ResolutionStrategy::One);
        assert_eq!(strategy(1_025, 1), ResolutionStrategy::Indexed);
        assert_eq!(strategy(512, 2), ResolutionStrategy::Two);
        assert_eq!(strategy(513, 2), ResolutionStrategy::Indexed);
        assert_eq!(strategy(341, 3), ResolutionStrategy::Scan);
        assert_eq!(strategy(342, 3), ResolutionStrategy::Indexed);
    }

    #[test]
    fn test_projection_edge_cases() {
        // empty names
        let empty = FieldProjection::from_headers(["a", "b"], Vec::<&str>::new()).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);

        // 1 name missing & duplicate
        assert!(FieldProjection::from_headers(["a", "b"], ["c"]).is_err());
        assert!(FieldProjection::from_headers(["a", "a"], ["a"]).is_err());

        // 2 names with duplicates & missing in resolve_two_names
        assert!(FieldProjection::from_headers(["a", "a", "b"], ["a", "b"]).is_err());
        assert!(FieldProjection::from_headers(["a", "b", "b"], ["a", "b"]).is_err());
        assert!(FieldProjection::from_headers(["a", "b"], ["a", "c"]).is_err());
        assert!(FieldProjection::from_headers(["a", "b"], ["c", "b"]).is_err());
        assert!(FieldProjection::from_headers(["a", "b"], ["c", "d"]).is_err());

        // Wide mapping (indexed): wide_mapping(names.len(), headers.len())
        // wide_mapping threshold is names.len() * headers.len() >= 64
        let wide_headers: Vec<String> = (0..20).map(|i| format!("col_{i}")).collect();
        let wide_names: Vec<String> = (0..10).map(|i| format!("col_{i}")).collect();
        let p_wide = FieldProjection::from_headers(&wide_headers, &wide_names).unwrap();
        assert_eq!(p_wide.len(), 10);

        // Wide mapping indexed error paths (missing and duplicate)
        let mut wide_missing_names = wide_names.clone();
        wide_missing_names.push("missing_col".to_string());
        assert!(FieldProjection::from_headers(&wide_headers, &wide_missing_names).is_err());

        let mut wide_dup_headers = wide_headers.clone();
        wide_dup_headers.push("col_0".to_string());
        assert!(FieldProjection::from_headers(&wide_dup_headers, &wide_names).is_err());
        struct UnboundedIter<I>(I);
        impl<I: Iterator> Iterator for UnboundedIter<I> {
            type Item = I::Item;
            fn next(&mut self) -> Option<Self::Item> {
                self.0.next()
            }
            fn size_hint(&self) -> (usize, Option<usize>) {
                (0, None)
            }
        }

        let p1 =
            FieldProjection::from_headers(UnboundedIter(["a", "b"].into_iter()), ["a"]).unwrap();
        assert_eq!(p1.indices(), &[0]);
        let p2 = FieldProjection::from_headers(UnboundedIter(["a", "b"].into_iter()), ["a", "b"])
            .unwrap();
        assert_eq!(p2.indices(), &[0, 1]);
        let p3 = FieldProjection::from_headers(
            UnboundedIter(["a", "b", "c"].into_iter()),
            UnboundedIter(["a", "b", "c"].into_iter()),
        )
        .unwrap();
        assert_eq!(p3.indices(), &[0, 1, 2]);

        // Wide mapping with 1 or 2 names on unbounded/collected headers
        let wide_unbounded_headers: Vec<String> = (0..1050).map(|i| format!("col_{i}")).collect();
        let p_wide_1 =
            FieldProjection::from_headers(UnboundedIter(wide_unbounded_headers.iter()), ["col_5"])
                .unwrap();
        assert_eq!(p_wide_1.indices(), &[5]);
        let p_wide_2 = FieldProjection::from_headers(
            UnboundedIter(wide_unbounded_headers.iter()),
            ["col_5", "col_6"],
        )
        .unwrap();
        assert_eq!(p_wide_2.indices(), &[5, 6]);

        // DoubleEndedIterator and size_hint on ProjectedFields and ProjectedTextFields
        let br = ByteRecord::from(vec![b"a".to_vec(), b"b".to_vec()]);
        let proj = FieldProjection::new([0, 1]);
        let mut pf = br.project(&proj);
        assert_eq!(pf.size_hint(), (2, Some(2)));
        assert_eq!(pf.next_back(), Some(Some(&b"b"[..])));
        assert_eq!(pf.next(), Some(Some(&b"a"[..])));
        assert_eq!(pf.next(), None);

        let tr = TextRecord::from(vec!["a".to_string(), "b".to_string()]);
        let mut ptf = tr.project(&proj);
        assert_eq!(ptf.size_hint(), (2, Some(2)));
        assert_eq!(ptf.next_back(), Some(Some("b")));
        assert_eq!(ptf.next(), Some(Some("a")));
        assert_eq!(ptf.next(), None);

        // 3+ names: resolve_names_scan (missing and duplicate)
        assert!(FieldProjection::from_headers(["a", "b", "c"], ["a", "b", "d"]).is_err());
        assert!(FieldProjection::from_headers(["a", "b", "c", "c"], ["a", "b", "c"]).is_err());

        // Wide mapping (indexed): wide_mapping(names.len(), headers.len())
        // wide_mapping threshold is names.len() * headers.len() >= 64
        let wide_headers: Vec<String> = (0..20).map(|i| format!("col_{i}")).collect();
        let wide_names: Vec<String> = (0..10).map(|i| format!("col_{i}")).collect();
        let p_wide = FieldProjection::from_headers(&wide_headers, &wide_names).unwrap();
        assert_eq!(p_wide.len(), 10);

        // resolve_names_collected error paths (1 and 2 names with unbounded iterator)
        assert!(
            FieldProjection::from_headers(UnboundedIter(["a", "b"].into_iter()), ["c"]).is_err()
        );
        assert!(
            FieldProjection::from_headers(UnboundedIter(["a", "a"].into_iter()), ["a"]).is_err()
        );
        assert!(
            FieldProjection::from_headers(UnboundedIter(["a", "b"].into_iter()), ["a", "c"])
                .is_err()
        );
        assert!(
            FieldProjection::from_headers(UnboundedIter(["a", "a", "b"].into_iter()), ["a", "b"])
                .is_err()
        );
        assert!(
            FieldProjection::from_headers(
                UnboundedIter(wide_unbounded_headers.iter()),
                ["nonexistent"]
            )
            .is_err()
        );
        assert!(
            FieldProjection::from_headers(
                UnboundedIter(wide_unbounded_headers.iter()),
                ["col_0", "nonexistent"]
            )
            .is_err()
        );
        let mut wide_unb_dup = wide_unbounded_headers.clone();
        wide_unb_dup.push("col_0".to_string());
        assert!(
            FieldProjection::from_headers(UnboundedIter(wide_unb_dup.iter()), ["col_0"]).is_err()
        );
        assert!(
            FieldProjection::from_headers(UnboundedIter(wide_unb_dup.iter()), ["col_0", "col_1"])
                .is_err()
        );

        // ByteSource::Spans test via Record::project
        let mut parser = crate::SliceParser::<crate::format::Csv>::new(
            b"a,b\n1,2\n",
            crate::config::ParseOptions::new().headers(crate::config::Headers::None),
        )
        .unwrap();
        let mut line = parser.next_line().unwrap().unwrap();
        let rec = line.record().unwrap();
        let mut p_spans = rec.project(&proj);
        assert_eq!(p_spans.next(), Some(Some(&b"a"[..])));
        assert_eq!(p_spans.next(), Some(Some(&b"b"[..])));
        assert_eq!(p_spans.next(), None);
    }
}
