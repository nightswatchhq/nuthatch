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

## 3a. The second target: aarch64-apple-darwin (#948)

Measured 2026-08-29, same commit, same pinned 1.95.0, on Apple Silicon. Not a translation of the Linux
run: the C++ evidence there is `libstdc++.so.6` in `ldd`, here it is `otool -L`, and the whole toolchain
differs (`stat -f%z`, no `find -printf`).

| | Linux x86_64 | macOS aarch64 |
| --- | ---: | ---: |
| clean release build | 223 s | **154 s** |
| duckdb share of build | 10.6% | **8.0%** |
| wasmtime + cranelift | 21.3% | **16.6%** |
| `libduckdb-sys` objects | 352 | 352 |
| `libduckdb-sys` bytes | 245 MB | **149 MB** |
| final binary | 102.4 MB | **87.7 MB** |

**The conclusion holds on both targets, and the ratio holds more tightly than the absolutes.** DuckDB
is not the largest build-time consumer on either; wasmtime and cranelift cost roughly twice as much on
both. §1's premise fails the same way in both places.

### The C++ dependency is real on macOS too - and cheaper

```
/usr/lib/libc++.1.dylib
/usr/lib/libSystem.B.dylib
/System/Library/Frameworks/{Security,CoreFoundation}.framework/...
```

So Tier 2's definition is target-independent: the binary links a C++ runtime on both.

**But its cost is not.** On Linux, `libstdc++` brings a real constraint - `GLIBCXX_3.4.29`, an ABI floor
that had to be documented (#946) and excludes a system with new glibc and old libstdc++. On macOS,
`libc++` lives in `/usr/lib` and is always present at a version the OS guarantees. There is no floor to
state and nothing for a user to install.

**Therefore the portability half of §1's argument is a Linux argument.** Removing DuckDB would delete an
ABI floor on one target and change nothing a user can observe on the other. Worth knowing before
"cross-compilation and portability complexity" is weighed as though it applied evenly.

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

## 4a. What a DataFusion port would and would not address (#891, from RFC-0043)

Carried across as input rather than rediscovered, which is what #891 exists for. RFC-0043 §5 mapped
Amp onto the four roles §9 knew about; slice 0 found six. Extending the mapping to all six:

| role | would a DataFusion port address it? |
| --- | --- |
| general SQL, views, hot+cold federation (`analytics.rs`) | **yes** - a real existence proof, minus the incremental layer underneath |
| admissible function vocabulary (`entities.rs`) | **no.** Amp has no authoring surface. Swapping engines swaps the vocabulary a nest may declare |
| AST for lowering authored SQL (`entity_lower.rs`) | **no.** Amp has no incremental layer to lower into |
| engine string in grafting identity (`graft.rs`) | **no.** Amp has no entity state to graft |
| segment-binding oracle (`seal.rs`) | test-only; replaceable by anything that parses Parquet |
| RFC-0041 spike (`authored_entity_spike.rs`) | measurement-only; follows whatever the reference becomes |

RFC-0043 §5's summary was **one of four roles, partially**. Against slice 0's fuller inventory it is
**one of six, partially, plus one that is only test-only anyway**. The honest size did not improve on
closer inspection.

### The baseline any re-run is measured against

**RFC-0013 §4, run 2026-08-02: DataFusion at 1.6-2.7x DuckDB's latency, the gap widening as segments
grow, at exact result parity.** That is the number a slice-2 spike is compared to, and RFC-0013 §2
already named DataFusion the long-term destination on architectural grounds, so the destination was
never in dispute - latency on our workload was.

**None of the published Amp figures may stand in for it** (RFC-0043 §9). The vendor numbers are
marketing; the fork's self-report is n=3 on a shared dev box and RPC-bound by its author's own
statement. Four million events per second and one hundred and thirteen blocks per second appear in the
same document and are not obviously the same system.

One thing from that report *is* useful and cuts against engine work generally: **backfill is
RPC-bound**, corroborated from outside by near-identical throughput between two systems sharing an
`evm-rpc` client. Any engine migration justified by backfill numbers is measuring the wrong thing.

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
- ~~Any target but Linux x86_64.~~ **Both release targets are now measured** - see §3a (#948).
- Whether removing DuckDB reaches Tier 2 in practice. It is *plausible* from the linkage evidence and
  unproven until something builds without it.
