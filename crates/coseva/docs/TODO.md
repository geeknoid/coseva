# TODO

This file tracks outstanding work for the `coseva` workspace. Completed items are deleted rather than marked done so the list stays a forward-looking backlog.

Paths under `src/`, `tests/`, `benches/`, `scripts/`, and `docs/` are relative to
`crates/coseva/`; paths beginning with `crates/` or `.github/` are relative to
the repository root.

## Contents

### Conformance
- [CON9](#con9) — Make `StaticFormat` genuinely sealed
- [CON11](#con11) — Use the crate's own fast hasher for the projection name-resolution map
- [CON15](#con15) — Settle `compact_str`'s public-API exposure before a 1.0 release

### Features
- [F7](#f7) — Support string interning for decoded, deserialized and encoded fields
- [F9](#f9) — Support decoding into a caller-supplied allocator or arena

### Testing
- [T1](#t1) — Ratchet scheduled mutation survivors

## Conformance

<a id="con9"></a>
### CON9 — Make `StaticFormat` genuinely sealed

**Area:** `src/format.rs`, `../coseva_macros/tests/ui/pass/manual_seal_leaks_today.rs` · **Priority:** Medium · **Effort:** Medium
**Guideline:** [`C-SEALED`](https://rust-lang.github.io/api-guidelines/future-proofing.html#c-sealed) (should) · **Confidence:** High
**Scope:** 1 pseudo-sealed public trait and 1 downstream characterization fixture — exhaustive

`StaticFormat` promises that only `csv_format!` can implement it, but its
sealing supertrait and module are public. A downstream crate can therefore
implement both traits manually and bypass the macro's declaration-time
validation, invalidating the guarantee documented on `StaticFormat`.

- `src/format.rs:45` — `pub trait StaticFormat: CsvFormat + sealed::Sealed`
- `src/format.rs:51-53` — the hidden-but-public `sealed` module and trait
- `../coseva_macros/tests/ui/pass/manual_seal_leaks_today.rs:27-35` —
  downstream code successfully implements `CsvFormat`, `sealed::Sealed` and
  `StaticFormat` manually

The fix must preserve downstream `csv_format!` expansion while making a manual
implementation impossible; simply hiding the current module may also hide it
from exported macro expansions.

**Done when:** the current characterization fixture becomes a compile-fail UI
case while downstream `csv_format!` declarations and renamed-crate expansion
still compile, and the sealing machinery adds no documented public API.

---

<a id="con11"></a>
### CON11 — Use the crate's own fast hasher for the projection name-resolution map

**Area:** `src/projection.rs` · **Priority:** Low · **Effort:** Small
**Guideline:** [`M-FAST-HASHER`](https://microsoft.github.io/rust-guidelines/guidelines/performance/#M-FAST-HASHER) (should) · **Confidence:** Medium
**Scope:** 1 map over trusted internal keys — exhaustive (the only default-hasher `HashMap` in `src/`)

`resolve_names_indexed` buckets columns by `hash_name(header)` — a `u64`
already produced by the crate's own header mix — but stores them in a map built
with the default `RandomState` (SipHash-1-3). The keys are trusted, being
derived from the header row of the file being parsed, and are already well
distributed, so SipHash re-hashes an already-mixed word for no DoS benefit.
This is the same situation `header_lookup.rs` analyzed and deliberately solved;
the projection path simply did not adopt the result.

- `src/projection.rs:295` —
  `let mut buckets: HashMap<u64, Vec<usize>> = HashMap::with_capacity(headers.len());`
- `src/engine/header_lookup.rs:45` —
  `type Table = HashMap<u64, HeaderSlots, HeaderHashBuilder>;`, the fast
  passthrough hasher that already exists in the crate

This runs once per parser over a map sized to the header count, not per record,
which is why it is Low and why it is a clause deviation rather than one of the
`P` items.

**Done when:** the projection bucket map is parameterized with the same
crate-internal `BuildHasher` used by `header_lookup`, so no trusted internal
key is hashed with the default SipHash, and the projection tests still pass.

---

<a id="con15"></a>
### CON15 — Settle `compact_str`'s public-API exposure before a 1.0 release

**Area:** `src/encoding/decode_field.rs`, `src/encoding/encode_field.rs`, `src/from_bytes.rs` · **Priority:** Low · **Effort:** Small
**Guideline:** [`C-STABLE`](https://rust-lang.github.io/api-guidelines/necessities.html#public-dependencies-of-a-stable-crate-are-stable-c-stable) (should), also [`M-DONT-LEAK-TYPES`](https://microsoft.github.io/rust-guidelines/guidelines/libs/interop/#M-DONT-LEAK-TYPES) (should) · **Confidence:** High
**Scope:** 3 impls — exhaustive (`grep -rn 'CompactString' crates/coseva/src`)

The crate implements three of its own public traits for
`compact_str::CompactString`, a type from a pre-1.0 dependency. A trait impl
for a foreign type places that type in the public API, so a breaking
`compact_str` 0.10 to 0.11 release would be a breaking change for any consumer
using `CompactString` through these traits.

- `src/encoding/decode_field.rs:259` — `impl<'record> DecodeField<'record> for CompactString {`
- `src/encoding/encode_field.rs:116` — `impl EncodeField for CompactString {`
- `src/from_bytes.rs:301` — `impl FromBytes for CompactString {`

This does not violate the guideline today: coseva is itself `0.1.0`, so
C-STABLE does not yet bind, and the exposure is behind an opt-in,
dependency-named feature — the accepted pattern serde integrations use. The
other pre-1.0 dependencies were checked and are private: `xxhash-rust` appears
only as `pub(super) hasher: Xxh3`, `fast-float2` only in internal parsing.
It is recorded so the 1.0 milestone does not silently inherit a pre-1.0 public
dependency. `M-DONT-LEAK-TYPES` reaches the same three impls from the
interoperability side; the `serde` integration is not a second instance,
because that guideline explicitly carves out serde support.

**Done when:** before tagging 1.0, either `compact_str` has reached 1.0 and the
requirement is bumped, or `docs/DESIGN.md` records the opt-in-feature exposure
as an accepted departure the way the other three departures are recorded. No
code change is required on the pre-1.0 line, and closing this by writing the
note is a valid outcome.

## Features

<a id="f7"></a>
### F7 — Support string interning for decoded, deserialized and encoded fields

**Area:** `src/encoding/`, `src/serde/` · **Priority:** Low · **Effort:** Large

Real CSV columns repeat heavily — a state code, a currency, a status, a
category. Every occurrence currently becomes its own `String`. An interner
would make each distinct value allocate once and every later occurrence a
cheap handle, which for a low-cardinality column over a large file is the
difference between millions of allocations and a few dozen.

This is a larger change than the `compact_str` support already shipped, because
an interner is state, and none of the current traits carry any. `DecodeField::decode_field`
(`src/encoding/decode_field.rs:19`) takes only the field bytes, its index and
its name, so there is nowhere to put one. The design question this item must
answer first is where interner state lives and how it reaches a field decoder,
and the plausible answers — a thread-local, a parameter threaded through
`DecodeRecord`, or a separate decode entry point taking `&mut Interner` — differ
enough that the rest of the work depends on which is chosen.

On the encode side the same state buys deduplicated output only if the emitter
is willing to track what it has already written, which is a different
mechanism and may not be worth pairing with the read side.

Interning is not free, and the item should be prepared to conclude it does not
pay. A hash of the field bytes is likely to cost more than the ~112 instructions
`benches/decode.rs` prices a short `String` at, so the win is in allocation
count and resident memory rather than instructions, and only above some
repetition rate. That threshold is the number that decides whether this ships,
and `benches/filter.rs` is the model for finding it — sweep the axis that
matters, here column cardinality, rather than measuring one case.

`TextRecord` is in scope only through its owned iteration: its storage is one
contiguous buffer with nothing to intern, but
`IntoIterator for TextRecord` allocates a `String` per field
(`src/text_record.rs:668`). A repeated column iterated that way is precisely
the case interning is for.

**Done when:** interning is available behind an off-by-default optional
feature for decoding, deserialization and encoding; a benchmark sweeps column
cardinality and states the repetition rate above which it wins on both
allocation count and instructions; and the documentation gives that threshold
so a caller can tell whether their data qualifies.

**See also:** the shipped `compact_str` feature, which uses the same extension
points and is the cheaper answer for short fields — interning only pays where
fields are too long to sit inline but repeat often.

---

<a id="f9"></a>
### F9 — Support decoding into a caller-supplied allocator or arena

**Area:** open · **Priority:** Low · **Effort:** Large

Owned decoding always allocates through the global allocator, so a caller who
wants a document's records to land in one arena and be released in one drop
cannot have it. The interesting shape is reading fields straight into an arena
rather than allocating each `String` separately and copying afterwards.

The design is open, and the surface is the hard part rather than the mechanism.
`DecodeField::decode_field(bytes, index, name)` is an associated function with
no context parameter, so an arena handle has no route to a field decoder as the
trait stands. `String` and `Vec<u8>` are bound to the global allocator, so arena
support means arena-backed field types reached through the same extension points
the `compact_str` feature uses, not a change to the existing ones. And
`allocator_api` is still unstable, so anything phrased as `A: Allocator` is
nightly-only; `allocator-api2` and `bumpalo` are the stable routes, at the cost
of a dependency.

Several surfaces are worth weighing before any is chosen: a decode-time context
threaded through a parallel trait, so the arena reaches field decoders without
disturbing the existing signature; an allocator parameter on the parser covering
only its internal scratch and span buffers, which is self-contained but does not
help user field types; and an arena-scoped entry point that yields records whose
fields borrow the arena. It is also worth establishing first how much is left to
win, because borrowed records already allocate nothing, so the gain is confined
to cases where records must outlive the parser window.

Nothing here should perturb the global-allocator path, which stays the default.

**Done when:** a surface is chosen and justified against the alternatives above,
records can be decoded into a caller-supplied arena and released in one drop, the
existing owned and borrowed paths are unmoved under Callgrind, and a benchmark
states what the arena wins over per-field global allocation so a caller can tell
whether it is worth the ceremony.

**See also:** F7, which is the other answer to allocation pressure and attacks
repetition rather than lifetime; the shipped `compact_str` feature, whose
extension points an arena-backed field type would reuse.

## Testing

<a id="t1"></a>
### T1 — Ratchet scheduled mutation survivors

**Area:** `.github/workflows/ci.yml`, `.github/workflows/mutants.yml` · **Priority:** High · **Effort:** Medium
**Gap type:** Infrastructure · **Would catch:** deletion or weakening of a test for unchanged production code · **Scope:** 2 mutation configurations — exhaustive

Pull requests run `cargo mutants --in-diff`, which is an effective gate for
changed production code but generates no relevant mutants when a change only
removes or weakens tests. The scheduled all-feature and alloc-only sweeps cover
unchanged production code, but both mutation commands deliberately continue on
error and the report job only publishes survivor lists. A test-only regression
can therefore make an existing mutant survive while every required workflow
remains green.

- `.github/workflows/ci.yml:539-553` — both pull-request mutation runs are
  scoped to `merge-base.diff`
- `.github/workflows/mutants.yml:48-52` — all-feature shard failures are
  tolerated so the report can collect every survivor
- `.github/workflows/mutants.yml:81-90` — the alloc-only sweep likewise
  tolerates survivors
- `.github/workflows/mutants.yml:113-180` — the report summarizes and uploads
  survivors but does not compare them with an accepted baseline or fail on
  additions

The scheduled jobs should retain their collect-everything behavior; the missing
piece is a final ratchet over normalized survivor identities, with an explicit
reviewed baseline for equivalent or intentionally accepted mutants.

**Done when:** deleting an assertion that is the sole killer of a mutant in
unchanged production code causes a required mutation workflow to fail in both
the all-feature and alloc-only configurations, while the reviewed survivor
baseline remains green.
