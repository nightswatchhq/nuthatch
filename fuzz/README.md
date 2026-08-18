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
cargo +nightly fuzz run abi_json
cargo +nightly fuzz run abi_arbitrary
cargo +nightly fuzz run decode_log
```

Requires `cargo install cargo-fuzz` and a nightly toolchain (libFuzzer's sanitizer coverage
instrumentation isn't available on the pinned stable 1.95.0 - see the toolchain note in
`../Cargo.toml`). This is dev-only tooling: `fuzz/` is excluded from the published crate and never
sits in the runtime data path.

**No sanitizer/debug-assertions workaround needed as of nuthatch#581.** These targets used to
require `-O --sanitizer none`: nightly's trait solver ICEs computing a vtable slot for dbsp's
`StarJoinFuncTrait`/`DynClone` impl whenever the build carried debug-assertions *or* ASan on top of
SanitizerCoverage instrumentation - a real rustc bug, reproduced on both a 2026-06-22 and a
2026-08-13 nightly, nothing to do with nuthatch's own code. #581 extracted the decode path
(`registry.rs`/`rpc.rs`) into the standalone `nuthatch-decode` crate this crate now depends on;
`dbsp` is not in `nuthatch-decode`'s or `nuthatch-fuzz`'s dependency graph at all
(`cargo +nightly tree -p nuthatch-fuzz | grep -c dbsp` is `0`), so the ICE cannot be reached
regardless of flags. ASan and debug-assertions run by default again. `overflow-checks = true` (set
in this crate's `[profile.release]`) is unaffected either way and remains the property this target
actually needs - a `uint256[huge]`-shaped size computation must panic, not silently wrap.

CI runs all three under a bounded `-runs=300000 -max_total_time=180` smoke pass on every push/PR
(`fuzz-smoke` in `.github/workflows/ci.yml`) - not a real fuzzing campaign, a regression tripwire.
For an actual campaign, run locally or in a dedicated job with a much larger time budget and let
the corpus grow (`cargo +nightly fuzz run <target> -- -max_total_time=3600`).

## Verifying the harness actually reaches decode

An acceptance criterion phrased as "no crash found" passes trivially if the harness silently isn't
reaching the code it claims to fuzz - this project has been bitten by that shape of false-green
five times (CLAUDE.md's docs-go-stale lesson generalises). Before trusting a green fuzz run, prove
it reds on a known-bad build: reintroduce a fixed panic (e.g. revert `registry.rs`'s
`u.saturating_to::<u64>()` guard, COR-11, to an unchecked `.to::<u64>()`) and confirm
`cargo +nightly fuzz run decode_log` finds a crashing input against the seeded corpus, then revert.
That check isn't automated here - a permanently red "this must find a bug" target isn't buildable -
so re-run it by hand after any change to the fuzz harness itself.

**Status as of 2026-08-18, re-run post-nuthatch#581: the live proof still doesn't find a crash, and
now the reason is diagnosed rather than blocked on the ICE.** With dbsp out of the dependency graph
the build itself is no longer the obstacle - `cargo fuzz build decode_log` with ASan and
debug-assertions on (the defaults, no flags) finished in 26s, no ICE. But 300,000 runs
(`-runs=300000`, 21s wall clock) of `decode_log` against the COR-11-reverted code found nothing.
The cause isn't fuzzer luck: none of this target's five fixture ABIs (`Transfer.value`,
`Collection.pair.b`, `HugeArray.data`'s element type, `Deep`'s nested leaf) declare a uint narrower
than `uint256`, so `value_from_dynsol`'s `*bits <= 64` branch - the one line the COR-11 guard
lives on - is structurally unreachable from this harness no matter how long it runs. This is a
coverage gap in the fixture set, not a mutation-budget problem; fixing it means adding a fixture
event with a `uint8`/`uint32`/`uint64`-typed field, not fuzzing longer. Filed as a follow-up rather
than fixed here, since this PR's job was getting dbsp out of the graph, not auditing fixture
coverage.

**The cheaper half is still proven, and doesn't depend on the fuzzer at all.** The COR-11 *oracle*
- that `value_from_dynsol` catches a dirty-high-bits uint and does not panic - is exercised by a
plain `cargo test` in `decode/src/registry.rs`
(`registry::tests::dirty_high_bits_on_a_sub64_uint_saturate_instead_of_panicking`), no nightly
toolchain and no sanitizer required. It runs a crafted log (a `uint64` field with a 32-byte word of
`0xff`) through the real `DecodeRegistry::decode` entry point. Verified live on 2026-08-18: with
the guard in place it passes; with `saturating_to::<u64>()` reverted to `.to::<u64>()` it panics.
That proves the guard itself is correct - it does not prove libFuzzer's mutation engine would ever
find that input on its own, which the fixture-coverage gap above explains it can't, structurally.
