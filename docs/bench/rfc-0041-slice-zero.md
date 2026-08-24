# RFC-0041 slice-zero measurements

Status: preliminary. This is evidence about the embedded compiler boundary and one captured
Linux cursor-budget run, not the go/park decision. Sustained ingest and a supported-release binary
comparison are still required.

## Captured Horizon corpus

The first real slice-zero input is a read-only snapshot of the sealed Horizon nest on the Lodestar
VPS, copied on 2026-08-24 to a local measurement directory outside Git. The copied manifest is:

```text
sha256 cabadf2f5e6d061e702afabba910dc1b7bcae7d45192792347327da1eb022303
```

All 9,476 manifest-listed Parquet segments verified against their declared SHA-256 hashes. The
snapshot is 371 MB locally. Its relation is the current delegated position per `(indexer,
delegator)`, restricted to indexers present in Horizon allocation history. It is built from the
actual sealed `staking__tokens_delegated`, `staking__delegated_tokens_withdrawn` and
`service__allocation_*` inputs, with exact decimal/integer arithmetic.

There is schema drift worth stating plainly: this VPS corpus predates the current repository views,
which name `extension__stake_delegated`. The measurement lowerer must normalise the captured
`staking__…` events explicitly. It must not silently query the newer view definitions against an
older corpus and mistake an absent table for an empty relation.

The manifest-bound normalisation and parity run is an ignored test because the 371 MB fixture is not
committed. Run it with:

```text
NUTHATCH_HORIZON_FIXTURE=/path/to/segments \
  cargo test --lib authored_entity_spike::tests::captured_horizon_relation_matches_embedded_dbsp \
  --locked -- --ignored
```

The 2026-08-24 capture produced 876 eligible delegation positions across 48 indexers. DuckDB and
the embedded DBSP circuit matched byte-for-byte after canonical key ordering.

The release-measurement command writes a JSON artefact and requires no RPC, compiler, network fetch
or running Nuthatch service:

```text
nuthatch bench authored-entity --segments /path/to/segments --max-rows 1000 \
  --out docs/bench/rfc-0041-horizon-linux.json
```

Run this using the CI-built Linux release artefact. The Lodestar VPS intentionally has no Rust
toolchain, so it executes the artefact but does not build it.

## Linux cursor budget

Measured on 2026-08-24 on the ThinkPad staging host: Linux x86_64, Rust 1.95.0, release build
from `f7e056065c71dd78645ed8c112a19d105e8e2baf` plus the uncommitted slice-zero spike. The source
and manifest-verified fixture were staged under `/tmp`; neither the working checkout nor the
Lodestar VPS was modified. The host compiler defaults to C23, so the build used
`CFLAGS=-std=gnu17` for the pinned `mimalloc-rust-sys 1.7.2` dependency, which still uses the
removed `ATOMIC_VAR_INIT` macro.

| measure | value |
| --- | ---: |
| declared maximum rows | 1,000 |
| accepted input rows | 876 |
| result rows | 876 |
| elapsed time | 284 ms |
| fixed RSS | 77,064 KB |
| peak RSS | 234,372 KB |
| approximate incremental RSS per input row | 183,885 bytes |

The circuit result contained all 876 expected rows. This is comfortably below the 2 GB cursor
budget for the captured corpus, but it is a single cold-process measurement, not a throughput
claim or a release gate.

## Recorded entity-input replay

The sealed Parquet capture is normalised once into a content-addressed sequence of weighted entity
input batches. This is deliberately not an RFC-0039 RPC tape: inventing `eth_getLogs` responses for
a sealed Horizon corpus would make the benchmark look end-to-end while exercising no real source.
The tape starts at the actual slice-zero ingestion boundary, `Spike::apply`, and a replay performs
no DuckDB scan, fixture access, RPC call or network operation.

Record and immediately measure a tape:

```text
nuthatch bench authored-entity --segments /path/to/segments --record /path/to/tape \
  --batch-rows 256 --max-rows 1000 --out replay.json
```

Replay it later without the fixture:

```text
nuthatch bench authored-entity --replay /path/to/tape --max-rows 1000 --out replay.json
```

On the ThinkPad Linux x86_64 release build, the manifest-verified Horizon capture produced a tape
with SHA-256 `00edced52ed7b676eff86c65cf043169c9285e9cc158ae20090599893925aa09`: one indexer-dimension
batch plus four 256-row delegation batches. Five standalone tape-only processes all reproduced the
876 expected entity rows. Their median was 55 ms, 15,927 delegation input rows/sec, 41,888 KB fixed
RSS and 44,028 KB peak RSS.

Those are circuit-ingestion figures, not a claim about Nuthatch's existing RPC/decode/store
throughput. The product lifecycle does not yet feed authored entities from `indexer.rs`; that is
slice #821. The tape is evidence that the candidate maintained-state boundary is fast and
deterministic, not evidence that the lifecycle work is already done.

## Release binary delta

The supported-architecture comparison was measured on 2026-08-24 on the ThinkPad staging host:
Linux x86_64, Rust 1.95.0 (`59807616e`), `f7e056065c71dd78645ed8c112a19d105e8e2baf` against the
current uncommitted slice-zero spike. Both builds used separate clean target directories and:

```text
CFLAGS=-std=gnu17 cargo build --release --locked
```

The C17 setting is required only because the host compiler defaults to C23 and the pinned
`mimalloc-rust-sys 1.7.2` source uses `ATOMIC_VAR_INIT`. It is not an application build option.

| build | bytes | SHA-256 |
| --- | ---: | --- |
| base | 97,369,248 | `f794e12587473b4dc1ad2d6c7fdce92b57a898860a8fb2c785779d15d7c4c817` |
| slice-zero spike | 99,228,128 | `db05d15be2d54bc7aabff193fc79c262235ad0f95e2b9a950ed2b07daaeec442` |
| delta | 1,858,880 | |

This Linux result is the one relevant to release. It is materially larger than the earlier local
macOS arm64 delta below, so the Linux figure supersedes that figure for the 3.0 decision. The spike
still adds no dependency, external executable, generated code, JVM, compiler service, network
fetch, Cargo invocation at circuit load, or installed toolchain requirement.

Measured on 2026-08-24 from `f7e056065c71dd78645ed8c112a19d105e8e2baf` plus the uncommitted
slice-zero spike, on macOS arm64 with Rust 1.95.0 (`59807616e`). Both builds used:

```text
cargo build --release --locked
```

Each build had a separate clean target directory. The base was a detached worktree at the commit
above. The spike adds no dependency, external executable, generated code, JVM, compiler service,
network fetch, Cargo invocation at circuit load, or installed toolchain requirement.

| build | bytes |
| --- | ---: |
| base | 81,160,320 |
| slice-zero spike | 81,178,000 |
| delta | 17,680 |

This is a local development binary, not the published Linux artefact. It establishes that adding
the AST-gated circuit does not accidentally pull in a second runtime. The release gate must repeat
the comparison on the supported Linux build before publication.

## Still required for the decision

1. Select the Lodestar delegation relation by captured raw-history scan cost and save its finalized
   corpus plus its tape content address.
2. Run DuckDB parity over that corpus and the embedded circuit, with canonical key ordering.
3. At the declared `max_rows`, record empty-circuit RSS, peak cursor RSS and the approximate
   per-row cost. The whole cursor, not merely DBSP, must remain below 2 GB.
4. Replay the same tape through the normal ingest path, recording sustained throughput against the
   existing floor.
5. Repeat the binary measurement on the Linux release artefact and make the explicit go/park
   decision. Until then, slices #820 through #822 remain blocked.
