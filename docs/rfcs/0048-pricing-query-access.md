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
exceeds a threshold.

**That threshold is derived, not chosen, and an earlier draft of this RFC got it wrong.** It said
"an operator config key, order of 1 GB". Put that next to the two facts either side of it and it
does not survive: `SQL_MAX_CONCURRENCY` admits **two** concurrent queries, and the enforcement
below copies hot rows into per-query temp tables. Two admissions at 1 GB is **2 GB of hot copies
alone**, before DuckDB's 512 MB working memory, before the redb store, before ingestion state,
before serving overhead. `CLAUDE.md`'s **≤2 GB per active-chain cursor** is not a target to be
approached from below by one subsystem; it is the whole cursor, shared across every nest on it. A
freely-configurable byte threshold is a way to violate a non-negotiable by editing a config file.

So the ceiling is a **cursor-wide reservation, divided**:

```
threshold ≤ (cursor_budget − ingestion − store − serving − duckdb_working_set) / SQL_MAX_CONCURRENCY
```

Three consequences follow, and none of them is optional:

- **Admission is accounted cursor-wide, not per query.** A second query admitted while the first is
  still materialising must be checked against what is left, not against the same ceiling again.
  Per-query admission with a shared budget is how two individually-legal queries become one
  violation.
- **The starting number is a few hundred megabytes, not 1 GB**, and this RFC declines to write a
  figure at all until the subtractions above are measured. RFC-0047's fourth commitment is exactly
  that work - expose the existing 512 MB and two-thread DuckDB caps as config and name the ingestion
  reservation - so this is a dependency, not a coincidence.
- **`SQL_MAX_CONCURRENCY` is a divisor here**, which changes its character. Raising it does not only
  add throughput, it shrinks every query's ceiling. That is worth stating because revisiting that
  constant is already an open item, and this RFC gives it a second constraint it did not have.

The CI per-cursor RAM budget is the check that would actually catch a mistake here, and a Phase 0
that ships without a scenario exercising `SQL_MAX_CONCURRENCY` admissions at the threshold has not
been tested against the thing most likely to go wrong.

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

**`cold_bytes` is per physical scan operator, not per distinct segment.** Written as a function of a
predicate it reads like "the set of segments this query touches", and that is not an upper bound. A
named query that self-joins a sealed table, reads it through two CTEs, or unions two branches over
it visits those segments **more than once**, and a set has no way to say so. Deduplicating segments
across operators under-prices exactly the queries that do the most I/O.

The rule, and it is deliberately crude in the safe direction:

1. Take the **physical plan**, not the SQL text. Two CTEs that the optimiser materialises once are
   one scan; two that it inlines are two. Only the plan knows which.
2. For **every scan operator** over a sealed table, add the bytes of every segment that operator can
   still touch after predicate and partition pushdown.
3. **Sum over operators with no deduplication.** A segment read by three operators is charged three
   times, because it is read three times.
3b. **Refuse any plan in which a sealed scan can be re-executed.** Counting an operator once is only
   an upper bound if it runs once, and it does not: a scan on the inner side of a nested-loop join,
   inside a correlated subquery, or under a lateral is **rescanned per outer row**. One operator,
   many passes over the same segments, and the per-operator sum charges it a single time while
   execution touches it thousands. With no cold bytes-read counter there is nothing downstream to
   catch it either, so this is the hole through which an unbounded cold scan would walk.

   Bounding it properly needs a proven multiplicity, which means trusting a cardinality estimate,
   and a resource cap resting on an estimate is not a cap. So the rule refuses instead: a named
   query whose plan contains a rescan-capable sealed scan is **not publishable**. That is a real
   restriction and it is meant to be - it is also usually a rewrite away, since a hash join over the
   same two relations reads each side once and is what the planner should have picked.
4. Any operator the rule does not recognise, or any plan that cannot be obtained, **refuses to
   quote**. Fail closed. A bound that silently skips a node it has never seen is not a bound.
5. **Recursive CTEs are refused outright**, for the same reason as 3b and more starkly: their scan
   count is not statically bounded at all, so no finite upper bound exists, and no named query the
   counter serves needs one.

**The reason this is tractable at all is that named queries are a fixed, reviewed corpus.** This RFC
does not price open `/sql`, and that is not only a safety decision - it is what makes the cold bound
reviewable. A query whose bound cannot be computed does not get published, which is a far better
failure than one that gets published and then cannot be priced.

**But the publish-time plan is not the execution plan, and treating it as one would sink the whole
guard.** DuckDB re-plans: join order, CTE materialisation, predicate pushdown and the number of scan
operators all depend on the current catalogue, the contents and statistics of the hot temp tables,
and the session's settings. A query quoted from a plan with one sealed-table scan can execute with
two. Rules 1 to 5 are only sound against the plan that actually runs.

So the two bounds have different jobs and must not be confused:

| | publish-time bound | execution-time bound |
|---|---|---|
| computed from | the plan at review, per (query, catalogue version) | the plan the node is about to run |
| what it is for | the **quote**, and the human review that decides publishability | the **guard** |
| when it is wrong | the query is re-reviewed | the query is refused |

The execution-time bound is obtained on the same connection, with the same catalogue and the same
settings, and it is the number admission is checked against. Any setting that can change a plan is
**fixed for named queries** rather than left to the session, because a bound that moves with a
session variable is not reproducible and §4's verifiability claim depends on it being so.

**The ordering that makes this work**, and it falls out of the hot rule rather than needing anything
new. Hot materialisation happens first and is already bounded by the residual budget. Only once the
temp tables exist does the planner see the statistics it will actually plan against, so the
execution plan is taken **after materialisation and before the user SQL is evaluated**, and the cold
bound is checked there. That is a safe place to abort: materialisation reads the hot store, not the
segments, so **no cold I/O has happened yet** at the moment the cold bound is tested.

A large divergence between the two bounds is not an error in itself, since data grows, but it is the
signal that the published quote needs re-reviewing, and it should be reported rather than absorbed
silently.

**What is enforced at runtime, stated honestly.** The hot side has a real counter, described below:
the materialisation refuses to grow past the ceiling. The cold side does **not** have an equivalent
byte counter today, so its guarantee rests on the plan bound plus the existing wall-clock timeout and
512 MB memory cap. That is weaker, and it is weaker in a specific way worth writing down rather than
glossing: the plan bound is sound only if rules 1 to 5 hold, and nothing at runtime would catch it if
they did not. Adding a cold bytes-read counter to the scan path, and aborting on it the way the hot
materialisation does, is the item that would make both halves symmetrical. It is not in Phase 0 and
Phase 0 should not claim it is.

The hot side has a property the cold side does not: it is **bounded by construction**. The hot
store holds only blocks in `(sealed_through, tip]`, so its size is capped by the finality window
rather than by history, and the node knows its own hot row count without consulting anything. That
is what makes a deterministic hot term possible at all.

| scan class | planner proves | hot term |
|---|---|---|
| keyed point read | the hot store's own key is fully bound by the predicate | `max_row_bytes` |
| bounded range | a contiguous range **of that key** is bound, so the access path visits only that range | `min(range_rows × max_row_bytes, hot_bytes)` |
| unbounded hot scan | nothing above | `hot_bytes`, the whole window |

**On today's `/sql` path there is only one class, and it is the third one.** This has to be said
before the table is read as a description of the node. `analytics.rs` gathers hot rows via
`Store::hot_rows_by_table` and `load_hot_temp` copies them into a per-table DuckDB temp table, and
only then is the user's SQL evaluated. The key predicate applies **after** the copy. So
`SELECT * FROM hot WHERE id = ?` materialises the whole hot table exactly as `SELECT * FROM hot`
does, and pricing it at `max_row_bytes` would admit an unbounded operation under a point-read bound.
The one path that genuinely avoids this is the trusted point-read endpoint, which passes no hot rows
at all - a different route through the node, not a cheaper query on this one.

**So the scan classes are conditional, and the condition is a real piece of engineering:** a redb
key and range pushdown that runs *before* materialisation, so a bounded query copies a bounded set.
Until that exists, every named query is an **unbounded hot scan** for admission purposes and the hot
term is `hot_bytes`, full stop. The table above describes what the classifier could distinguish once
the pushdown lands, and nothing before.

That is not as bad as it sounds, because `hot_bytes` is bounded by the finality window rather than by
history, so a threshold set above it is workable. It is simply not yet *discriminating*: the check
stops a runaway cold scan and treats every hot query alike. It also settles Phase 1's sequencing
rather than leaving it to taste - **the hot/tip tier cannot ship before the pushdown**, because its
whole premise is a distinction the serving path cannot currently make.

**`max_row_bytes` must be an enforced ceiling, not an observed width, and this is the part that
decides whether the check is a guard or a decoration.** Hot rows are redb values and are
variable-width: a decoded event with a long `bytes` field is not the same size as a balance. An
average or a representative width **underestimates**, and it underestimates exactly on the rows an
adversarial caller would pick, so a keyed read of the largest value in the store could be admitted
under a threshold it actually exceeds. A pricing estimate may use an average. A resource cap may
not.

So the snapshot carries `hot_bytes`, the store's real size, and not merely a row count:

```
(catalogue_hash, sealed_through, hot_rows, hot_bytes)
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
(catalogue_hash, sealed_through, hot_rows, hot_bytes)
```

`sealed_through` and `hot_rows` alone are **not sufficient**, and assuming they were is the easiest
mistake to make here. They pin *where* the boundary sits, not *what the node knows about the cold
side of it*. A catalogue revision can restate segment statistics, or compact and replace the
segment set, with both of those numbers unchanged. A client recomputing from the quoted pair would
then derive a different `cold_bytes` than the node did, and execution could admit against a
different bound than the one quoted, while §4 goes on claiming the price is reproducible over
published metadata.

**And it must be the catalogue's content hash, not a version label.** A version is a name, and a
name can be re-pointed: the manifest for `v` replaced in place, or two hosts resolving `v` to
different bytes. Either breaks the recomputation §4 promises while every field in the quote still
matches. This is not a new discipline to invent - the architecture is content-addressed throughout,
segments included, and RFC-0047's second commitment is to version the manifest that already exists
with an atomic rename rather than add a second source of truth. The quote carries the hash of that
manifest, and **execution must use that exact catalogue**. If it has been superseded, the node
re-quotes rather than silently pricing against a newer one.

Which imposes one obligation worth stating rather than discovering later: **a quoted catalogue hash
must stay resolvable for as long as the quote is valid.** A quote whose catalogue has been compacted
away cannot be honoured or verified, so quote validity and catalogue retention are the same window,
and the shorter of the two is the real one. That is the whole contract: hash in the quote, retention
at least as long as the quote, and a re-quote rather than a substitution when either lapses.

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

- Refuse at admission, before materialising anything, when `cold_bound >= threshold`. There is no
  budget left to spend and no reason to copy a single hot row first.
- Materialise the hot side under **one redb read transaction**, so the set is fixed for the
  query's lifetime and blocks arriving mid-scan land in a later version it cannot see. Today that
  set is the whole hot side for every query; with the pushdown it is whatever the bound admitted.
- **Count real bytes while materialising, against the residual budget** - `threshold - cold_bound`,
  not the threshold. Charging the hot copy against the whole ceiling is the obvious mistake and it
  is worth naming: a 1 GB threshold with a 900 MB cold bound and 200 MB of hot rows passes a naive
  hot counter while the query reads 1.1 GB. The hot side may only spend what cold has not already
  claimed.
- The cold side is already immutable by construction: sealed segments never change, and the
  catalogue version pins which of them exist.

**What that adds up to, without overclaiming.** The hot half is hard by construction: a measured
counter on a copy that is already being made, needing no coordination with the single-writer
ingestion thread, which a serving path may not block. The cold half is **plan-bounded, not
counted** - its guarantee is rules 1 to 5 above plus the existing wall-clock timeout and 512 MB
memory cap.

So the honest description of Phase 0 is **not** "a hard total byte ceiling". It is a hard ceiling on
hot bytes and a reviewed static bound on cold bytes, and the two are not the same kind of promise.
Writing it the other way round would be the exact failure this RFC keeps warning about elsewhere:
a check that reads as a guarantee because nobody said which half it covers. A cold bytes-read
counter on the scan path, aborting the way the hot materialisation does, is what would make the
total ceiling hard, and until it exists the RFC should say total *bound* and reserve *cap* for the
hot side.

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

- **Hot / tip tier:** flat, and **blocked on the redb key/range pushdown named in Phase 0** - until
  a bounded query materialises a bounded set, this tier is a price for a distinction the node cannot
  make. Entry is then the **keyed point read** and **bounded range** scan classes, and nothing else. The proof is a key-bound access path, never a `LIMIT`: this tier
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
- Catalogue **retention**, now that §3 has settled the binding itself: the quote carries the
  manifest's content hash and execution must use that exact catalogue, so the open question is only
  how long a quoted catalogue is kept resolvable, and therefore how long a quote may live. The
  shorter of the two is the real quote lifetime; neither number is chosen here.
