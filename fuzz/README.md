# Fuzzing the decode path (nuthatch#290)

Decode is the one place untrusted input meets stored state (CLAUDE.md §4): an ABI is
attacker-supplied the moment a nest is pulled from a registry, and a log is whatever an RPC
endpoint hands back. These targets hunt **panics and unbounded allocation**, not wrong answers -
a `Result::Err` or an `Ok(None)` is always an acceptable outcome; a crash or an OOM abort is a
finding.

## Targets

- **`abi_json`** - raw fuzzer bytes interpreted directly as ABI JSON text. Finds malformed
  *shapes* (wrong JSON types, missing fields, truncated documents) that a structured generator
  would never emit. Seeded from `corpus/abi_json/`.
- **`abi_arbitrary`** - a small `Arbitrary`-derived generator that reliably reaches the shapes raw
  byte mutation rarely stumbles into: absurd tuple depth (iteratively nested, so the generator
  itself can't stack-overflow first), duplicate topic0 (repeated identical event signatures), and a
  `uint256[huge]` fixed-array param.
- **`decode_log`** - a fixed, curated set of ABIs (ordinary indexed/non-indexed params, an indexed
  `string` that arrives as a topic hash, a dynamic array + tuple, a `uint256[4000000000]` fixed
  array, and a tuple nested 512 levels deep) decoded against a fuzzed `Log`: arbitrary topic count
  and length, truncated/garbage `data`, and corrupted hex. `topic0` is left fixed per fixture -
  it's resolved ahead of time by whoever built the registry, not something a byzantine RPC response
  or a malicious log actually controls.

## Running

```sh
cargo +nightly fuzz run abi_json -O --sanitizer none
cargo +nightly fuzz run abi_arbitrary -O --sanitizer none
cargo +nightly fuzz run decode_log -O --sanitizer none
```

Requires `cargo install cargo-fuzz` and a nightly toolchain (libFuzzer's sanitizer coverage
instrumentation isn't available on the pinned stable 1.95.0 - see the toolchain note in
`../Cargo.toml`). This is dev-only tooling: `fuzz/` is excluded from the published crate and never
sits in the runtime data path.

**`-O --sanitizer none` is required, not optional**, until an upstream rustc bug is fixed. Without
it, `cargo fuzz build` fails outright: nightly's trait solver ICEs while compiling `dbsp` (checked
against both a 2026-06-22 and a 2026-08-13 nightly, so this isn't a one-off regression) whenever
the build carries debug-assertions *or* ASan on top of SanitizerCoverage instrumentation - it
crashes resolving a vtable slot for `StarJoinFuncTrait`'s `DynClone` impl, deep in dbsp's
type-erased operator graph, nothing to do with nuthatch's own code. `overflow-checks = true`
(set in this crate's `[profile.release]`) survives on its own and is the property this issue
actually needs - a `uint256[huge]`-shaped size computation must panic, not silently wrap - so `-O`
only drops the *separate* debug-assertions flag cargo-fuzz otherwise adds by default. Losing ASan
gives up memory-corruption detection inside dependencies' `unsafe` blocks; `registry.rs`/`rpc.rs`
(the decode path itself) contain none, and libFuzzer's own `-rss_limit_mb`/`-malloc_limit_mb`
already catch unbounded allocation without it. To check whether upstream has fixed the ICE, drop
both flags and see if `cargo +nightly fuzz build` still panics with "the compiler unexpectedly
panicked" mentioning `StarJoinFuncTrait`.

CI runs all three under a bounded `-runs=300000 -max_total_time=180` smoke pass on every push/PR
(`fuzz-smoke` in `.github/workflows/ci.yml`) - not a real fuzzing campaign, a regression tripwire.
For an actual campaign, run locally or in a dedicated job with a much larger time budget and let
the corpus grow (`cargo +nightly fuzz run <target> -O --sanitizer none -- -max_total_time=3600`).

## Verifying the harness actually reaches decode

An acceptance criterion phrased as "no crash found" passes trivially if the harness silently isn't
reaching the code it claims to fuzz - this project has been bitten by that shape of false-green
five times (CLAUDE.md's docs-go-stale lesson generalises). Before trusting a green fuzz run, prove
it reds on a known-bad build: reintroduce a fixed panic (e.g. revert `registry.rs`'s
`u.saturating_to::<u64>()` guard, COR-11, to an unchecked `.to::<u64>()`) and confirm
`cargo +nightly fuzz run decode_log` finds a crashing input within seconds against the seeded
corpus, then revert. That check isn't automated here - a permanently red "this must find a bug"
target isn't buildable - so re-run it by hand after any change to the fuzz harness itself.
