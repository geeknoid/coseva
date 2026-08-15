# Testing

How this crate decides its tests are good enough, and what it does when they
are not.

Coverage answers "did this line run". It does not answer "would anything have
noticed if this line were wrong", and that second question is the one that
matters. A suite can execute every line of a comparison and still not care
which way the comparison points. So coverage is a floor here, not the
measurement, and mutation testing is the measurement.

## The two gates

| Gate | Where | What it asks |
| --- | --- | --- |
| Line coverage ≥ 95% | `ci.yml`'s `coverage` job, every pull request | Did a module drop out of the measured set entirely? |
| Mutants in the diff | `ci.yml`'s `mutants` job, every pull request | Is the code this pull request touched defended by a test that fails when it is wrong? |
| Full mutation sweep | `mutants.yml`, weekly, 8 shards | Has the suite's strength decayed anywhere? |
| Alloc-only coverage and mutants | `ci.yml`'s `coverage` and `mutants` jobs, and `mutants.yml`'s `mutants-alloc-only` job | Is the `no_std` surface defended, or merely compiled? |
| Miri, scalar arm | `ci.yml`'s `miri` job, every pull request | Does the unsafe code have undefined behavior? |
| Miri, avx2+bmi2 arm | `ci.yml`'s `miri` job, every pull request | …and the same question of the vectorized kernels, which the run above cannot reach |
| Coverage-guided fuzzing | `scheduled.yml`, daily, 10 targets | Is there an input nobody thought of? |

Miri is listed twice because it has to be run twice. Every vectorized kernel in
`crates/coseva_unsafe` is selected at run time by `is_x86_feature_detected!`, and
Miri does not execute a real `cpuid`: under the interpreter that macro reports
the *compile-time* target features, which a default `x86_64-unknown-linux-gnu`
build sets neither `avx2` nor `bmi2` in. A plain `cargo miri test` therefore
interprets the scalar fallback and reports success whatever the vectorized code
does. That is measured rather than argued — a deliberate one-byte over-read in
`load_avx2` passes the plain run and fails the `-C target-feature=+avx2,+bmi2`
one with a Stacked Borrows violation — and it mattered, because
`crates/coseva_unsafe` has no unit tests of its own for `record.rs`, so that job
is the only thing between a bounds bug in a vectorized kernel and a green build.

`tests/miri_unsafe.rs`'s `the_dispatch_arm_is_the_one_the_job_asked_for` makes
each run state which arm it got, so a `RUSTFLAGS` that stops taking effect fails
the job rather than quietly returning it to the arm the other run already covers.

The per-pull-request mutation gate is `--in-diff` rather than a full sweep
because the workspace generates ~4,700 mutants and a full sweep costs hours.
`--in-diff` scopes it to the change, which is where a new gap is introduced.

### Why every measurement is run twice

The primary runs pass `--all-features`, because the subtlest behavior in the
crate lives behind `index`, `parallel`, `serde` and the `test-util`
oracle, and a mutant inside a feature that is never compiled is unkillable
whatever the tests do.

That configuration also compiles every `#[cfg(not(feature = "std"))]` body out,
and the consequence is worse than a blind spot. `llvm-cov` cannot measure code
that does not exist, so the 95% figure is silent about the alloc-only surface by
construction — not measured low, not measured at all. `cargo-mutants` is worse
still: it rewrites source syntactically, *before* `cfg` is resolved, so it does
generate mutants in alloc-only bodies, applies them to code the compiler then
discards, watches the suite pass, and reports them as survivors. Those survivors
are noise that no test could ever have killed, and they bury the real ones.

The alloc-only surface is on the crate's "must not break" list, so each gate is
paired with a `--no-default-features` run. Coverage is reported rather than
gated — the alloc-only test population is small enough that a percentage floor
would be noise, and the value is in the number existing and being visible when
it moves. The mutation sweep is scoped to the files that carry an alloc-only
branch, derived from the source by `grep` so a new branch joins it without
anyone remembering to, rather than sharded across the workspace a second time.

## Reproducing the measurements

```text
# Coverage, exactly as the gate runs it. `+nightly` is load-bearing.
cargo +nightly llvm-cov --workspace --all-features

# Mutation testing, one file at a time — start here, a full sweep is hours.
cargo mutants --file crates/coseva/src/reclaim.rs -j 4

# The full set, as the weekly sweep runs it.
cargo mutants --shard 1/8 -j 4 --all-features

# Miri, both dispatch arms. The second is the one that reaches the AVX2 kernels.
cargo +nightly miri test -p coseva --test miri_unsafe --features std
COSEVA_MIRI_EXPECT_AVX2=1 RUSTFLAGS="-C target-feature=+avx2,+bmi2" \
  cargo +nightly miri test -p coseva --test miri_unsafe --features std

# The alloc-only surface, which neither of the above can see.
cargo +nightly llvm-cov --workspace --no-default-features --summary-only
cargo mutants --no-default-features -j 4 \
  $(grep -rl 'not(feature = "std")' crates/*/src --include='*.rs' \
    | sed 's/^/--file /')
```

The feature set is chosen at each call site rather than in `.cargo/mutants.toml`,
because there is more than one that matters and a config-level
`additional_cargo_args` silently overrides the command line.

Configuration lives in `.cargo/mutants.toml`, which documents why each
excluded site is excluded and cites the DESIGN.md section establishing it.

## The surviving-mutant baseline

A surviving mutant is one of three things: a real gap in the tests, an
*equivalent mutant* whose change cannot alter observable behavior, or an
unreachable site. Only the first is a defect. The distinction has to be argued
per survivor — "we looked at it and it seemed fine" is how a real gap gets
filed as accepted — so every entry below states the mechanism that makes it
unkillable.

The weekly sweep publishes the current full list as an artifact. This section
records the survivors that have been analyzed and accepted, so that the sweep's
output can be diffed against a known set rather than re-triaged from scratch.

The baseline below is the `--all-features` one. The alloc-only sweep is new and
its survivors have not been triaged yet: the first `mutants-alloc-only` run
establishes that list, and until it is triaged its survivors should be read as
"unmeasured until now", not as "accepted". One is already known —
`push_emitter.rs:314:9`, `replace PushEmitter<F>::reclaim_scratch with ()` —
and it is a real gap rather than an equivalent mutant: nothing in
`tests/no_std.rs` observes scratch buffers being reclaimed.

### Accepted survivors

**`reclaim.rs:43:14` — `replace > with >=` in `should_reclaim`.**
Provably equivalent. The expression is
```rust
capacity > FLOOR && capacity > keep.max(FLOOR).saturating_mul(EXCESS_FACTOR)
```

The two operands of the mutated comparison differ only when
`capacity == FLOOR`. In that case the second conjunct asks whether
`FLOOR > keep.max(FLOOR) * 4`, and since `keep.max(FLOOR) >= FLOOR` and
`EXCESS_FACTOR` is 4, that is `FLOOR > 4 * FLOOR` — false for any positive
`FLOOR`. So the whole expression is `false` under both `>` and `>=`, and no
caller can distinguish them. Not a gap; no test can kill it.

**`engine/framing.rs:47:28` — `replace < with <=` in `find_record_ending`'s
`while search_start < input.len()` loop condition.**
Provably equivalent. The extra iteration the mutant permits slices
`&input[input.len()..]`, which is a legal empty slice rather than a panic;
`find1` over an empty slice returns `None`, and the `?` propagates that `None`
out of the function — exactly what exiting the loop does. There is no
observable difference for a test to assert on.

**The seven `self.block_cache = BlockCache::new()` reset sites in
`engine/cursor.rs` and `engine/access.rs`.**
All seven survive deletion, and this was investigated at length because it
looks exactly like a real gap. It is not. Instrumentation that recomputed the
mask on every cache hit and compared it against the cached one found, over a
full suite run: 0 stale-byte hits at `cursor.rs:685` (28,783 hits — it rewinds
within one window, and the compaction and rebase paths clear the cache before
any offset is reused), and 3 post-compaction stale-byte hits at `cursor.rs:1150`
out of 371,184. Even at 1150 the difference is unobservable from outside,
because a stale mask can only carry *extra* candidate bits, and
`StructuralBlock::next_match` re-reads the current byte before dispatching on
it, so an extra bit is a no-op unless that byte really is structural. A bounded
search found no case where a stale mask is missing a bit it needs. The resets
are therefore defensive; `tests/block_cache.rs` holds a streaming-versus-slice
differential that guards the property the cache is supposed to preserve.

### Analyzed and fixed
**`reclaim.rs:21:24` — `replace * with +` in `const FLOOR: usize = 8 * 1024`.**
This one was real, and it is the archetype worth remembering: every test in
`reclaim.rs` was written in terms of `FLOOR` (`Vec::with_capacity(FLOOR * 2)`,
`reclaim(&mut buffer, FLOOR)`), so all of them kept passing when `FLOOR` itself
changed from 8192 to 1032. Tests phrased in terms of a constant cannot detect
that constant moving. Fixed by
`the_tuning_constants_hold_their_documented_values`, which pins the values
absolutely and checks a buffer size that only a mis-set floor would reclaim.

**`engine/framing.rs:51` — the CrLf record-ending confirmation.**
`at > start && input[at - 1] == b'\r'`. Both `> `→`>=` and `&&`→`||` survived,
because no test put a bare `\n` at exactly the start of the search window under
a CrLf dialect — the one position where `at - 1` underflows. Fixed by
`find_record_ending_rejects_a_bare_newline_at_the_window_start`.

**`engine/framing.rs:176` — null-awareness on the borrowed record path.**
`.with_null_aware(!header && self.nulls != Nulls::None)`. All three mutants
(`&&`→`||`, deleting the `!`, `!=`→`==`) survived, which meant nothing exercised
this path with a null policy configured at all. Killing them needs the truth
table rather than one case, so there are now three tests: a data record under a
null policy (aware), the header record under the same policy (not aware), and a
data record under `Nulls::None` (not aware).

**`index/format.rs:178` — the `io::ErrorKind::Interrupted` retry.**
Three mutants survived on the match guard. `Interrupted` is not a failure and
must be reissued, or a signal during index verification looks like a corrupt
index; conversely a real error must surface rather than spin forever. Neither
direction was tested. Fixed by
`an_interrupted_read_is_retried_rather_than_reported` and
`a_real_io_error_is_reported_rather_than_retried`.

Sampled files with no remaining survivors: `filter.rs`, `record.rs`,
`slice_parser.rs`, `index/format.rs`, and `engine/framing.rs` apart from the
equivalent mutant recorded above.

## What to do with a survivor

1. Write the test that kills it, if it is a gap. That is the whole point.
2. If it is equivalent, add it above **with the argument**, not with an
   assertion that it is fine.
3. If it is unreachable for a reason DESIGN.md already establishes, add it to
   `exclude_re` in `.cargo/mutants.toml` citing that reason, so a future reader
   can tell an accepted exclusion from a forgotten one.

## Fuzzing

Ten coverage-guided targets run daily; `crates/coseva/tests/__fuzz__/campaign.toml`
is the source of truth for the list and `crates/coseva/scripts/fuzz_campaign.py`
runs one. Every target is also an ordinary bounded `#[test]` that replays its
committed corpus on every `cargo test`, so a crash found once is a regression
test forever: minimize the artifact the campaign writes to
`tests/__fuzz__/<target>/crashes/` and commit it into the matching `corpus/`.

The campaign runs libFuzzer without AddressSanitizer. That is not an oversight
— `campaign.toml` documents the `cargo-bolero` defect that makes ASan
unlinkable on every toolchain tried — but it does mean the campaign detects
panics, assertion failures and hangs rather than memory errors. The `miri` job
covers the unsafe kernels for undefined behavior on every pull request instead.
