use crate::config::Dialect;
use crate::error::Location;
use crate::search::StructuralBlocks;

/// A record boundary, carrying everything a parser needs to resume there.
///
/// The line and record counters are what let a worker report absolute
/// positions: it seeks to `byte` with these counters restored, so the offsets,
/// physical lines and record indices it reports are the ones a serial parse of
/// the whole document would have reported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Boundary {
    pub(crate) byte: usize,
    pub(crate) line: u64,
    pub(crate) record: u64,
}

impl From<Location> for Boundary {
    fn from(location: Location) -> Self {
        Self {
            byte: location.byte,
            line: location.line,
            record: location.record,
        }
    }
}

impl From<Boundary> for Location {
    fn from(boundary: Boundary) -> Self {
        Self {
            byte: boundary.byte,
            line: boundary.line,
            record: boundary.record,
            field: 0,
        }
    }
}

/// Locate up to `wanted` record boundaries at or after `start`.
///
/// The returned vector always begins with `start`, so its length is the number
/// of chunks and consecutive entries delimit one each. Boundaries land only at
/// true record starts: the scan tracks whether it is inside a quoted field and
/// splits only on a record ending seen outside one.
///
/// This is the serial fraction that bounds the speedup, so it does as little as
/// it can get away with. It visits only the bytes a SIMD scan reports --
/// quotes, record endings, and line feeds -- and per hit does no more than
/// toggle a flag and bump two counters. It locates no fields and assembles no
/// records, which is the point: field spans are most of the cost of parsing,
/// and computing them here would serialize the work the threads exist to
/// spread out.
pub(crate) fn boundaries(
    input: &[u8],
    dialect: Dialect,
    start: Boundary,
    wanted: usize,
) -> Vec<Boundary> {
    let mut splits = Vec::with_capacity(wanted);
    splits.push(start);
    let Some(span) = input.len().checked_sub(start.byte) else {
        return splits;
    };

    // A stride in bytes rather than a count of records, because the record
    // count is not known until the scan has run, and bytes are what needs
    // balancing anyway.
    let stride = span.div_ceil(wanted.max(1));
    let quote = dialect.quote;
    let ending = dialect.record_ending.byte();

    let mut in_quotes = false;
    let mut line = start.line;
    let mut record = start.record;
    let mut target = start.byte.saturating_add(stride);

    for mut block in StructuralBlocks::new(&input[start.byte..], quote, ending, b'\n') {
        while let Some((offset, byte)) = block.next_match() {
            let at = start.byte + offset;
            if byte == quote {
                in_quotes = !in_quotes;
            }
            if byte == b'\n' {
                // Physical lines count every line feed, including those inside
                // quoted fields, which is what the parser's own numbering does.
                line += 1;
            }
            if byte == ending && !in_quotes {
                record += 1;
                let next = at + 1;
                if next >= target && next < input.len() {
                    splits.push(Boundary {
                        byte: next,
                        line,
                        record,
                    });
                    target = next.saturating_add(stride);
                }
            }
        }
    }

    splits
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::{Boundary, boundaries};
    use crate::config::Dialect;

    const START: Boundary = Boundary {
        byte: 0,
        line: 1,
        record: 0,
    };

    /// Every position a record may legally begin at, found the slow way.
    ///
    /// This is the oracle the splitter is checked against: a byte-at-a-time
    /// walk that knows where the quoted regions are.
    fn record_starts(input: &[u8]) -> Vec<usize> {
        let mut starts = vec![0];
        let mut in_quotes = false;
        for (at, &byte) in input.iter().enumerate() {
            if byte == b'"' {
                in_quotes = !in_quotes;
            } else if byte == b'\n' && !in_quotes && at + 1 < input.len() {
                starts.push(at + 1);
            }
        }
        starts
    }

    /// A document whose quoted field holds a delimiter, a record ending and a
    /// doubled quote, so a boundary landing anywhere inside it would split a
    /// field, a record, or an escape in half.
    const HOSTILE: &[u8] = b"a,b\n\"x,y\nz\"\"w\",c\nd,e\n\"p\nq\",r\ns,t\n";

    #[test]
    fn a_boundary_never_lands_inside_a_quoted_field() {
        let legal = record_starts(HOSTILE);
        // Sweeping the requested count sweeps the stride over every byte
        // position, so the scan is asked to split at each byte of the quoted
        // fields in turn.
        for wanted in 1..=HOSTILE.len() * 2 {
            for split in boundaries(HOSTILE, Dialect::CSV, START, wanted) {
                assert!(
                    legal.contains(&split.byte),
                    "wanted {wanted} split at {} which is not a record start",
                    split.byte
                );
            }
        }
    }

    #[test]
    #[expect(
        clippy::naive_bytecount,
        reason = "an oracle for a fast scan should be the slow obvious thing, and these documents are tiny"
    )]
    fn boundaries_carry_the_line_and_record_a_serial_parse_would_have() {
        let splits = boundaries(HOSTILE, Dialect::CSV, START, HOSTILE.len());
        for split in splits {
            let prefix = &HOSTILE[..split.byte];
            let lines = u64::try_from(prefix.iter().filter(|&&byte| byte == b'\n').count())
                .expect("a test document short enough to count");
            assert_eq!(split.line, 1 + lines, "line at {}", split.byte);
            assert_eq!(
                split.record,
                u64::try_from(
                    record_starts(HOSTILE)
                        .iter()
                        .filter(|&&start| start <= split.byte && start > 0)
                        .count()
                )
                .expect("a test document short enough to count"),
                "record at {}",
                split.byte
            );
        }
    }

    #[test]
    fn boundaries_are_ordered_and_start_where_asked() {
        let start = Boundary {
            byte: 4,
            line: 2,
            record: 1,
        };
        let splits = boundaries(HOSTILE, Dialect::CSV, start, 4);
        assert_eq!(splits[0], start);
        assert!(splits.windows(2).all(|pair| pair[0].byte < pair[1].byte));
        assert!(splits.iter().all(|split| split.byte < HOSTILE.len()));
    }

    #[test]
    fn a_document_with_nothing_to_split_yields_one_chunk() {
        assert_eq!(boundaries(b"", Dialect::CSV, START, 8).len(), 1);
        assert_eq!(boundaries(b"a,b", Dialect::CSV, START, 8).len(), 1);
        assert_eq!(boundaries(b"a,b\n", Dialect::CSV, START, 8).len(), 1);
    }

    fn slow_boundaries(input: &[u8], start: Boundary, wanted: usize) -> Vec<Boundary> {
        let mut splits = vec![start];
        let Some(tail) = input.get(start.byte..) else {
            return splits;
        };
        if tail.is_empty() {
            return splits;
        }

        let stride = tail.len().div_ceil(wanted.max(1)).max(1);
        let mut in_quotes = false;
        let mut line = start.line;
        let mut record = start.record;
        let mut target = start.byte.saturating_add(stride);
        for (offset, &byte) in tail.iter().enumerate() {
            let at = start.byte + offset;
            if byte == b'"' {
                in_quotes = !in_quotes;
            }
            if byte == b'\n' {
                line += 1;
            }
            if byte == b'\n' && !in_quotes {
                record += 1;
                let next = at + 1;
                if next >= target && next < input.len() {
                    splits.push(Boundary {
                        byte: next,
                        line,
                        record,
                    });
                    target = next.saturating_add(stride);
                }
            }
        }
        splits
    }

    #[test]
    fn requested_chunk_counts_match_the_bytewise_oracle_exactly() {
        for wanted in 0..=HOSTILE.len() * 2 {
            assert_eq!(
                boundaries(HOSTILE, Dialect::CSV, START, wanted),
                slow_boundaries(HOSTILE, START, wanted),
                "wanted {wanted}"
            );
        }

        let at_end = Boundary {
            byte: HOSTILE.len(),
            line: 7,
            record: 5,
        };
        assert_eq!(boundaries(HOSTILE, Dialect::CSV, at_end, 4), [at_end]);
        let beyond = Boundary {
            byte: HOSTILE.len() + 1,
            ..at_end
        };
        assert_eq!(boundaries(HOSTILE, Dialect::CSV, beyond, 4), [beyond]);
    }
}
