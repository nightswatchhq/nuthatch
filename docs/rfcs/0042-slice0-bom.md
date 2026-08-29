# RFC-0042 slice 0: native bill of materials and DuckDB role inventory

Measured 2026-08-29. Linux x86_64, the release target. Commit `c081cae` (v3.0.0), rustc 1.95.0 as
pinned. Clean release build, `CARGO_TARGET_DIR` private to this run.

Slice 0 asks four things: what native code ships, why, what it costs, and what DuckDB is actually
*for*. It does not propose replacing anything.

## 1. What compiles native code into the binary

Ground truth is object files produced under each crate's build directory, not crate names.

| crate | objects | native artefact bytes |
| --- | ---: | ---: |
| **`libduckdb-sys`** | **352** | **245,073,046** |
| `ring` | 60 | 15,229,428 |
| `zstd-sys` | 37 | 3,122,374 |
| `mimalloc-rust-sys` | 1 | 414,758 |
| `ittapi-sys` | 2 | 355,072 |
| `wasmtime`, `wasmtime-internal-jit-debug` | 2 | 9,192 |

**DuckDB is 93% of native artefact bytes.** That part of §1's premise holds comfortably.

## 2. What it costs in build time, and this is where the premise weakens

Clean release build: **223 s wall**, 872 units, 1,257 s summed unit time (parallel, so the sum exceeds
wall). Attributed from `cargo build --timings`:

| group | seconds | share | units |
| --- | ---: | ---: | ---: |
| **wasmtime + cranelift + wasm\*** | **267.4** | **21.3%** | 69 |
| duckdb (incl. `libduckdb-sys`) | 133.4 | 10.6% | 4 |
| dbsp + feldera | 108.0 | 8.6% | 8 |
| arrow + parquet | 100.8 | 8.0% | 15 |
| ring / zstd / mimalloc / ittapi | 65.5 | 5.2% | 20 |

`libduckdb-sys` is the largest **single** unit at 132.1 s. But **the transform runtime costs twice
what DuckDB does**, and DuckDB is 10.6% of the build rather than a dominating share.

> **This qualifies RFC-0042 §1.** The RFC says a bundled DuckDB build "affects clean-build time, disk
> use, cross-compilation and contributor experience" and that "a dependency which dominates time, disk
> and portability complexity must justify itself continuously". On disk it dominates: 93% of native
> bytes. **On build time it does not dominate**, and removing it would leave the larger consumer in
> place. §13's second gate exists for exactly this: if the share is small, the board should hear it
> before anything else is spent.

## 3. The C++ question, from the link rather than from crate names

The **published** `v3.0.0-alpha.1` Linux artefact - not a local build, which would carry the build
host's floors:

```
dynamic deps: libstdc++.so.6, libgcc_s.so.1, libm.so.6, libc.so.6
highest GLIBC:   GLIBC_2.34
highest GLIBCXX: GLIBCXX_3.4.29
highest CXXABI:  CXXABI_1.3.13
```

**The binary dynamically links `libstdc++`**, which is the concrete form of "C++ ships". It gives
Tier 2 a checkable definition and a real payoff: **remove DuckDB and the binary should depend on libc,
libm and libgcc only.** That is a better statement of the benefit than purity.

Two documentation findings fall out:

- `GLIBCXX_3.4.29` (GCC 11+) is **undocumented**. README names a glibc floor and nothing else. Every
  platform it lists clears it, so this is incompleteness rather than a broken promise.
- The measured GLIBC floor is **2.34**, and README says 2.35. Conservative, therefore safe, and worth
  leaving alone rather than tightening a number that costs nothing.

Final binary: **102,359,456 bytes** (97.6 MB), unstripped.

## 4. The DuckDB role inventory: six sites, not §9's four

The deletion checklist. §9 named four roles; walking the call sites finds six, and two are
**product-visible**.

| site | role | classification | notes |
| --- | --- | --- | --- |
| `analytics.rs` | general SQL, views, hot+cold federation | production | 53 connection ops, the obvious one |
| `entities.rs` | **admissible function vocabulary** from `duckdb_functions()` | **production, public contract** | its own comment: "the same catalogue the binder uses". The SQL a nest may declare *is* DuckDB's function list |
| `entity_lower.rs` | AST for lowering authored SQL to a DBSP circuit | production | RFC-0041 parser role |
| `graft.rs` | **writes the engine string into grafting identity** (`engine: "duckdb-v1.4.0"`) | **production, migration consequence** | grafts already recorded name the engine (RFC-0033) |
| `seal.rs` | segment-binding oracle | test-only | one in-memory connection in a fixture |
| `authored_entity_spike.rs` | RFC-0041 slice-zero spike | **production, measurement-only** | `pub mod` in `lib.rs`, reachable via `nuthatch bench`. A naive read files this as test-only; it ships |

The two product-visible ones are what make "remove DuckDB" more than an implementation change. The
function vocabulary decides what a user may write in `entities.toml`; the graft engine string is
already written into artefacts on disk.

## 5. Noise floor and covariates

Per the RFC's method amendment:

- **Segment layout is measured separately and is a confound** - see `docs/bench/segment-layout.md`
  (#889). `horizon-nest` holds 10,923 files at a 6 KB median, and a file costs 0.14-0.18 ms on a
  `COUNT(*)`. Any query comparison must record file count and size distribution or it has two causes.
- **Peak RSS over a stated window, never a slope.** A nest's RSS sawtooths ~120 MB unaided.
- **#896 is in this baseline** (commit `c081cae`). Before it, `SELECT 1` cost 2,465 ms on a
  38,428-segment nest for reasons that were ours and not the engine's.

## 6. What slice 0 does not answer

- **Why** a native crate costs what it does. Compile, link and codegen are not separated.
- Any target but Linux x86_64. The aarch64-apple-darwin BOM is not run.
- Whether removing DuckDB reaches Tier 2 in practice. It is *plausible* from the linkage evidence and
  unproven until something builds without it.
