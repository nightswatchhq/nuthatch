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

### Phase 1 - Two-tier quote-then-pay

Only after that benchmark.

- **Hot / tip tier:** flat. The planner proves the query is bounded (point read, bounded-range
  hot lookup, `LIMIT`-ed tip). Charge like an RPC call. Most consumer named queries should live
  here.
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
