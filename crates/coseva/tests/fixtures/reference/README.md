# Reference corpora (TODO T9)

This directory documents the provenance of the licensed external CSV test
cases exercised by `../../reference_corpora.rs`. It contains no vendored
source code -- only a provenance manifest and the upstream license texts
that cover the small, individually attributed snippets of test data adapted
into that Rust file.

## Contents

- `manifest.json` -- one entry per adopted case: upstream source, exact URL,
  revision (commit SHA), license/SPDX identifier, the upstream case name,
  a description of any adaptation, the coseva format profile used, and the
  expected outcome (rows or rejection). This file is human/tooling-readable
  documentation; it is **not** parsed by the test harness. The actual input
  bytes and expected results live as Rust literals in `reference_corpora.rs`
  itself, so byte-exact data -- including intentionally invalid UTF-8 -- never
  has to round-trip through JSON text.
- `licenses/UNLICENSE-rust-csv.txt` -- verbatim Unlicense text from
  BurntSushi/rust-csv.
- `licenses/LICENSE-BSD-3-golang.txt` -- verbatim BSD-3-Clause text from the
  Go project (covers `encoding/csv`).

## Sources used

1. **BurntSushi/rust-csv** (Unlicense, dual-licensed with MIT) -- per-case
   unit tests embedded in `src/reader.rs`'s `mod tests`, e.g.
   `read_byte_record`, `read_trimmed_records*`, `read_record_unequal_*`,
   `headers_on_empty_data`. 11 cases adapted, prefixed `rust_csv/` in the
   manifest and in the Rust test names.
2. **Go standard library, `encoding/csv`** (BSD-3-Clause) -- the `readTests`
   table in `src/encoding/csv/reader_test.go`. 34 cases adapted, prefixed
   `go/`. Go's table annotates its `Input` string with position-marker
   glyphs (`§` marks a field start, `¶` a record boundary, `∑` an error
   offset) that its own test driver strips before parsing and uses only to
   check reported line/column positions that coseva does not expose in the
   same shape; those glyphs are not CSV data and are removed from every
   `go/` case here. This is noted once, rather than per case, in
   `manifest.json`.

Both sources are independently maintained, still-active upstream projects
with unambiguous open-source licenses compatible with inclusion here, and
each is small, individually attributed, and (for the byte-for-byte cases)
directly traceable back to a specific upstream commit and test name.

### Sources deliberately not used

- **CSV Spectrum** -- its repository invites reuse but ships no license
  file, so no cases were vendored or adapted from it pending upstream
  license clarification.
- **CPython `test_csv.py`** (PSF-2.0) -- marked optional by the T9 backlog
  item; left out so the corpus draws from two independently maintained
  sources without duplicating coverage the Go and rust-csv cases already
  provide.
- **W3C CSVW** -- transformation-focused (CSV-to-JSON/RDF metadata) rather
  than parser-conformance-focused, and marked optional by the T9 backlog
  item; out of scope for a parser/emitter reference corpus.

Within the two sources actually used, a further handful of upstream cases
were excluded to keep the corpus compact rather than exhaustive: Go's
"lazy quotes" family (`LazyQuotes`, `BareQuotes`, `BareDoubleQuotes`,
`LazyQuoteWithTrailingCRLF`, `LazyOddQuotes` -- Go's single `LazyQuotes`
switch does not map cleanly onto coseva's more granular `Recovery` flags),
its non-ASCII multi-rune delimiter/comment cases (coseva's default
delimiter/comment are single bytes; multi-byte support is a separate,
non-default feature with different construction rules), its `HugeLines`
synthetic performance case (not a correctness case), and most of its
repetitive CR-at-end-of-input variants beyond the two or three representative
cases kept here (`trailing_cr`, `quoted_trailing_cr`,
`quoted_trailing_cr_cr`), which already establish the relevant behavior.

## Distinguishing real differences from failures

Several adopted cases intentionally assert a **different** result than the
upstream implementation produces for the same bytes, because coseva makes a
different, deliberate design choice. Each such case sets
`semantic_difference` in `manifest.json` and its Rust test asserts coseva's
*actual* behavior, with a comment explaining the divergence. These are not
bugs or test failures -- they are the documented boundary of the two
implementations' behavior for identical input:

- **Embedded `\r\n` inside a quoted field is not normalized.** Go's reader
  rewrites an embedded CRLF (or bare CR) inside a quoted field to `\n`;
  coseva returns the bytes unchanged.
- **A bare trailing CR at end-of-input is retained as data**, not treated as
  an implicit record terminator. Go strips it; coseva's
  `RecordEnding::Newline` only strips a CR that is immediately followed by
  an LF.
- **A bare CR immediately after a closing quote at end-of-input is
  rejected** (`UnexpectedByteAfterQuote`). Go accepts and strips it.
- **A field-count mismatch poisons the rest of the parse.** Go and rust-csv
  treat a wrong-width record as a per-record, non-fatal error and keep
  yielding subsequent records; coseva's parser reports the mismatch and then
  fails every subsequent `next_line()` call with `ErrorKind::ParserFailed`.
  Only the first mismatch is exercised for this reason.
- **`headers()` on empty input returns `Ok(None)`**, i.e. no header row was
  ever read; rust-csv's `byte_headers()` instead returns an empty-but-present
  zero-field record for the same input.
- **Whitespace trimming trims both ends of a field.** Go's
  `TrimLeadingSpace` trims only the leading edge. The one Go case that
  exercises this (`TrimSpace`) happens to contain no trailing whitespace, so
  it cannot distinguish the two policies -- the manifest calls this out as a
  coincidental match, not a general equivalence.

A couple of cases converge on rejection through different named error
paths (`OddQuotes`, `QuotedTrailingCRCR`); these are noted as agreement
despite the differing error identity, and are not treated as semantic
differences.

## Format profiles

`manifest.json`'s `format_profiles` map gives, for each profile name used by
a case, the exact `FormatOptions`/`ParseOptions` construction used in
`reference_corpora.rs`. Cases whose profile is built entirely from one of
coseva's built-in static formats (`Csv`, `Semicolon`, `CommentedCsv`,
`TrimmedCsv`, ...) are additionally run through that `StaticFormat` type, in
addition to the dynamic `FormatOptions`/`ParseOptions` path, by the test
harness.
