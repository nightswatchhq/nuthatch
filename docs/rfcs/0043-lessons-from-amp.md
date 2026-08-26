# RFC-0043: Lessons from Amp - what a competitor's architecture does and does not tell us

- Status: **Reference and analysis. No implementation, no decision.** Under the 2026 feature freeze
  this is a document to argue with, not work to start. It is **not** a third carve-out, and it does
  not start, reorder or unblock any slice. Its only operational output is §10's short list of
  candidate issues, each of which must stand on its own under the freeze's bug-fix, security,
  performance and maintenance allowance.
- Author: Jenny
- Date: 2026-08-26
- Depends on: RFC-0013 (the DataFusion benchmark gate this document is set against), RFC-0041 (the
  four DuckDB roles), RFC-0042 (the investigation this feeds as input), RFC-0009 (content-addressed
  sealed segments), RFC-0029 (backfill throughput and the RPC bound), RFC-0001 (versioned decodings).
- Origin: a research report on Edge & Node's Amp delivered to the board on 2026-08-26, read against
  our own RFCs rather than accepted at face value.
- Blocks: nothing. It explicitly does **not** unblock RFC-0042, whose §9 sequencing is unchanged.

## §0 - Why write this down at all

Amp is the closest public system in shape to Nuthatch: a single Rust binary that ingests EVM data,
stores columnar segments, and serves SQL from an embedded engine. When something that close appears,
two failure modes are available. The first is to ignore it. The second, more expensive, is to treat
its architecture as a verdict on ours and start re-litigating decisions we have already measured.

This RFC exists to take the useful parts, name the parts that conflict with our own correctness
rules, and record - once, with the numbers attached - **why a competitor's engine choice is not a
benchmark result.** The next time someone proposes moving to DataFusion because Amp did, the answer
should be a link, not another argument.

> **A competitor's architecture is evidence about their requirements. Only a measurement on our
> workload is evidence about ours.**

## §1 - What Amp is

Edge & Node's Amp (`ampd` daemon, `ampctl` CLI), unveiled 2025-11-05, marketed as a
"blockchain-native database". The pipeline, as publicly documented:

| Stage | Amp |
|---|---|
| Extract | pluggable providers: batched `evm-rpc`, Firehose gRPC, Pinax's public Firehose-to-Parquet buckets, Solana, `eth-beacon` |
| Transform | Apache DataFusion, extended with EVM UDFs (`evm_decode_log`, `evm_topic`, `eth_call`, …) |
| Store | Apache Parquet "segments" over a block range, on local disk or object store, `zstd(1)`, with a background compactor and collector |
| Serve | Arrow Flight / FlightSQL, JSON Lines over HTTP, and a read-only Postgres wire endpoint |
| Coordinate | **an external PostgreSQL** holding datasets, manifests, providers, jobs, workers, file metadata and progress, with `LISTEN/NOTIFY` for distributed workers |

Datasets are addressed `namespace/name@version`; manifests pin the exact Arrow schema. Reorgs are
handled by serving consistent "revisions" of immutable files and swapping the active view with a
single metadata update. One binary runs any role: `dev`, `server`, `controller`, `worker`, `migrate`.

That is a good design. It is also, on inspection, a design for a different product.

## §2 - Provenance: what counts as evidence here

The report that prompted this RFC nominates two artefacts as its strongest evidence. Both need
discounting before use, and the discount is not small.

1. **`nightswatchhq/camp-node`**, put forward as "the highest-signal artifact available", is a
   repository in **this project's own GitHub organisation**. Nuthatch is `nightswatchhq/nuthatch`.
   Whatever camp-node is worth to us, it is not outside corroboration, and a recommendation to go
   and discover it is a recommendation to read our own shelf.
2. **The "independent" benchmark** (The Graph forum, 2026-06-09, camp-node v0.5.1 against public
   amp v0.0.36) is written by that fork's own maintainer, who is candid about it: "indicative, not
   publication-grade", a shared dev box, n=3, and a backfill the author states is RPC-bound, since
   camp inherited amp's `evm-rpc` client. He is right on every count, and the honesty is exactly
   why the numbers are usable at all - but they are a fork's self-report, not a third party's.

Removing those two leaves a launch blog, a product docs site, and a community explainer series.
That is enough to learn an architecture from. It is not enough to overturn a measurement.

**Rule for this file: any claim below is labelled vendor, fork-self-report, or ours.**

## §3 - The licence boundary, and the reading discipline

Amp and its public fork are **BUSL-1.1**: source-available, not open source, with a non-compete
Additional Use Grant and a per-version Change Date after which that version becomes Apache-2.0.

Our own dependency rule got *stricter*, not looser, when the core relicensed to `MIT OR Apache-2.0`:
we can no longer consume copyleft or otherwise-encumbered source at all. BUSL sits firmly outside
what may enter this repository.

What is fine, and normal practice:

- reading public BUSL source, building it, running it;
- black-box observation of a public binary: gRPC reflection, Flight schemas, wire formats, CLI
  behaviour, `strings`;
- reading published protobufs, docs and UDF signatures.

What is not:

- copying or transliterating BUSL-licensed code into Nuthatch, at any size;
- decompiling or disassembling the closed binary to recover private internals;
- routing around the token gate on private repositories or releases.

And one procedural guard the report does not mention, which is where contamination actually happens:
**the person who reads their implementation should not be the person who then writes ours**, or if
that is impractical, a design must be written down from the public *documentation* first and
implemented from that document. Any idea taken from public Amp material gets a provenance line in
the RFC or issue that introduces it. This paragraph is not legal advice; it is the cheap discipline
that keeps the question from ever being interesting.

## §4 - Where Amp and Nuthatch actually differ

The single most useful output of this exercise is the realisation that the systems are not solving
the same problem, which is why their engine choice does not transfer.

| | Amp | Nuthatch |
|---|---|---|
| Mutable tip | none - small Parquet files written at the tip, cleaned up by a compactor | **redb hot store**, sealed to Parquet only past finality |
| Reorg | serve a different immutable revision, swap active view via metadata | roll back the hot store of the affected chain's cursor; DBSP retractions; **sealed segments never mutate** |
| Decode | **at query time**, as SQL UDFs over a raw `logs` table | deterministic Rust at ingest, topic0-keyed, stored decoded and **versioned** |
| Derivation | none. SQL views over raw tables | built-in DBSP IVM, authored SQL views, authored incremental entities (RFC-0041) |
| Metadata / coordination | **external PostgreSQL, mandatory** | in-process, embedded |
| Serving | Arrow Flight, JSON Lines, Postgres wire | HTTP entity point-reads, analytical SQL, MCP |
| Deployment | one binary **plus a database** | one binary, non-negotiable #1 |

Read the first two rows together and the engine choice explains itself. Amp has no mutable state and
no incremental layer, so its whole product surface is *a scan with clever functions bolted on*. It
needs an engine that lets you bolt things onto a scan. DataFusion is exactly that engine, and it is
the right call for them.

Our defining requirement is different: federate a mutable hot store with immutable cold segments,
under an incremental layer that must produce identical results to a reference. Extensibility matters
to us too, but it is not the axis the decision turns on.

## §5 - The DataFusion question: what this settles, and what it does not

**Ours, and this is the number that matters.** RFC-0013 §4's benchmark gate was run on 2026-08-02.
DataFusion came in at **1.6-2.7x DuckDB's latency, with the gap widening as segments grow, at exact
result parity.** DuckDB stayed. The §2 destination was recorded as unmet, not repudiated.

Amp's existence does not move that figure. It tells us DataFusion is production-viable for
SQL-over-Parquet with custom UDFs and an Arrow-native wire, which was never in dispute; RFC-0013 §2
already named DataFusion the long-term destination on exactly those grounds. What was in dispute is
latency on our workload, and a competitor's architecture diagram is silent about it.

Mapped onto RFC-0042 §9, which is where the decision actually lives, DuckDB holds four roles inside
RFC-0041:

| Role | What Amp tells us |
|---|---|
| SQL parser / canonicalisation | **nothing.** Amp does not have our authoring surface |
| Incremental reference oracle | **nothing.** Amp has no incremental layer to check against |
| Finalised restart seed | **nothing.** Amp has no entity state to seed |
| Analytical entity serving | a real existence proof, minus the incremental layer underneath |

One of four, partially. That is the honest size of it. RFC-0042's slices remain the only thing that
can answer this, and they remain sequenced behind RFC-0041 for the reason §9 already gives.

**What would change the picture:** a re-run of RFC-0013 §4's gate on current DataFusion and current
Arrow, on our segments, as RFC-0042 slice zero already plans. Not a competitor's launch blog.

## §6 - What we must not borrow

The report's "what to borrow" list contains three items that collide with our own rules. Recording
them here so the collision is argued once.

**Decode as UDFs at query time.** Amp keeps one raw `logs` table and decodes inside SQL, so a single
extraction powers many decoded views with no separate decoded datasets. Elegant, and incompatible
with **RFC-0001's versioned-decodings rule**: we never retroactively re-decode stored history when
ABIs improve. Query-time decode means the day a better ABI lands on Sourcify, every historical answer
silently changes and no version boundary exists to record it. Amp can afford this because it sells a
queryable view of the chain. We sell re-executable state, and `prod-readiness.md` gates on it.

**`eth_call` inside a query.** A live remote call in the query path is non-determinism in the one
place we have said it may not live. RFC-0023 and RFC-0024 already took the opposite decision -
derive first, content-address the result, seal it, and version it like a decoding. That decision
stands and this is not new evidence against it.

**External PostgreSQL for metadata and coordination.** Straightforwardly non-negotiable #1. Worth
noting for the opposite reason: it is a **competitive asymmetry in our favour**, not a pattern to
copy. Their `dev` mode is one binary plus a database. Ours is one binary.

## §7 - What is worth having

Short list, and deliberately so.

1. **Compaction and Bloom filters on by default.** Upstream amp v0.0.36 shipped both **off**, and the
   public write-ups describe the predictable consequence: small Parquet files degrading query
   planning in production. (fork-self-report, and camp-node flipped the defaults.) Our sealing path
   is not their tip path, but "many small files accumulate and nobody notices until planning slows"
   is a failure mode we can check for cheaply. See §10.
2. **Backfill is RPC-bound.** The fork's own benchmark says so explicitly, and attributes
   near-identical throughput between the two systems to a shared `evm-rpc` client. This corroborates
   RFC-0029 and RFC-0028 from outside: the throughput lever is the extraction path, not the engine.
   Useful mainly as a caution - any engine migration justified by backfill numbers is measuring the
   wrong thing.
3. **The canonical-chain verification gap, stated publicly by Edge & Node**: hash-linking proves a
   set of headers forms an internally valid chain, but not that it is the chain network consensus
   agreed on, absent consensus-layer data. That is an honest limitation, it applies to us in exactly
   the same way, and it belongs in `verification.md`'s claims rather than being quietly assumed away.
4. **Arrow type choices** (`FixedSizeBinary(20/32)` for addresses and hashes, `Decimal128(38,0)` for
   values, nanosecond timestamps) are a sane convergent answer worth comparing our columns against
   the next time the schema is opened. Not urgent.

## §8 - What we were told to build and have already shipped

Recorded because it is the clearest sign of how far to trust a summary written from outside.

- **"Adopt hash-linked immutable segments."** RFC-0009 already content-addresses sealed segments:
  the file is `{table}-{hash}.parquet`, with the `registry_snapshot` hash written into each seal
  manifest. RFC-0011 goes further and uses cross-operator segment-hash equality as a free determinism
  check between the GraphOps host and the Hetzner shadow.
- **"Keep redb as the mutable tip buffer, sealing to Parquet only at finality."** That is the
  standing architecture and has been since slice 2.
- **"Ship a compactor."** Partially; see §10.

## §9 - The numbers, and how far to trust them

| Claim | Source | Use |
|---|---|---|
| 5.9x faster than BigQuery public datasets; 2.3x better storage than full-node DBs; >4M events/sec; 100x freshness (1s vs 101s); >4,300x backfill; 98% smaller than archive nodes; 747 ms median freshness | vendor marketing | **do not cite** |
| 44.2s vs 51.2s wall clock; 113 vs 98 blocks/sec; 150 vs 157 MB peak RSS; 1931 vs 1990 bytes/block, over a 5,000-block Arbitrum window | fork self-report, shared dev box, n=3, RPC-bound by the author's own statement | indicative only |

Both figures appear in the same report. Four million events per second and one hundred and thirteen
blocks per second are not obviously the same system. Neither number was produced under conditions we
would accept from ourselves under RFC-0004, and neither should appear in our marketing, our docs, or
an argument about engines.

## §10 - What this changes, and the follow-ups

**Sequencing: unchanged.** RFC-0042 stays behind RFC-0041. Nothing in the public Amp material
addresses §9's parser, reference-oracle or restart-seed roles, which are the reason for the
sequencing in the first place. Reading costs nothing and may happen at any time; building does not
start until the entity work is done.

Candidate issues, each justifiable under the freeze on its own terms and none of them new capability:

1. **Check our sealed-segment file-count and planning behaviour** on a long-running nest, and confirm
   whether compaction and any Parquet Bloom/statistics options are on by default and effective.
   Maintenance and performance. (§7.1) - filed as **#889**.
2. **Add the canonical-chain provenance limitation to `verification.md`** as a stated non-claim, so
   the verified-by-us table stays honest. Documentation. (§7.3) - filed as **#890**.
3. **A single competitive line for the site**: nuthatch's embedded mode requires no external database;
   the nearest comparable system requires PostgreSQL for metadata and coordination. Fits the honest
   comparison table rather than the feature list. Marketing. (§6) - not filed here; it belongs to
   `nuthatch-frontend`, and the honest comparison table is edited there.
4. **Feed §4's difference table and §5's role table into RFC-0042 slice zero** as input, so the
   inventory does not have to rediscover which DuckDB roles a DataFusion port would and would not
   address. (RFC-0042 §4) - filed as **#891**, blocked by design.

Not proposed, and listed so nobody proposes them as though they were free: Arrow Flight serving,
Postgres wire serving, query-time decode UDFs, and a distributed worker split behind PostgreSQL. The
first two are new capability under the freeze and are RFC-0042-adjacent at best; the third is §6; the
fourth is RFC-0022's territory and is not started.
