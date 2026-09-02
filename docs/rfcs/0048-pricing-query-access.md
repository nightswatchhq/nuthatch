# RFC-0048: Pricing query access behind a gateway

- Status: **Draft. Design only, and not a carve-out.** Under the 2026 feature freeze this is a
  document to argue with, not work to start. It proposes how an operator *names the number*
  RFC-0046 left as a constant. It does not reopen §5.1 of that RFC (no facilitator in the query
  path), does not put x402 in the binary this year, and does not add a query-shape DSL.
- Author: Jenny
- Date: 2026-09-02
- Origin: a board research note on Graph-native query pricing (Agora, TAP/GraphTally, x402 `upto`,
  BigQuery bytes-scanned), read against the tree and against RFC-0046 the same day. §1 records
  where the note and the tree disagree.
- Depends on: RFC-0046 (the counter, the settlement split, the two-door decision), RFC-0034 (the
  bounded surface is what makes a query priceable), RFC-0047 (catalogue stats a quote can be
  audited against), RFC-0013 §3 (hot redb / sealed Parquet), RFC-0016 §4 (provenance stamp).
- Blocks: nothing. RFC-0046's slices stay the order of build if the freeze ever lifts. This RFC
  answers what the `402` body contains.

## Abstract

RFC-0046 lets an operator charge for a named query: verify a signature locally, serve the answer,
settle out of band. The price in that design is a number the operator wrote down. This RFC is
about whether that number should stay flat, and if not, what it may depend on.

**Ship flat per-query pricing first, gated by a deterministic byte-scan admission check, not a
full cost model.** The Graph's own operational history is decisive: Agora cost models were
abandoned in practice, indexers defaulted to `default => x;`, and Edge & Node proposed removing
Agora entirely (gateway issue #971, repo archived 2025-06-02). Nuthatch should not launch with a
query-shape DSL.

Where nuthatch genuinely differs from a subgraph is cost variance. A bounded tip lookup in redb
and an unbounded scan across sealed Parquet can differ by orders of magnitude in bytes touched.
That variance is the one place cost-based pricing earns its complexity. It maps onto a two-tier
quote (flat for proven-bounded hot/tip queries, `base + α·bytes_upper_bound` for cold analytical
ones), a hard cap at the quoted bound, and settlement that still follows RFC-0046: local verify,
back-office settle. Curation, slashing, and indexing rewards stay out.

## §0 - The lesson, and the place it does not reach

**The Graph proved that per-query cost modeling loses to flat pricing**, for reasons that were
specific to GraphQL subgraphs.

Agora is a real DSL: a sequence of `match => price` statements, first match wins, with globals
and query-shape variables. In November 2024 Theo (Theodus, Edge & Node) opened gateway issue
#971 to *remove Agora entirely*: "Indexers don't use them. And those that do require complex
automation like auto-agora… We do not have a way to reliably associate GraphQL query shapes with
the cost to execute a query." His proposal: a static price per query, in GRT, for each
`(deployment, TAP sender)`, "practically equivalent to a cost model of `default => …;`." The
Devcon talk by Semiotic's own researcher said the same: most indexers default to flat because
populating models by hand is tedious. AutoAgora's maintenance plan was "unknown/stalled". The
agora repo was archived on 2025-06-02.

That is the single most important lesson for nuthatch, and Phase 0 below is it.

**The reason flat pricing won for subgraphs does not fully hold here.** Subgraph GraphQL is
relatively bounded and homogeneous, and the gateway never actually selected on query shape, so
complex indexer models produced no price-efficiency benefit. Nuthatch serves SQL over a hot
store *and* sealed Parquet. A point read and a `SELECT *` across every segment of a busy table
are not the same job. That is the BigQuery situation, where bytes-scanned pricing exists because
an unpartitioned scan can be enormous. The variance is real. The response is not "therefore a
DSL". It is "flat, plus a hard admission bound, and a second tier only when telemetry shows the
bound is in the way".

## §1 - What the tree already does, and what the note got wrong

The research note's own caveats asked for a repo check. Here it is.

**There is no x402 in this repository.** RFC-0045 and RFC-0046 both recorded that. The working
buyer and seller live in Lodestar (`src/lib/x402.ts`, `x402-seller.ts`). "Nuthatch already has
x402 integrated" is false of this tree. RFC-0046 is the design; nothing is wired.

**The public priced surface is not arbitrary SQL.** RFC-0034's allowlist is what makes a query
priceable: callers send a name and typed arguments, never SQL. RFC-0046 §3 is explicit: an
operator may accept x402 for *named queries on a bounded surface*. A two-tier price that needs
the planner to prove a query is bounded still works, because the named query's statement is
known. It does not license opening `/sql` as a metered product. Open `/sql` remains node
self-protection (timeout, row cap, hot-row cap) and is not a payment surface.

**`parquet_metadata()` is not a quoting API.** It is on the `/sql` denylist in `analytics.rs`
(`FORBIDDEN_FNS`), with `read_parquet`, `parquet_scan`, and friends, because a table function
that reads the filesystem bypasses the read-only keyword gate. A quote computed by letting the
client, or the untrusted `/sql` path, call `parquet_metadata` is a security hole, not a
verifiability feature. The auditable bound belongs on the **catalogue** RFC-0047 versions
(segment-level stats, published, content-addressed). Host-side planning may read footers. The
query path may not.

**Admission control already exists, and it is not bytes-scanned.**

| Guard | Value | What it stops |
| --- | --- | --- |
| wall-clock | 30 s | cartesian / runaway |
| rows returned | 50,000 | result-buffer RAM |
| unsealed hot rows | 2,000,000 | deep-finality hot scan |
| query string | 16 KiB | planner DoS |
| DuckDB memory / threads | 512 MB / 2 | one query's heap |
| concurrency | 2 (ceiling 16) | the DoS multiplier |

There is **no byte-scan cap**. A named query that plans as a full cold scan of every segment of a
wide table will run until a row cap, a timeout, or memory says no. Phase 0's admission check is
the missing one, and it is the one BigQuery has (`maximum bytes billed`) that we do not.

**EXPLAIN is an MCP tool, not a public `/sql` verb.** RFC-0016 shipped `explain`. The untrusted
SQL path does not. A dry-run quote is a host-side plan of a *named* query, not an `EXPLAIN` the
payer submits.

**The forum post the note cited (`1185c4d`) is stale.** `eth_call` (RFC-0023 tier 3) and IPFS
(RFC-0037) shipped in v2.6.0. That does not change the pricing argument. It means a nest can
have more than event tables, and the catalogue has to describe those too.

**RFC-0046 already refused the facilitator.** The note's x402 flow (Coinbase facilitator in the
verify/settle loop, `upto` on the wire) is the reference design 0046 §5.1 called wrong: it puts
a third party in the data path and leaks query timing. This RFC does not reopen that. Quote
shape and settlement path are different questions. The quote may look like an `upto` ceiling
(authorise at most X, settle for actual ≤ X). The nest still verifies locally and settles in the
back office. TAP-style receipt aggregation, if it happens, is an operator/gateway concern, not a
nest concern, and it does not route x402 revenue through Horizon (0046 §8, already answered:
two doors).

**RFC-0046 non-goals this RFC must not quietly eat:** not a nuthatch wallet; not subscriptions
or per-consumer accounts as a nest feature; one payment buys one query at the counter. A free
tier or a dashboard subscription, if anyone wants them, live in a gateway. The nest still sees
a signature or it does not.

## §2 - Comparable models, compressed

Everyone serving *homogeneous* requests (RPC, subgraph GraphQL) picked a flat or per-request
unit. Everyone serving *arbitrary SQL over large data* (BigQuery, Dune, Snowflake) picked a
usage unit correlated with scan or compute.

- **BigQuery on-demand:** $6.25/TiB scanned (US), 10 MB minimum, cached/errored free, dry-run
  upper bound from metadata, hard "maximum bytes billed" cap. Capacity/slot pricing exists
  because per-scan punishes bursty dashboards. The failure mode is a `SELECT *` on a petabyte
  table, or a misconfigured scheduled query.
- **Compute-time (Snowflake, Databricks, ClickHouse Cloud):** honest about actual use,
  unquotable in advance.
- **Alchemy CUs, Dune credits, SubQuery per-1,000-requests:** homogeneous-request units. The
  Alchemy failure mode (a `debug_traceTransaction` on a hot path, 20× CU) is the same class as
  a nuthatch named query that accidentally scans every segment.

Bytes-scanned is quotable and cap-able. It under-prices scan-light, compute-heavy SQL (joins,
windows, large sorts). Add a compute component only if telemetry shows those queries are
common. Do not add it on day one.

## §3 - The pricing function

### Phase 0 - Flat plus admission (ship first, if anything ships)

A single flat price per named query, in the asset RFC-0046 already chose, plus a
**deterministic byte-scan admission check** that rejects a query whose planned upper bound
exceeds a fixed threshold. The threshold is an operator config key, default conservative
(order of 1 GB is a starting number, not a measurement).

This is the Agora lesson made mechanical: do not build a DSL until usage proves you need one.
The admission check is what stops the pathological scan that flat pricing cannot survive. It
is also the missing row in the guard table in §1.

*Benchmark to leave Phase 0:* telemetry shows a meaningful fraction of legitimate named queries
hitting the ceiling, or a byte-scan distribution spanning more than two orders of magnitude
among queries that *pass* admission.

### Phase 0 - what the byte bound actually counts

The bound above is derived from the RFC-0047 catalogue, and the catalogue describes **sealed
Parquet only**. A named query runs over hot redb ∪ sealed segments, so a catalogue-derived bound
is not a whole-query bound and must not be called one. Taken literally it prices a query that
reads two million unsealed hot rows and touches no segment at **zero**, which is the worst
possible failure for an admission check: the pathological case scores best. The obvious inverse,
charging the whole hot store to every query, fails in the other direction and rejects the bounded
point reads that Phase 1's hot tier exists to serve cheaply.

So the bound is two terms, and the hot term is a **scan class**, not a catalogue lookup:

```
bound = cold_bytes(catalogue, predicate) + hot_bytes(scan_class, hot_rows)
```

The hot side has a property the cold side does not: it is **bounded by construction**. The hot
store holds only blocks in `(sealed_through, tip]`, so its size is capped by the finality window
rather than by history, and the node knows its own hot row count without consulting anything. That
is what makes a deterministic hot term possible at all.

| scan class | planner proves | hot term |
|---|---|---|
| keyed point read | the hot store's own key is fully bound by the predicate | `max_row_bytes` |
| bounded range | a contiguous range **of that key** is bound, so the access path visits only that range | `min(range_rows × max_row_bytes, hot_bytes)` |
| unbounded hot scan | nothing above | `hot_bytes`, the whole window |

**`max_row_bytes` must be an enforced ceiling, not an observed width, and this is the part that
decides whether the check is a guard or a decoration.** Hot rows are redb values and are
variable-width: a decoded event with a long `bytes` field is not the same size as a balance. An
average or a representative width **underestimates**, and it underestimates exactly on the rows an
adversarial caller would pick, so a keyed read of the largest value in the store could be admitted
under a threshold it actually exceeds. A pricing estimate may use an average. A resource cap may
not.

So the snapshot carries `hot_bytes`, the store's real size, and not merely a row count:

```
(catalogue_version, sealed_through, hot_rows, hot_bytes)
```

`hot_bytes` makes the unbounded class **exact** rather than estimated, which matters because that is
the class the guard exists for. The two bounded classes then need `max_row_bytes` to be a number
the node **enforces at write time**, so that no row can exceed it. If the hot store does not enforce
such a ceiling, the only sound bound for every class is `hot_bytes`, the tiers collapse into one,
and the RFC should say so plainly rather than quote a width it cannot defend. Establishing whether
that ceiling exists, or can be added without disturbing the ingestion path, is slice zero work and
a precondition for Phase 1, not a detail to settle during implementation.

The third row is the honest default. It is a large number, and it should be: an unbounded scan of
the mutable tip is exactly what a flat price cannot survive. It is still finite, and it is finite
for a structural reason rather than an empirical one.

**`LIMIT` is not a proof and must not be treated as one.** It bounds the rows a query *returns*,
not the rows it *reads*. `SELECT ... FROM hot WHERE value = 7 LIMIT 1` has a predicate on a
non-key column, so the access path is a full scan that stops early at best and reads the whole
hot store when no row matches - which is the case a hostile caller picks. `ORDER BY ... LIMIT 1`
is worse, since the sort must see every candidate before it can name the first. Both are
**unbounded hot scans** for admission purposes and are priced as such. The classifier keys on the
access path the planner actually chose, never on the shape of the SQL, because those two agree
right up until the moment it matters.

The bias is deliberate and one-directional: a query the classifier cannot prove bounded is priced
as unbounded. That over-prices some legitimate queries, which is a complaint. The opposite error
admits an unbounded scan under a bounded quote, which is an outage.

**The consistency rule, which is the part that bites.** Both terms are computed against a snapshot
captured **before planning**, and the `402` body quotes it alongside the coefficients. The snapshot
is a **four-field record**, and each element is load-bearing:

```
(catalogue_version, sealed_through, hot_rows, hot_bytes)
```

`sealed_through` and `hot_rows` alone are **not sufficient**, and assuming they were is the easiest
mistake to make here. They pin *where* the boundary sits, not *what the node knows about the cold
side of it*. A catalogue revision can restate segment statistics, or compact and replace the
segment set, with both of those numbers unchanged. A client recomputing from the quoted pair would
then derive a different `cold_bytes` than the node did, and execution could admit against a
different bound than the one quoted, while §4 goes on claiming the price is reproducible over
published metadata. Binding the quote to the catalogue version closes that, and it is the direct
consumer of RFC-0047's second commitment - version the catalogue that already exists rather than
adding a second source of truth. **Execution must use that exact catalogue.** If it has been
superseded, the node re-quotes rather than silently pricing against a newer one.

The boundary half is not ceremony either. It moves on its own: measured on the Lodestar box on
2026-09-02, `sealed_through` advanced from 501070866 to 501072741 inside a few minutes of ordinary
operation, and a parity script pinned to the earlier value refused to run against the later one. A
quote that does not name its snapshot is a quote against an unstated moving target.

Sealing between quote and execution moves rows from hot to cold, which can move bytes from a
scan-class term to a catalogue term and change the total in either direction.

**Two ceilings, and conflating them is the mistake to avoid.** They sit at different heights, they
are checked at different moments, and only one of them is negotiable.

| | the admission threshold | the quoted price ceiling |
|---|---|---|
| what it protects | the node's resources | the client's wallet |
| set by | operator config, order of 1 GB | the quote for this query |
| evaluated against | the **current** snapshot, at execution | the **quoted** snapshot |
| when the bound grows past it | **reject** | serve, and absorb the difference |

The admission threshold is **hard and re-evaluated at execution**. A query quoted at 900 MB that
sealing has since made a 1.2 GB scan is rejected, authorisation or no authorisation, because the
whole purpose of the check is that the node never runs the pathological scan. A guard that a
stale quote can walk past is not a guard, and "we already quoted it" is not a reason to run a
query the operator configured the node to refuse. Rejection at that point is a `4xx` with the
recomputed bound and both snapshots, and it charges nothing: RFC-0046 records the authorisation on
serving, so a refused query is simply never recorded.

The **price** is what the server eats. Where the bound grew but stayed under the admission
threshold, the client pays the quoted amount and the node absorbs the extra bytes, because the
client authorised a maximum in good faith against a snapshot the node itself published. What the
node must never do is re-quote upward mid-flight. The incentive that keeps this cheap is the same
one Phase 1 names: tight catalogue stats, and a quote whose snapshot is fresh.

The two behaviours differ because the failure modes differ. Absorbing a price gap costs the
operator a fraction of one query's margin. Absorbing a resource gap costs the operator the outage
that flat pricing plus an admission check was designed to prevent.

**Where the check is enforced, which is what makes the ceiling hard.** Re-estimating at execution is
still only an estimate if the query then runs for thirty seconds against a hot store that ingestion
keeps growing. A query admitted at 900 MB under a 1 GB threshold could scan 1.2 GB by the time it
finishes, and the guard would have guarded nothing.

The architecture already supplies the answer and it needs no lock on the ingestion path. `/sql` does
not evaluate against the live hot store: `analytics.rs` **scans hot rows into per-table temp tables**
and `UNION ALL`s them into each table's view, with hot and cold kept structurally disjoint by the
`sealed_through` watermark. That materialisation is a bounded, observable step that happens **before**
any user SQL is evaluated, and it is therefore the enforcement point:

- Materialise the hot side under **one redb read transaction**, so the set is fixed for the
  query's lifetime and blocks arriving mid-scan land in a later version it cannot see.
- **Count real bytes while materialising**, and abort the moment the running total crosses the
  admitted ceiling. Not an estimate compared against a threshold, but the actual copy refusing to
  grow past it.
- The cold side is already immutable by construction: sealed segments never change, and the
  catalogue version pins which of them exist.

That makes the ceiling hard **by construction** rather than by promise, and it costs no coordination
with the writer, which matters because the single-writer ingestion thread is not something a serving
path may block. It also relocates the guard from a prediction to a measurement, which is the general
shape this RFC should prefer wherever it can: §3's byte bound stays a *quote* input, and the thing
that actually stops a runaway scan is a counter on a copy that is already being made.

The cost is stated rather than hidden: aborting mid-materialisation means the node did real work for
a query it will not serve, and it must not bill for it. That is the same rule as a rejected
admission, and RFC-0046 already gives it - the authorisation is recorded on serving, so a query that
never serves is never recorded.

This is also the sharpest freeze-legality point in the RFC. The scan classes above are a claim
about what the planner can prove, and nothing in the tree proves them today. Whoever builds this
owes a slice zero that demonstrates the classifier on the real named-query corpus **before** any
price is attached to its output, because an admission check that silently classifies an unbounded
scan as a point read is worse than no admission check at all.

### Phase 1 - Two-tier quote-then-pay

Only after that benchmark.

- **Hot / tip tier:** flat. Entry is the **keyed point read** and **bounded range** scan classes
  from Phase 0, and nothing else. The proof is a key-bound access path, never a `LIMIT`: this tier
  is the one place where getting that wrong is profitable to exploit, since a caller who can dress
  an unbounded scan as a tip lookup buys it at RPC-call prices. `SELECT * FROM hot WHERE value = 7
  LIMIT 1` is an unbounded hot scan here exactly as it is in Phase 0, and a query the planner
  cannot prove key-bound falls to the cold tier or is refused. Most consumer named queries should
  still live here, because most of them really are point reads.
- **Cold analytical tier:** `price = base + α·bytes_scanned_upper_bound` (+ optional
  `β·rows_returned` for egress). `base` covers fixed overhead; `α` covers I/O per byte of
  sealed Parquet. **Cap on the quoted bound, settle on actuals** if actuals are measured. The
  quoted bound is a true upper bound, so the client is never surprised above the authorised
  max. The server eats the gap between bound and actual, which is the incentive to keep
  catalogue stats tight (RFC-0047).

The `402` body carries the ceiling and the breakdown (segments touched, byte bound, coefficients)
so a client that has the catalogue can recompute the bound. That is the verifiability property
BigQuery's dry-run does not have: our estimate is over published, content-addressed metadata,
not an opaque server number.

Settlement remains RFC-0046: local verify of an authorisation whose amount is at least the
quoted ceiling (or, if `upto` is used as a wire shape, at most that ceiling and settle for
actual ≤ max, still without a facilitator in-process).

### Phase 2 - A pricing manifest, not a DSL

A static document co-located with the nest, in the NID if the price is part of the product
(and therefore out of the data identity, same reason RFC-0034's ceiling is). Declares: tier
boundaries, `base` / `α` / `β`, the byte-cap, the wall-clock timeout, free-tier limits if a
gateway applies them. Data, not a Turing-complete model. Agora's lesson, again.

Receipt aggregation for high-frequency clients is TAP-shaped and lives next to the settler,
not in the nest.

### Phase 3 - Multi-host selection

When more than one host serves the same NID, a gateway may select on published manifest
(price ceiling per tier), liveness, and freshness. That is SubQuery / ISA-shaped, with SQL
cost tiers instead of a query-shape model. It is a gateway RFC. It is not a nest RFC. REO-style
eligibility (HTTP 200, latency bound, freshness bound) as a serve-quality gate belongs there
too.

**Explicitly punt, same as the note and same as GIP-0066/0081's unbundling:** curation, bonding
curves, slashing, indexing rewards, on-chain staking. Payments (0046) and verifiability
(published catalogues, optional re-execution) first. Economic security later, optional, not a
prerequisite.

## §4 - Verifiability, without building a fisherman

Sealed, content-addressed segments plus a published catalogue (RFC-0047) mean a client can
recompute the same bytes-scanned upper bound the server quoted. That is optimistic
verification of the *price*, by re-execution over immutable inputs, which is the shape of The
Graph's fisherman game without the bonds. Do not build slashing in order to publish a
catalogue.

Result verification is the same primitive the project already uses: two operators, same nest,
compare. GRC-009 said that out loud. Pricing does not need a new one.

## §5 - Build sequence, if unfrozen

RFC-0046's slices 0-3 remain first (boundary test, local verify, the counter, the back office).
This RFC adds, after those:

1. Host-side byte-scan estimator from the catalogue (and footer reads the query path cannot
   make). Named queries only.
2. Hard byte-cap + the existing wall-clock timeout, as admission, even on a flat price.
3. Quote in the `402` body (ceiling + breakdown).
4. Two-tier function + published pricing manifest, only if Phase 0's benchmark fires.
5. Deferred/batch settlement in the *settler*, not the nest.
6. Multi-host selection: a gateway, a different document.

## §6 - Drawbacks and caveats

- Bytes-scanned is not compute. Joins and windows can be expensive on little I/O. Measure
  before adding a second term.
- A metadata-derived bound is an upper bound. DuckDB's pushdown means actuals can be far
  below. Over-quoting prices legitimate queries out. Invest in tight catalogue stats and
  block-range partitioning (which sealing already is) rather than padding the bound.
- x402 `upto` and deferred settlement were new in 2025-2026. Confirm the operator's settler
  supports the wire shape before depending on it. The nest does not speak to a facilitator
  regardless.
- Phase 1+ is new capability. Phase 0's admission check is a guard, like the 2M hot-row cap,
  and is the only slice that could be argued freeze-legal as hardening. Even that wants a
  board nod, because a byte-scan planner is not a one-line `const`.

## §7 - Alternatives

- **Launch with a cost-model DSL.** Rejected. Agora is the existence proof.
- **Charge compute-time.** Honest, unquotable, and it makes the `402` a guess. Rejected for
  the counter; a gateway may still offer a subscription for bursty dashboards.
- **Meter open `/sql`.** Rejected. RFC-0034 exists because arbitrary SQL is unquotable. A
  priced surface that accepts a string is how you recreate BigQuery's $140,000 scheduled
  query as a nest outage.
- **Put TAP receipts in the nest.** Rejected by RFC-0046 §8. Two doors.

## §8 - Unresolved

- The default admission threshold in bytes. Needs a measured distribution of named queries
  against real catalogues (Lodestar allocations nest is the obvious first), not a round
  number from this document.
- Whether `β·rows_returned` is worth a second coefficient, or whether the existing 50k row
  cap is the egress bound and the price stays bytes-only.
- Whether a named query's tier is declared in the pricing manifest or inferred by the
  planner. Declaration is simpler and Agora-proof; inference is how a "hot" query silently
  becomes a cold scan after a view change.
- How the quote binds to a catalogue hash, so a client that recomputes against a different
  `manifest_version` cannot be told they mis-read.
