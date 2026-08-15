# coseva design

This document explains how `coseva` works as a machine: the algorithms it
runs, why those algorithms were chosen, what shape the data takes at each
stage, where the fast paths diverge from the slow ones, and what
invariants keep the whole thing correct. It is not a tour of the source
tree; type and function names appear only in service of explaining a
mechanism.

For SIMD structural scanning specifically: this document explains the
current design, including how small the remaining `unsafe` footprint is
and why the scan is unusually sensitive to inlining.

## Table of contents

1. [One record's journey](#one-records-journey)
2. [SIMD structural scanning](#simd-structural-scanning)
3. [Parsing kernels: the fast/slow split](#parsing-kernels-the-fastslow-split)
4. [Zero-copy representation and copy-on-escape](#zero-copy-representation-and-copy-on-escape)
5. [Buffer management for streaming](#buffer-management-for-streaming)
6. [Header handling and the `headers_initialized` invariant](#header-handling-and-the-headers_initialized-invariant)
7. [Typed decoding](#typed-decoding)
8. [Predicate pushdown](#predicate-pushdown)
9. [Encoding](#encoding)
10. [The random-access index](#the-random-access-index)
11. [`no_std` and feature architecture](#no_std-and-feature-architecture)
12. [Error model and poisoning](#error-model-and-poisoning)
13. [Proc-macro architecture](#proc-macro-architecture)
14. [API guideline departures](#api-guideline-departures)
15. [Test coverage and what is deliberately not covered](#test-coverage-and-what-is-deliberately-not-covered)

## One record's journey

A CSV parser's cost is dominated by how much of the input it touches more
than once, so coseva structures the pipeline to touch escaped bytes once
and everything else zero times before a caller asks for it:

```
 raw bytes (input slice or read window)
        │
        ▼
 structural scan  ── SIMD 32-byte blocks find candidate delimiter/quote/
        │             terminator bytes; scalar loop for tails & fallback
        ▼
 field boundary discovery ── candidate positions become field spans,
        │                    applying quoting/escaping rules
        ▼
 borrowed record (`Record`)  ── `Span`s pointing into the input, or into a
        │                       small scratch buffer for unescaped text
        ├──────────────► optional owned materialization (`ByteRecord`) —
        │                 copies every field once into one owned buffer
        ▼
 optional typed decode ── `FromBytes`/`CsvDecode`/serde turn borrowed or
                           owned bytes into `T`
```

The state machine driving every parser (`SliceParser`, `IoParser`,
`PushParser`) holds no bytes itself; the window is passed in on each call,
letting the streaming/push parsers keep the bytes in a sibling field that
resizes independently of the parsing state.

For a record with no escapes, nothing is copied before a field is read: a
`Span { start, end }` is recorded into the input slice, and reading a
field is a slice index. Only an escaped quote inside a quoted field (the
decoded bytes are literally shorter than the source, so no sub-range of
the input can represent them) or a record whose bytes aren't contiguous
at inspection time — chiefly one straddling a streaming buffer refill —
forces a copy; both are covered in detail below.

If a caller only wants a subset of columns, the scan still discovers
every field and the target's mapping selects among them afterwards. That
sounds wasteful and is not: a discovered field costs a `Span`, not a copy,
so [there is nothing to save](#why-there-is-no-projected-kernel) by fusing
column selection into the scan.

## SIMD structural scanning

The primitives answer one question — "where is the next byte that
matters?", where "matters" means one of a small fixed set of structural
bytes (delimiter, quote, record terminator) — via two shapes: single/multi
-needle searches (`find1`/`find2`/`find3_near`, `rfind1`, `count1`) and a
block-oriented iterator (`StructuralBlocks`) that walks *every* structural
byte in a region.

**The needle searches** share one generic core parameterized by needle
count and by how many leading bytes get a plain scalar check before
dropping into SIMD. That scalar prefix exists because CSV fields are
usually short: unconditionally paying for a 32-byte AVX2 load and three
vector compares can cost more than walking a handful of bytes by hand.
`find1`/`find2` use no scalar prefix; the `_near` variants used inside a
record's hot loop use an 8-byte prefix on x86, a concession that most real
fields are shorter than one AVX2 block. Once the prefix is exhausted, the
AVX2 loop loads 32 bytes, compares against up to three broadcast needle
bytes with `vpcmpeqb`, ORs the results, and turns the vector predicate
into a bitmask with `vpmovmskb`; the first set bit is the answer. NEON
covers the same primitives on `aarch64`. A byte-for-byte identical scalar
loop handles inputs shorter than a block, architectures with neither
instruction set, and residual tail bytes — it is not a behavior-changing
fallback, just the same predicate evaluated one byte at a time, which is
why differential tests hold every SIMD entry point to agreement with a
naive scalar reference at lengths clustered around the 32-byte boundary
(the classic place for tail off-by-ones to hide).

**`StructuralBlocks`** divides the input into fixed 32-byte blocks
anchored to `input[0]` — not to wherever the caller starts scanning — and
computes, per block, a `u32` bitmask where bit `i` means byte `i` matched
one of up to three needles. `trailing_zeros`/`mask &= mask - 1` extraction
lets a consumer walk from one structural byte to the next inside an
already-scanned block without touching memory again. The block-mask
implementation is SSE2/NEON only (not AVX2): it loads the block's low and
high 16 bytes separately, compares each half against up to three broadcast
needles, ORs the per-needle results, and packs each half's 16-bit
`movemask` (x86) or weighted-lane-sum reduction (NEON, which has no direct
`movemask` equivalent) into the low/high halves of the `u32`.

Anchoring the block grid to the whole input, rather than to the caller's
start position, is what makes the resumable `BlockCache` sound: a block's
mask is a pure function of its 32 bytes and the three needle bytes, both
fixed for the parser's life, so a fully computed block never goes stale.
When a fast-path kernel bails out mid-block (hits a quote and hands off to
the general parser) and a later scan resumes inside that same block,
`StructuralBlocks::resume` reuses the cached mask instead of recomputing
it — sound only because the grid alignment is always identical. `resume`
also takes a `skip_below` position so a cached block's already-consumed
leading bits are masked off (`mask &= !((1 << skipped) - 1)`); only whole
32-byte blocks are ever cached, since a short tail block's valid mask
depends on where that particular scan was allowed to stop.

Runtime dispatch is a single `is_x86_feature_detected!("avx2")` check
(gated on `std`; without it the crate falls back to the compile-time
`cfg!(target_feature = "avx2")`, since runtime detection needs OS/libc
support), checked once per call into the AVX2 path. NEON is used
unconditionally on `aarch64` as part of that architecture's baseline, the
same way SSE2 is treated as baseline on `x86_64`.

**How little of this is `unsafe`.** The vector code is written as safe
Rust wherever the language allows. Blocks are fed to the kernels through
`as_chunks::<32>()` and `split_first_chunk::<32>()`, so the scan carries
no raw-pointer arithmetic and no hand-written "these 32 bytes are
readable" proofs; on x86 even the loads are pointer-free, built by
listing the 32 lanes explicitly, which LLVM folds back into a single
`vmovdqu`. What is left is only the irreducible part: the runtime
feature-detection boundaries — calling a `#[target_feature]` function is
unsafe unless the caller declares the same feature — plus, on `aarch64`
only, the `vld1q_u8` load, because every pointer-free NEON alternative
costs an extra instruction. That is three `unsafe` sites per
architecture, each a one-line "the CPU has this feature" assertion.

A consequence worth knowing before editing this code: the block scan's
performance depends on it being *inlined* into the record parser far more
than on the instructions it emits. The chain from the record parser down
through the scanner, the block iterator and the block-mask kernel must
collapse into a single function; when it does not, parsing costs 10-36%
more instructions even though the hot loop's codegen is unchanged. The
`#[inline]` attributes along that chain are therefore load-bearing and
were chosen by measurement, not by habit.

`rfind1` is a backward search used by [predicate pushdown](#predicate-pushdown)
to find the start of a record containing an already-located forward match.
Rather than scanning all preceding input, it starts with a 128-byte window
and quadruples it on each miss (128 → 512 → 2048 → ...), bounding the
average cost near the true record length while still terminating with
`None` if nothing precedes the search origin.

## Parsing kernels: the fast/slow-path split

Every configuration is resolved once, at construction, into either a
specialized whole-record kernel or the general byte-by-byte state
machine. Two independent predicates decide this, and understanding
exactly what defeats each fast path is the single most
performance-relevant fact about the crate.

### The owned-record kernels

`owned_parser_for` selects a specialized function pointer only when
*every one* of these holds: `Limits == Limits::DEFAULT` (the kernels
hard-code the default limits as compile-time constants so their bounds
checks fold away or are provably elided — a custom limit has nowhere to
plug in); `FieldCount == Flexible` (`Exact`/`MatchFirst` need mid-scan
validation the kernel has no hook for); `Whitespace == NONE` (trimming
needs a post-processing step the kernel doesn't have); `BlankRecords ==
Preserve` (skipping blanks is a decision the kernel never makes — it
always emits whatever it scanned); `Syntax == Strict` (looser syntax
changes what counts as a structural transition); `!skip_initial_space`
(leading-space skipping is a lookahead before deciding if a field is
quoted); and `Nulls == None` (NULL-marker comparison is exactly the kind
of per-field branch the kernel avoids).

If all hold, the kernel is chosen by `Dialect`: `Dialect::CSV` selects
`try_parse_default_record` (comma/double-quote/`\n` only, standard
doubled-quote escape); `TSV`/`SEMICOLON`/`PIPE`/both `BACKSLASH_*`
dialects select `try_parse_named_dialect_record`, a `const`-generic
kernel parameterized by delimiter byte and by whether backslash escaping
is active — one function monomorphized per dialect rather than five
hand-written copies, but functionally a dedicated kernel per dialect. Any
other custom dialect falls through to `None`. Each kernel is additionally
instantiated over a `const CHECK_FIELD_LIMIT: bool`: the field-size check
is skipped only when the whole input is already known smaller than the
limit (decided once at construction for slice parsers); it is
unconditionally `true` for a windowed parser, because a growing window can
eventually hold a field that only later exceeds the limit.

On `x86_64` with AVX2 and BMI2, standard CSV owned records also have a fused
plain-record materializer. Each 32-byte load produces delimiter, quote, and
newline masks while retaining the loaded words; BMI2 `PEXT` removes structural
bytes eight at a time, and comma positions become `ByteRecord` endpoints
without rereading field slices. Capability checks use the runtime's cached
feature bits only on eligible owned CSV reads, so parser construction and other
formats pay nothing. Quotes, incomplete blocks, unsupported CPUs, and records
beyond the 4 KiB specialization bound discard partial output and resume through
the existing kernel.

For standard CSV owned records, quote handling adapts to the observed row
shape. A single interior quoted region is parsed by a scalar prefix helper and
then handed back to the structural kernel. A second adjacent or separated
quoted field switches subsequent predicted rows to a whole-record strategy. On
AVX2, short rows are classified from two 32-byte delimiter, quote, and newline
masks; quote parity removes separators inside quoted fields, and doubled-quote
pairs are decoded directly from the quote mask without restarting the record
through the scalar parser. Longer rows use the existing scalar whole-record
parser. The strategy is selected by function pointer during construction and
changed only inside the already-predicted quote branch, leaving unrelated
plain-record paths unchanged.

When `owned_parser_for` returns `None`, records still run an
unquoted-field fast path (`try_parse_owned_plain`) driven directly by
`StructuralBlocks`, but perform trimming, blank-skipping, NULL-detection,
and non-strict-syntax checks per field — work the specialized kernels
cannot do. A quoted field or unusual byte defers further to per-field
routines that assemble the record piece by piece.

### Why there is no projected kernel

An earlier design resolved a target naming a subset of the columns to a
separate "projected" mapping, read by `try_parse_default_projected_record`
— a kernel that walked the record with the same grammar as the plain one
but pushed a `Span` only for selected fields. The premise was that
skipping unwanted columns must be cheaper than discovering them.

It is not, and the reasoning is worth keeping because it generalizes.
Projection earns its place when it avoids *copies*. A lending `Record`
makes no copies: an unwanted field costs one `Span` push, and the scan has
to cross its bytes either way to find where the next one starts. So the
projected kernel had nothing to skip, and paid for the privilege by being
scalar where `parse_positioned_record` is vectorized. On the streaming
front ends it was worse: `push` and `io` must parse a record to know it
fits the window, and the projected branch then discarded that parse and
rescanned from the record start.

Removing it — see `benches/decode.rs` — cut per-record instructions by 33%
on `slice` and by 47% on `push` and `io`, with the control benchmark unchanged.
The identical change had already been made on the Serde path. What remains
is a single mapping applied to a fully materialized set of spans, which is
what `TypedMapping::Mapped` is.

### `needs_general_parsing`: the record-level escape hatch

Independently, `needs_general_parsing` decides — for the borrowed
(`Record`/`Span`) path, not the owned path above — whether records use the
structural-block walk or the fully general byte-by-byte machine. It is
`true` when any of: `RecordEnding::CrLf` (rejecting a bare `\r`/`\n`
outside quotes needs per-byte lookahead the block mask doesn't carry — it
only says *a* terminator byte matched, not which one, nor whether its
partner follows); an escape style that applies outside quotes —
`Escape::Mysql` or `Escape::Unquoted` (escape decoding is a stateful
"consume the next byte specially" transform the scan can't express as
"skip a run of matches"); any non-`None` `Nulls` (NULL comparison happens
*after* boundaries are known, not while finding them); or a `Whitespace`
policy that exempts quoted fields (an owned record trims in one pass over
already-flattened bytes with no memory of which came from a quoted field,
whereas each `Span` still remembers its quoted flag — so exempting quoted
fields specifically forces the path that still has that information at
trim time). These four conditions are orthogonal to the `owned_parser_for`
list: they gate whether the engine falls back to per-byte parsing at all,
while the other list gates which specialized function handles the
non-general case — a configuration can need general parsing for the
borrowed path while still having an owned kernel available, or vice versa.

## Zero-copy representation and copy-on-escape

A borrowed record's fields are a `Vec<Span>` alongside two byte buffers:
the caller's `input` and a per-parser `scratch` buffer reused across
records. `Span` packs a source tag, a "quoted" flag, and an "explicit
NULL" flag into the spare high bits of its two `usize` offsets instead of
adding separate fields: the top bit of `start` selects `input` (`0`) vs.
`scratch` (`1`); the second-highest bit of `start` marks an explicit NULL
field — always zero-length by construction, so this bit never has to
survive offset arithmetic; the top bit of `end` records whether the field
was quoted. This is safe because both offsets are validated against
`MAX_OFFSET = (1 << (usize::BITS - 2)) - 1` before construction — well
above the 16 MiB default record limit on a 64-bit target — so the packing
costs nothing while saving two `bool` fields (plus padding) per field,
which matters because `spans` is rewritten every record.

The library is forced to copy bytes only in two situations. First, an
escaped quote (doubled `""` or a backslash escape) inside a quoted field:
the decoded value is literally shorter than its source bytes, so no
contiguous sub-range of the input can represent it. The parser copies the
already-scanned prefix into `scratch` (lazily, only once the first escape
in that field is actually found), appends the unescaped byte, and
continues — only fields that actually contain an escape pay this cost.
Second, a record whose bytes aren't contiguous at inspection time — the
defining case being one straddling a streaming window refill: only a
record known to end strictly inside the current window is ever handed
back as a borrowed `Record`; anything else is retried against a wider
window, or (when retained across calls) copied wholesale into an owned
`ByteRecord` so it survives the window being compacted underneath it.
`Record::is_copied` (test-only) reports per field whether its span
currently resolves against `scratch` — literally "was the source tag set
to `Scratch`" — used to pin the copy-on-escape rule down in tests.

The owned representation (`ByteRecord`) takes a related approach: instead
of `Vec<Span>` it stores one contiguous `bytes: Vec<u8>` (every field's
decoded bytes concatenated) plus `ends: Vec<usize>`, cumulative end
offsets where field `i` starts at field `i - 1`'s end. An explicit NULL
field again costs no allocation, via the same "spare bit in an otherwise-
unused offset" trick applied to the flatter layout (no source tag is
needed since there is only one buffer).

## Buffer management for streaming

`IoParser<R>` and `PushParser` give the shared engine a sliding
window over bytes arriving incrementally, rather than the single fixed
slice `SliceParser` operates on. The core problem: a record boundary can
appear anywhere, including straddling wherever the currently-available
bytes end, so the window must grow, retry, and eventually release bytes
it no longer needs — all while enforcing `Limits` so a malformed stream
cannot grow the window without bound.

```
        ┌───────────── window (Vec<u8>) ─────────────┐
        │  already reported   │ in-flight  │  spare    │
        │  (may be dropped)   │ record(s)  │  capacity  │
        └──────────────────────┴────────────┴───────────┘
                              ▲            ▲
                       window_anchor     filled
```

`refill` first calls `compact`, which asks the engine for
`window_anchor()` — `min(cursor_start, location)`, the earliest offset
still needed since everything before it has already been reported — and
`copy_within`s everything from that anchor to offset `0`, tracking a
running `consumed` counter to translate window-relative offsets back to
stream-absolute ones. Only after compacting does it grow the buffer, and
only if compaction didn't free enough room; new bytes land directly in
the buffer's own (pre-zeroed) spare capacity rather than a staging buffer.
`PushParser` reaches the same end differently: `Chunk` reads records
straight out of the caller's slice, and only the record a chunk boundary
cut in half is absorbed into parser-owned storage, so the compaction the
other parsers perform becomes the settling `Chunk::done` does when the
loan ends.

Growth is capped by `max_record_bytes` (16 MiB default): a chunk is only
ever taken in part — the caller drains records and offers the remainder —
once taking the whole of it would let the in-flight record exceed that
bound, returning an error rather than growing forever. The
same bound applies to `IoParser` via `ErrorKind::RecordTooLarge`/
`FieldTooLarge` raised as the window grows, exactly as for the slice
parser; `max_fields` (16,384 default) bounds field count the same way.
Together these make unbounded growth categorically impossible: either a
terminator is found before the limit and the window shrinks back via
`compact`, or the limit is hit first and parsing fails cleanly.

`compact` only moves live bytes to the front; it does not give capacity
back to the allocator, so one outsized record would otherwise pin its own
size for the life of the parser. `reclaim` closes that gap. It runs on
the same compact-then-append path, where the window is at its emptiest,
and applies a deliberately reluctant rule: capacity is only handed back
when it exceeds both a floor and four times what is about to be needed.
The factor leaves a doubling growth policy's ordinary headroom alone, so
a steady workload never reallocates, and the test — a load and two
comparisons — inlines into the caller while the reallocation itself stays
`#[cold]` and out of line.

The order matters on `IoParser`. Reads land in the window's *pre-zeroed*
spare capacity, so truncating it unconditionally would force the growth
step to re-zero a full read's worth of bytes on every refill; the
measurement for that mistake was a 2.4% regression on the plain fixtures.
The window is therefore only disturbed once the test has already passed.

A record ending strictly inside the window is known whole and handed back
immediately. One that reaches the end of available bytes without a
terminator is ambiguous — complete, or merely waiting on more bytes — so
the engine reports `Advance::NeedMore`, the window widens, and the same
offset is retried. `finish()` on `PushParser` (and real EOF on
`IoParser`) resolves that ambiguity: once the caller declares no
more bytes are coming, a record reaching the window's end without a
terminator is accepted as complete rather than held pending forever — why
a stream with no trailing newline still needs an explicit `finish()` call
before its last record is reported; without it, "maybe more is coming" is
indistinguishable from "the stream just ended."

`window_anchor()` also lets `advance_window` retry speculatively without
losing its place: it snapshots the cursor (`cursor_state()`), attempts a
parse, and — if the result was inconclusive (`NeedMore`, or an error
provably about running out of window rather than malformed input, per
`truncated_by_window`) — restores the exact prior cursor
(`restore_cursor`) so the retry after widening starts clean.

Owned-only `IoParser` loops bypass line-view staging through
`IoParser::read_byte_record_into`. The same speculative cursor snapshot is
used, but a settled record is parsed directly into the caller's reusable
`ByteRecord`; no internal record is staged and no full-record swap follows.
The ordinary `next_line` path retains staging because it cannot know which
view the caller will request.

## Header handling and the `headers_initialized` invariant

Headers come from three policies: `None` (no header row), `FirstRecord`
(the first physical record read is consumed as headers, not data), or
`Provided(record)` (caller supplies headers up front; the input's first
record is ordinary data). The engine's invariant: **`headers_initialized
== false` if and only if the policy is `FirstRecord`.** This falls
directly out of construction — `None` and `Provided` both set it `true`
immediately, since there is nothing left to discover, while `FirstRecord`
is the only policy that starts `false`, because the first record has to
actually be read before headers exist. `ensure_headers` (non-windowed
parsers) and `ensure_headers_window` (streaming/push, which may not yet
have enough bytes for a whole first record) are the only functions that
flip it, and both do so unconditionally as their first act — before even
checking the policy — so a second call is a guaranteed no-op.

Consequently, code that runs after headers are known resolved
(`on_headers_changed`, the serde struct-cache sync it triggers, typed
mapping resolution, filter-column resolution by name) can assume
`header_record` is final and never re-checks whether a header row is
pending. Every public entry point that can be a parser's first call
(`advance`, `read_byte_record_into`, `advance_with_filter`, `deserialized`,
`decode_with_mapping`)
begins by calling the appropriate `ensure_headers*`, while deeper helpers
rely on it already having run — which is what makes some of their
branches provably unreachable rather than merely unlikely.

### The header lookup is built on demand

`on_headers_changed` invalidates the name-to-column map but does not fill
it. Nothing on the paths that matter reads it: typed decode resolves its
columns with `resolve_decode_mapping`, which scans the header record
directly, and the Serde path uses its own struct cache. The map exists for
`header_index`, `header_indices`, and resolving a filter's `Column::Name`,
all of which are explicit caller requests.

Building it eagerly therefore charged every parser for a structure most
never touch, and charged it per column: a boxed copy of each name, a `Vec`
for the columns sharing it, and a SipHash, into a map that was not
pre-sized. On a hundred-column header that was about 138,000 instructions
before the first record was read. It is now built on the first read, and
`benches/decode_wide.rs` records what that was worth.

Three smaller decisions go with it, in `engine/header_lookup.rs`. The map
is pre-sized from the header count. A name that appears once stores its
column inline as `HeaderSlots::One`, so only a genuine duplicate allocates,
while `header_indices` can still hand back a `&[usize]` in both cases.
And it hashes with `HeaderHasher`, a rotate-xor-multiply mix, rather than
`RandomState`: the keys are the parsed file's own header names, the map
dies with the parser, and a caller who controls the header row controls the
whole input, so there is no adversarial-collision exposure for SipHash to
defend against. Without `std` the map is a `BTreeMap` and none of the
hashing applies.

Scanning rather than hashing is the right default here and the numbers say
so: resolving five names against a hundred columns by linear scan costs
about a fifth of what building the map cost, and it is paid only by targets
that ask.

## Typed decoding

Two routes turn a record into a typed value, sharing little beyond
`Record`/`ByteRecord` and the byte-native `FromBytes` conversions.

**`CsvDecode`/`DecodeRecord`** (driven by `#[derive(CsvDecode)]`)
generates a fixed positional or header-name mapping resolved once
(`resolve_decode_mapping` scans the header record per named field,
erroring on a missing or ambiguous name). Generated code calls
`FromBytes::from_bytes` directly on raw bytes — no intermediate `&str`
unless the target type needs one. Integer conversions walk ASCII digits
with a `checked_mul`/`checked_add` accumulator, and float conversions hand
raw bytes straight to the Eisel-Lemire-based `fast_float2` parser, both
entirely bypassing `str::from_utf8`: bytes that never claim to be text
never have to be proven valid UTF-8. `String`, `char`, and the `net`
address types do validate UTF-8, since they cannot avoid being text.

**The serde bridge** (`serde.rs`) drives a generic `Deserialize` against a
record, with a cache making repeated struct decoding cheap. `StructCache`
validates header bytes as UTF-8 once (not per record), remembering the
first invalid header so the *original* error still reproduces on every
later record. More importantly it learns which columns a struct's Serde
visitor calls `deserialize_ignored_any` on — but commits that observation
only after a record deserializes *successfully*, because
`#[serde(deny_unknown_fields)]` fails before ever calling
`deserialize_ignored_any` for an unwanted column, and committing a
partial ignore-set from a failed attempt would wrongly start skipping
columns such a struct actually needs to reject. The learned set is a
single `u64` bitmask (only the first 64 columns are tracked; wider
records simply never skip their tail columns — conservative, not
incorrect), keyed by both the field-name slice's `&'static` identity *and*
the struct's type name, since two unrelated structs can share an
identical field list and must not leak one's learned skip-set into the
other. Once the ignore-set is committed, later records consume those
columns' fields without offering them to the visitor, so the discard
happens once rather than on every record.

`Option<T>` ordinarily maps an empty field to `None` via `FromBytes`'s
blanket implementation. When a database `Nulls` policy is configured, the
record is marked `null_aware` and the serde bridge instead consults each
field's real null flag (carried alongside its span/end offset), so an
explicit NULL is distinguished from a merely-empty string even though
both would otherwise look like a zero-length field.

## Predicate pushdown

`Predicate` is deliberately an inspectable value — a `Column`, a
`MatchKind`, and a literal — rather than an opaque closure, specifically
so the literal can be searched for directly in the raw, still-escaped
input before a candidate record is ever split into fields.

Raw-byte search only gives correct answers when a literal, if present in
a decoded field, must also appear contiguously and unmodified in the raw
source. Escaping can both *hide* a literal (a delimiter inside a quoted
field isn't a real delimiter) and *split* one (a literal containing a
quote could straddle a doubled-quote escape), so a literal is pushed down
(`Predicate::is_skippable`) only when it contains none of the dialect's
structural bytes and no bare `\r`/`\n`; an empty literal is never pushed
down since it matches everything trivially. When skippable, `find_literal`
(SIMD `find1` on the literal's first byte, confirmed by a full-slice
compare) locates the next possible occurrence, and `rfind1` walks
backward to that record's start; only that one record is fully parsed and
evaluated — every record between the previous match and this one never
has its fields split at all. Because the raw scan only says the literal
*might* be there, the candidate is always fully parsed and checked
against the real decoded value, so results are always exactly equivalent
to filtering an ordinary full scan.

`filter_backoff` bounds the scan's own overhead: when a scan skips
nothing (a candidate lands immediately adjacent to the current position),
the parser assumes a high match rate and stops re-probing for the next
several (`FILTER_BACKOFF`, 16) records, falling back to plain sequential
parsing during that window before resuming probing — bounding the worst
case (nearly everything matches) to roughly one wasted literal
search per 16 records rather than one per record.

## Encoding

All encoding funnels through one core, `PushEmitter`, which is the dual of
`PushParser`: records go in, encoded bytes accumulate in a caller-visible
`Vec<u8>` buffer, and it never performs I/O. The two public emitters are
wrappers that differ only in what they do with that buffer — `VecEmitter`
never releases it, so the whole document stays in memory, while `IoEmitter<W>`
owns a sink and writes the buffer out whenever it reaches a drain threshold.
Neither contains any encoding logic, so quoting, escaping, BOM placement, and
field-count policy cannot diverge between them.

Records are encoded *in place* at the end of the buffer and truncated back to
their starting offset if they turn out to be invalid, so a rejected record
commits nothing while still costing a single copy rather than being staged in
a scratch buffer first. The byte-order mark is spliced in only when the first
record is actually accepted, which is what makes an emitter whose every record
was rejected emit nothing at all rather than a lone mark.

A `ByteSink` trait abstracts "a growable buffer fields get appended to." It now
has a single implementor, `Vec<u8>`, since every emitter shares the one buffer
type.

**Quoting decisions** (`needs_quotes`) are not SIMD; they use a SWAR
(SIMD-within-a-register) technique over one `u64` (8-byte) word at a
time: each structural byte is broadcast into a same-valued 8-byte word,
XORed against the field's chunk (producing a zero byte exactly where they
matched), and `zero_byte_mask` detects any zero byte via the bit-twiddling
identity `(word - ONES) & !word & HIGH_BITS`. This rejects 8 bytes'
worth of "does this need quoting" in a few ALU ops without a
target-feature-gated intrinsic; the remainder under 8 bytes falls back to
a byte-at-a-time check.

Once quoting is needed, escaping dispatches on `Escape`: doubled-quote
style rewrites each quote as two quotes; backslash style prefixes both
quote and escape bytes; and **MySQL-style escaping is a categorically
different route** that never quotes at all, instead prefixing a fixed
table of special bytes (NUL, backspace, `\n`, `\r`, `\t`, `0x1A`,
backslash, plus the dialect's own delimiter/terminator/quote) with a
backslash, matching MySQL's own export convention.

Layered on top are the **whole-document generation entry points** —
`encode_to_vec`, `encode_to_writer`, and `encode_to_path`. These invert control relative to the
emitters: rather than the caller pushing records, they pull an iterator of
`CsvEncode` values to completion, emit the statically known header record,
and finalize the sink. Because values are pulled lazily into the emitter's
bounded buffer, peak memory is flat in the record count for the `encode_to_writer`
and `encode_to_path` shapes; `encode_to_vec` is necessarily proportional to output, since
the document is the return value.

`serialize_to_vec`, `serialize_to_writer`, and `serialize_to_path` are the
Serde counterparts, driving an iterator of `Serialize` values through the
same emitters with the same memory behavior. They differ in exactly one
observable way, which is inherent rather than incidental: Serde field names
are discovered from the first value actually serialized, whereas the native
path reads them statically from `CsvEncode::field_names`. An empty iterator
therefore yields a lone header record natively and an empty document through
Serde. The alternative — synthesizing a header from a type that was never
serialized — is not something the `Serialize` trait can express, so the
difference is documented and tested rather than hidden. 

The read side has the mirroring six: `decode_from_slice`, `decode_from_reader`
and `decode_from_path`, with `deserialize_from_*` as their Serde counterparts.
Where the write side takes an `IntoIterator` of values and drives it to
completion, these return an `Iterator` of `Result<T, Error>` — the same
inversion, run backwards. Returning a `Vec<T>` was rejected for the reason
`encode_to_writer` exists: it would make resident memory proportional to the
input, which only `decode_from_slice` is entitled to and only because the
caller already paid it.

The iterator **owns** its parser, which is the property the parser methods
cannot offer. `Cursor` is therefore generic over how the parser is held,
`BorrowMut<P>` covering both `&mut P` and `P`, so the four record kinds are
still written once. Construction failures — an invalid format, a rejected
byte-order mark, an unopenable file — surface eagerly in an outer `Result`
rather than behind the first `next()`, and the typed mapping is resolved once
per run as it already was. The gain over holding a parser in a local is
ergonomic; the per-record path is unchanged and measured to be so.

Three further shapes sit on the same core. `encode_append_path` resumes an existing
document: it probes the file's tail against the configured record terminator
and refuses to continue when the final record is unterminated, because
appending there would fuse the new first record onto the truncated one. The
probe reads as many bytes as the terminator actually occupies, so `CrLf`
requires the full `\r\n` rather than accepting the bare line feed its
single-byte form would match. When it does resume, the header record and the
byte-order mark are suppressed, since both belong only at the start of a
document. A file holding nothing but a byte-order mark is the one case where
those two decisions diverge: it is a started document with no records, so the
mark is not repeated but the header it never received is still written.
`encode_to_segments` inverts that:
every part it writes is a standalone document, so the preamble is repeated in
each one, and rollover is evaluated only between records — a record larger than
the size bound produces an oversized part rather than a split, because a split
record is not recoverable. Both are driven from the emitter's own buffer, so
neither reintroduces a write per record.

`CsvIndex::generate` closes the loop between the two halves of the crate.
Record offsets and physical line numbers are already known while encoding, and
the output is hashed as it drains, so the index a reader would otherwise have to
recover by reparsing falls out of generation for the cost of two integers per
record. Index entries stream to their own file as they are produced, so a
document larger than memory yields an index without either being held. The
correctness argument is deliberately not structural but differential: the index
produced while generating must equal the one built by parsing the generated
bytes, which is what pins the handling of the byte-order mark (written ahead of
the emitter, so its splice cannot shift measured offsets) and of line feeds
embedded in quoted fields.

`VecEmitter::emit_slices` pre-computes an upper bound on encoded size
(twice the summed field lengths, worst case every byte escapes, plus
per-field/per-record overhead) and calls `try_reserve` once before
writing a byte, so the write loop never reallocates mid-record and an
allocation failure surfaces as an ordinary `Result`.

This capacity strategy used to go further: an earlier `ReservedOutput`
type wrote directly into a `Vec`'s reserved-but-uninitialized spare
capacity via `unsafe` pointer writes, skipping `Vec`'s own bounds checks.
It was deliberately removed in favor of the safe operations used today,
at a measured cost of roughly +5% instructions on the vector-encode
benchmarks — a conscious
safety-over-speed tradeoff, judged worth it given how often and widely
the emitter's hot loop runs. The crate's only remaining `unsafe` is the
three per-architecture SIMD sites in `search.rs` described
[above](#simd-structural-scanning) and the bounds-check-elision
`get_unchecked` calls in the default-dialect kernels
(`engine.rs`/`engine/record_parser.rs`) — confirmed by grepping the crate
for `unsafe` and finding no other file.

## The random-access index

The `index` feature persists a mapping from record number to byte offset
(and physical line number) so a huge file can be opened at an arbitrary
record without re-scanning everything before it. The on-disk format is
conceptually a small fixed header (source length, a content hash of the
source, the `FormatOptions`/`Limits` used to build it, and a record
count), followed by one offset and one line number per record, followed
by a whole-payload xxh3 checksum. Building the index is just an ordinary
parse over the source (`SliceParser` in memory, or `IoParser` over
a file for the constant-memory builder), recording each record's start as
it is reported and discarding the fields.

Two validation layers protect against misuse: **content validation**
recomputes the source's length and hash and rejects any mismatch (a
modified, truncated, or different file) before trusting a stored offset;
**index integrity validation** (the whole-payload checksum) independently
protects against the index file itself being corrupted, since a bit-flip
there says nothing about whether the source changed. Once both pass,
`parser_at` builds a parser spanning the whole source (so byte/line/record
counters stay absolute) and seeks it directly to the looked-up offset — a
pure cursor repositioning, not a re-parse of everything before it.

### Why the parallel builder merges rather than shares a buffer

From 8 MiB up (`PARALLEL_INDEX_THRESHOLD_BYTES`, measured and gated by
`benchmarks/index/run.py`), the in-memory builder splits the source into
`threads * INDEX_CHUNKS_PER_THREAD` chunks, gives each worker its own pair
of position tables, and concatenates them once every chunk has finished.
That costs roughly twice the final table at peak: `scripts/perf_memory.rs`
records `index_build_parallel` at 30.45 MiB peak and about 1,096
allocations against a 12.67 MiB final table, where the serial builder
manages 3 allocations and no doubling.

The obvious alternative — reserve one shared pair of tables and hand each
worker a disjoint `split_at_mut` range, so workers write into their final
home and the merge disappears — was considered and rejected, for two
reasons that are properties of the problem rather than of the current
code:

- **The chunk sizes are not known before parsing.** Disjoint ranges need a
  per-chunk entry count up front. The only counter available cheaply is
  `build_entry_estimate`, which counts record-ending *bytes* and therefore
  over-counts every record ending inside a quoted field. It is an estimate
  by construction, so ranges derived from it can be overrun, and handling
  that needs a spill path per chunk — which reintroduces the merge for
  exactly the documents where memory matters most. Getting exact counts
  instead means a full counting pass over the source, roughly doubling the
  work the builder does.
- **The crate forbids `unsafe`.** `split_at_mut` needs an initialized
  slice, so the shared tables would have to be zeroed to their full length
  before any worker could write — trading the merge's copy for a memset of
  the same 25 MiB. That is a different cost, not obviously a smaller one.

So the doubling stands, and it is bounded, transient, and proportional to
the index being produced rather than to the source: a caller already
holding the document pays it only for the duration of the build. The
serial builder, which is what runs below the threshold, has none of it.

## `no_std` and feature architecture

The crate builds under `no_std + alloc` with default features disabled,
covering slice parsing, the borrowed/owned record types, `FromBytes`,
field projection, and the in-memory `VecEmitter`. What that configuration
cannot do is anything inherently OS-dependent: `IoParser` (reads
from `std::io::Read`), filesystem-backed encoding, and — per the crate's
own feature table — `benchmarking`, `index`, and `serde`, all gated on
`std` (the `serde` *crate* is itself `no_std`-capable; the dependency here
is on this crate's own streaming/caching machinery, not on `serde` needing
an allocator). `derive` has no `std` dependency of its own — only on the
`coseva_macros` proc-macro crate — so it composes with a `no_std` build as
long as the generated code only calls `no_std`-available API. `PushParser`
deliberately needs neither a reader nor the filesystem, only fed byte
slices, making it the crate's recommended integration point for
`no_std`/WASM/FFI/async contexts — an async socket, a WASM callback, a
decompressor emitting blocks.

`multibyte` is the one feature that gates a *format option* rather than a
capability, and it is a feature for a reason worth recording. A delimiter or
terminator of several bytes has to be stored somewhere, and `FormatOptions` is
`Copy` and `const`-constructible, so "somewhere" means inline in every value of
it. Storing two four-byte tails grows the struct from 20 to 28 bytes, and the
cost of that is not free: parser construction measured about 80 instructions
more, linearly in the added bytes at roughly six instructions each, whether or
not any dialect used a multi-byte separator. No encoding avoids it — the
alternatives were measured at 22 and 24 bytes and cost proportionally.

So the option is one a caller opts into. With the feature off, all 162 cases of
`startup`, `read_record`, `dialects`, `quoted` and `encode` report instruction
counts identical to a build with no such option, which is the strongest form of
"costs nothing to callers who do not use it" available.

Where the tails live also decides how they are read. `Dialect` keeps a single
lead byte for each separator — the byte every scan in the crate can find — and
`Dialect::delimiter_tail`/`ending_tail` return what must follow it. Those
accessors exist in both builds and return an empty tail when the feature is off,
so the parse and emit paths spell the question the same way either way and fold
it away when the answer is constant.

A multi-byte separator never uses the vectorized kernel. That is enforced in
`plain_kernel` rather than only in `needs_general_parsing`, because the kernel
runs *before* the latter is consulted: it commits to a record and declines only
for reasons it can see in the bytes, and having already split on a lead byte is
not one of them.

`parallel` is a feature for a different reason: it is a capability, and one
that brings threads into a crate that otherwise creates none. It touches no
scanning kernel. A worker is a plain `SliceParser` over the *whole* input,
seeked to a record boundary and stopped at the next one, which is what makes
the offsets, physical lines and record indices it reports absolute without any
fixing up — the alternative, a parser per subslice, would have needed every
position rebased.

The one new algorithm is the boundary pass. A separator-aware split has to know
whether a candidate record ending sits inside a quoted field, and counting
quotes answers that exactly rather than heuristically, which is why a value
containing a newline needs no opt-out here. Counting is only exact where a
quote byte always means a quote, so formats where it might not — comments,
backslash or MySQL escaping, quoting disabled or bare quotes permitted, skipped
blank records, multi-byte separators — are rejected at the entry point rather
than silently run on one thread. The pass is the serial fraction that bounds the
speedup, so it visits only the bytes a three-byte SIMD scan reports and does
nothing per hit but toggle a flag and bump two counters. Locating fields there
would have capped the gain near 1.3x, since assembling records is most of the
cost of parsing.

Ordering and error selection fall out of one decision: chunks are dealt to
workers round-robin and drained in the same rotation, each worker having its own
bounded queue. That gives document order with no reorder buffer, bounds memory
at threads times queue depth times batch whatever the document's size, and makes
the first failure the consumer sees the first in the document — which is what
"deterministic by lowest byte offset" requires, since every earlier chunk has
already been delivered by then.

That rotation is a deliberate choice rather than a leftover, and it was
measured. On a 32 MiB document whose parsing cost is deliberately skewed — long
quoted fields bunched into a few chunks — the owned path scales badly where the
borrowed path does not: 0.53x, 0.86x and 1.28x at 2, 4 and 8 threads, against
the borrowed path's 0.75x, 2.51x and 2.82x on the same bytes. The obvious
suspect is the static rotation, since a run of expensive chunks lands on one
worker while its neighbours idle, and the borrowed path already avoids that by
letting workers claim chunks from a shared cursor.

Replacing the rotation with the same cursor claiming does not recover the
scaling. Workers claim from a monotonic cursor and publish which chunk each took
so the consumer can still read their queues in document order; measured against
the rotation in paired, interleaved runs, the owned path moved by a median of
1.06x, 1.03x and 1.12x at 2, 4 and 8 threads, with a per-round spread from 0.43x
to 1.36x — an effect smaller than the noise.

The reason is that assignment is not the bottleneck. Ordered delivery is: the
consumer takes chunks strictly in document order, so a worker still parsing an
expensive chunk blocks it however that chunk was assigned, and the workers that
ran ahead stall once their own bounded queues are full. Claiming cures the
imbalance at the end of a run, which is not what a skewed document suffers from;
it suffers from head-of-line blocking, which is inherent to delivering in order
with bounded memory. Curing that needs a reorder buffer, and an unbounded one is
exactly the memory promise this design exists to keep. The rotation therefore
stays, and skewed input is a known limit of the owned path rather than a defect
in how it deals out work — `for_each_record`, which makes no ordering promise,
is the path to reach for there.

The published clean-tree reference run is recorded in `docs/PERF.md`, including
its host, toolchain, revision and exact command. On that 16-logical-CPU host the
borrowed `fold` path reaches 2.76x at 64 MiB. Smaller runs can cross earlier on
an idle machine, but the 8 and 16 MiB margins move substantially under ordinary
host load; 32 MiB is the conservative crossover that holds and therefore the
default threshold. The older 16 MiB/2.2x figures described a previous
implementation and are not current evidence.

The gap between speedup and CPU cost is the price of owned records. A serial
parse reads every record into one buffer that stays in L1; a parallel one writes
into a rotating set of thousands and hands them to another thread to read.
Recycling those buffers rather than allocating per record was worth a factor of
three on its own, and batching keeps the handoffs amortized. Because none of
this can be seen by counting instructions, `benches/parallel.rs` is the crate's
only wall-clock benchmark, and it is kept out of the Callgrind suite rather than
merged into it.

## Error model and poisoning

An `Error` carries an `ErrorKind` and a `Location` (byte offset, physical
line, record index, field index) pinpointing where a failure was
detected. Once a parser returns a structural error — a real syntax
violation or an exceeded resource limit, as opposed to a transparently
retried I/O `Interrupted` — the engine sets an internal `failed` flag and
is thereafter **poisoned**: every later call that would parse more input
instead immediately returns `ErrorKind::ParserFailed` rather than
attempting to resume. This exists because the engine's internal cursor
state (spans, scratch contents) is only guaranteed consistent up to the
last successfully parsed boundary; a parse that failed partway through may
leave that state not corresponding to any real input position, so
resuming from it cannot be done safely in general. Poisoning turns
"silently produce a wrong answer from corrupted internal state" into
"reliably report this parser can no longer be trusted" — the more
conservative, more debuggable failure mode. Recovering from one malformed
record therefore requires constructing a new parser positioned past the
bad bytes, not reusing the same one.

## Proc-macro architecture

`coseva_macros` — the crate actually named in `#[derive(...)]` attributes
— is intentionally almost empty: each entry point parses its
`TokenStream` and immediately hands off to `coseva_macros_impl`, passing
an explicit `root_path` (`::coseva`) so generated code can refer to crate
items by an absolute path regardless of how the caller imported `coseva`.

This split exists because a crate declared `proc-macro = true` cannot be
depended on as an ordinary library or unit-tested directly — its macros
can only be invoked end-to-end through a separate crate that uses them,
which makes fine-grained testing of expansion logic (attribute parsing,
field-shape handling, error messages) awkward. Moving all token-generation
logic into `coseva_macros_impl`, an ordinary `lib` crate with no
`proc-macro` restrictions, lets that logic be unit-tested directly against
`TokenStream`/`syn::DeriveInput` values in the normal way. `root_path` is
what lets the same implementation crate be exercised by tests that don't
depend on `coseva` at all — the generated code just needs some valid path
to compile against, not necessarily the real crate name.

## API guideline departures

The crate was audited against all 143 of the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/checklist.html)
(`C-*`) and the [Pragmatic Rust Guidelines](https://microsoft.github.io/rust-guidelines/)
(`M-*`). Most deviations found were defects and have been fixed. The ones
recorded here are deliberate: the guideline was read, understood, and departed
from for a reason that is written down so the next audit does not re-open it.

### `FieldProjection::new` names a boxed slice in its signature

[M-AVOID-WRAPPERS](https://microsoft.github.io/rust-guidelines/guidelines/libs/ux/#M-AVOID-WRAPPERS)
(a *should*) asks that smart pointers stay internal rather than appearing in
public signatures. `FieldProjection::new` takes `impl Into<Box<[usize]>>`
(`src/projection.rs:44`), so `Box` is named there.

It stays, because the `impl Into<_>` is what the guideline is actually after:
callers pass a `Vec` or an array and never write `Box` themselves, while the
type still records that the projection is fixed once built. Taking `Vec<usize>`
instead would name no wrapper but would keep a capacity word that can never be
used.

The other site the audit found was a real leak and was fixed rather than
recorded. `Column::Name` held a `Box<str>`, which every construction and every
`match` had to spell. It now holds a `Cow<'static, str>`. The size argument for
boxing did not survive measurement — `Column` grows from 16 bytes to 24 and
`Predicate` from 72 to 80, but a predicate is built once and read for every
record, so it stays resident and the extra word costs nothing; what is read per
record is `name.as_bytes()`, which is the same two loads either way. `Cow`
rather than `String` because at identical size it is the only candidate that
lets a literal column name stop allocating. That gain is opt-in through
`Column::borrowed` rather than automatic: `From<&'static str>` and
`From<&'a str>` overlap, coherence permits one, and the general one was kept so
that a shorter-lived name still converts.

### Boolean setters are not two-variant enums

[C-CUSTOM-TYPE](https://rust-lang.github.io/api-guidelines/type-safety.html#c-custom-type)
(a *should*) argues that a `bool` argument makes a call site opaque —
`.skip_initial_space(true)` does not say what `true` selects — and prefers a
named two-variant enum per flag. Six public setters take a bare `bool`:

- `src/config/recovery.rs` — `quoting`, `unquoted_quotes`,
  `any_backslash_escape`, `trailing_whitespace_after_quote`
- `src/config/format_options.rs` — `skip_initial_space`
- `src/config/emit_options.rs` — `has_headers`

They stay `bool` for three reasons. Each name is already a predicate that reads
correctly with a boolean, so the enum would restate the setter's own name
rather than disambiguate it. The builder-with-`bool` form is what Rust callers
expect, and all six are `const fn`, which enum construction would not improve.
And `csv`, the crate users most often arrive from, takes `bool` in exactly
these positions, so diverging would cost every migrating caller a rewrite.

Six new public enums is real, permanent API surface, added to remove an
ambiguity that these particular names do not have. The guideline is right in
general and does not pay here.

### `Error` exposes its kind and carries no backtrace

[M-ERRORS-CANONICAL-STRUCTS](https://microsoft.github.io/rust-guidelines/guidelines/libs/ux/#M-ERRORS-CANONICAL-STRUCTS)
(a *should*) prefers that an error's category not be part of the public surface,
and that errors carry a backtrace. `Error` departs on both counts.

It is a struct wrapping a private `kind: ErrorKind` (`src/error/mod.rs:71`)
exposed through `Error::kind()` (`src/error/mod.rs:225`). The hazard the
guideline protects against is a public category that cannot be extended without
a breaking change — but `ErrorKind` is `#[non_exhaustive]`
(`src/error/kind.rs:14`), so variants can be added freely and callers must
already carry a wildcard arm. With the evolution hazard removed, what is left
is a real need: matching on the kind is the only way a caller can tell a
malformed quote from an I/O failure, and reacting differently to those two is
the ordinary case for a parser.

`Error` carries no backtrace because it cannot. The crate is `no_std`-capable —
`std` is a default feature, not a requirement, and the `no_std` build is tested
— while `std::backtrace::Backtrace` exists only under `std`. A `std`-gated
backtrace field is possible, but it would make the size and the behavior of a
public type differ by feature, which is a worse defect than the one it fixes.

## Test coverage and what is deliberately not covered

Coverage is measured with

```text
cargo +nightly llvm-cov --workspace --all-features
```

The `+nightly` matters: test modules and a handful of provably unreachable
helpers carry `#[cfg_attr(coverage_nightly, coverage(off))]`, which is a
nightly-only attribute enabled by `#![cfg_attr(coverage_nightly,
feature(coverage_attribute))]` in `lib.rs`. On stable those regions are
measured as ordinary code and the reported number is meaninglessly low.

Line coverage sits at **96.6%** across the workspace, measured with the
command above; 478 lines in 37 files are uncovered. The sections below
enumerate the *categories* those lines fall into and name the largest
concentrations. They are not a line-by-line inventory: the categories are
stable, but the exact residue moves with every change, so treat the CI
threshold as the tripwire and this text as the explanation of what the
accepted exclusions are.

The largest concentrations are `index/generate.rs` (70), the SIMD variant
tables in `coseva_unsafe/record.rs` (54) whose non-host paths cannot run on
a single machine, `io_parser.rs` (35) and `projection.rs` (34).

Much of the residue cannot be annotated away. `coverage(off)` applies to a
function, not a statement, and many of the lines below are one-line guards
inside a function that is otherwise fully covered — several of them among
the largest in the crate. Marking those functions off would drop hundreds
of genuinely measured lines out of the report and hide the next real gap in
them, which costs more than the fraction of a percent it would buy.

### Invariant guards that a caller bug would trip

`not_positioned()` is called from five sites in `engine.rs` — in
`rewind_to_current`, `materialize_full`, `read_byte_record_into`,
`read_owned`, and the projected-record reader. It panics when a view
is used against a parser that is not positioned on a record. Every public
entry point establishes that position first, so the guard can only fire on
an internal bug. The function body is already excluded from coverage; what
remains uncovered is the call site itself.

Two `unreachable!()` arms are the same idea in match form:
`engine.rs`'s owned-bytes fast path and `emit.rs`'s `write_quoted` both
have an arm for the escape styles that apply outside quotes, which exists
solely to keep the match exhaustive. Both paths reject those upstream — the
reader diverts to the spans-based path because such dialects always require
general parsing, and the writer diverts to
`write_unquoted_escaped_field` — so neither arm can be entered.

`text_record.rs`'s `field_utf8_error` closes with a third. It is called
only once validation has already failed, so one of the fields it walks must
be the culprit; the tail exists because the compiler cannot see that. Both
ways in — a field that is invalid on its own, and a multi-byte sequence
split across two field boundaries — are covered, so only the tail is not.

### Branches dead by a construction invariant

`headers_initialized` is set to `true` at construction for every header
policy except `Headers::FirstRecord`, and the only place that resets it to
`false` (`reset_headers`) does so only when the policy *is*
`Headers::FirstRecord`. So `headers_initialized == false` implies
`Headers::FirstRecord`, and the `header_policy != Headers::FirstRecord`
early return in both `ensure_headers` and `ensure_headers_window` cannot be
reached. The branches are kept because they make each function correct in
isolation rather than only in the presence of a non-local invariant.

The same invariant strands the cold arm of `fused_mapping_ready`, which
resolves the typed mapping when `headers_initialized` is still `false`. A
`Line` only exists once `advance` has run, and `advance` resolves headers,
so the flag is always set by the time a fused decode asks.

`resolve_typed_mapping`'s cache-hit return and its `None =>
TypedMapping::Identity` arm are unreachable for a related reason:
`resolve_optional_typed_mapping` consults the same cache and short-circuits
the `header_record.is_none()` case before it ever delegates, and the one
direct caller in `iter.rs` resolves the mapping exactly once per run.

### CPU dispatch

`count1` selects a counting kernel at run time. On x86 with AVX2 present it
always takes the AVX2 path, so the portable structural-block fallback is
dead on this host — and on a host without AVX2 the AVX2 path would be dead
instead. No single machine can execute both. The fallback body is factored
out as `count1_portable` precisely so that a test can call it directly and
check it differentially against the naive count on every target; only the
one-line dispatch tail remains unmeasured.

The same run-time choice makes every benchmark ambiguous until it says which
arm it took, and that is not answerable from outside the process: the
Callgrind sentinels run under Valgrind, which emulates the guest CPU and
answers `CPUID` itself, so the arm is a property of the CI image and the
Valgrind version rather than of the host or the source. `benches/dispatch.rs`
therefore reports `coseva::benchmark::dispatch_arm()` from inside the profiled
binary, and `scripts/perf_gate.py` fails when it differs from the arm recorded
in `scripts/perf-dispatch-arm.txt` — under which every committed baseline was
refreshed. The recorded arm is `avx2+bmi2`, so the sentinels do pin the vector
kernels rather than the fallback.

The same benchmark counts structural bytes over the same 64 KiB twice, once
through `scan_scalar` and once through `scan_selected`, which prices the two
arms against each other in one run: 475,176 against 110,634 instructions, so
the dispatched scan is about 4.3 times cheaper. That ratio is gated too, and it
is the stronger of the two checks — a detection flag can be right while the
kernel it gates is not reached, but a ratio that collapses towards 1.0 cannot
be.

### Arithmetic and allocation failures that need impossible inputs

Several guards can only fire on inputs no test can construct: the
`checked_add` overflow in `HashingWriter::write` needs an index larger than
`u64::MAX` bytes, and the `try_reserve` failure arm in
`push_emitter.rs` needs the allocator to refuse a reservation the test
process can actually satisfy. The equivalent `try_reserve` arm in the index
reader *is* covered, because there the count comes from an untrusted file
and a corrupt index can demand an absurd allocation cheaply.

`record_too_large` in `emit.rs` is the same shape — it needs a record whose
encoded size overflows `usize` — but it is a plain error constructor rather
than a branch, so a unit test calls it directly instead. Only its call
sites remain unmeasured, and those are counted above.

### Conversions that cannot fail on a 64-bit target

Widening `usize` to `u64` never fails on any target this crate builds for,
and narrowing `u64` to `usize` never fails where `usize` is 64 bits — but
the narrowing genuinely protects a 32-bit host reading an index written on
a 64-bit one, so it is not dead code, merely untestable here. These were
consolidated into four helpers — `widen`/`narrow` in `index/format.rs` and
`widen_offset`/`narrow_offset` in `io_parser.rs` — which are marked
`coverage(off)` and documented in place. Consolidating them turned roughly
thirty scattered unmeasurable lines into four. What survives is the `?` at
two call sites of `encode_header`, whose only failure mode is now inside
those helpers.

### The rest

Two sites resist a clean explanation and are worth revisiting if the
surrounding code changes: the two EOF returns in `read_physical_record`,
and the `FieldTooLarge` check at the close of an owned quoted field — in
the latter case a coarser limit check against the raw scan window fires
first for every input tried. The two
error arms in the streaming parser's `headers` and `header_indices`, and
the `eof` early return in `refill`, are likewise guarded by their callers
today.

`shift_window`'s "the drop stops short of the line in progress" arm belongs
here too. Reaching it needs a window compaction that drops fewer bytes than
the current line origin, and the origin only moves ahead of the window
front on an indexed seek, so it takes a seek followed by a short refill that
no test happens to produce.
