# Pricing the Lodestar nests: RFC-0048 applied to a real corpus

**What this is:** one page taking [RFC-0048](rfcs/0048-pricing-query-access.md)'s pricing design and
running it against the nests actually in production, with the numbers measured on the box on
**2026-09-06** against **nuthatch 3.5.1**. RFC-0048 §8 says its default admission threshold "needs a measured distribution of
named queries against real catalogues (Lodestar allocations nest is the obvious first), not a round
number from this document". This is the first instalment of that measurement.

**What this is not.** RFC-0048 is a draft, design only, and **not a carve-out**; RFC-0046 is accepted
in principle, design only. Under the 2026 feature freeze neither is work to start. There is **no x402
in the nuthatch tree** - the working buyer and seller live in Lodestar's TypeScript. Nothing below
is a plan to build. It is the arithmetic an operator would need before anyone did.

The companion page, [the Lodestar lifecycle](lodestar-lifecycle.md), carries the cost and
performance figures this one prices against.

---

## 1. The corpus is small, fixed and reviewed, which is the whole reason this is tractable

RFC-0048 does not price open `/sql`; it prices **named queries on a bounded surface** (RFC-0034).
What that surface actually contains here:

| | count |
|---|---:|
| SQL builders exported by Lodestar's `nest-queries.ts` | **42** |
| distinct views they read | **13** |
| authored view files on `graph-allocations-nest` | 12 |
| decoded tables the nest exposes | 81 |
| nests behind one host | 5 |

Forty-two statements is a corpus a human can review one at a time, which is exactly the property
that makes a per-operator cold bound reviewable rather than aspirational. It is also small enough
that a pricing manifest (Phase 2) is a short data file, not a DSL - the Agora lesson made cheap.

---

## 2. The two terms, with the real numbers

RFC-0048 Phase 0: `bound = cold_scan_bytes + hot_scan_bytes`, one unit throughout, source bytes read.

**Cold.** `graph-allocations-nest` holds **1,924 Parquet segments totalling 643 MB**. A worst-case
unpartitioned scan of the entire nest is therefore 643 MB, and every per-table bound is a fraction of
it. That is a small number by the standards RFC-0048 worries about - BigQuery's failure mode is a
petabyte, ours is two thirds of a gigabyte - and it means a Phase 0 threshold sized to refuse a
runaway scan on this corpus has little work to do.

**Hot.** The redb hot store is **16 MB on disk**, holding roughly 353,000 unsealed blocks (about a
day of Arbitrum). RFC-0048 concedes that on today's serving path there is only one scan class, the
unbounded hot scan, because `analytics.rs` copies every hot row into a temp table before any key
predicate applies. **That concession costs almost nothing here.** The pessimistic default term is
16 MB. The redb key/range pushdown that Phase 1's hot tier is blocked on would be a correctness and
latency improvement on these nests, not a pricing one.

**So the total Phase 0 bound for the worst named query on this nest is order 660 MB**, and for a
typical one, far less. An operator sizing a threshold on this corpus is choosing between "refuse
almost nothing" and "refuse the whole-nest scan".

---

## 3. The finding that matters most, and it is a caution

The first thing I ran against the real corpus was `SELECT count(*) FROM lodestar_delegator_stakes`.
It **exhausted DuckDB's per-connection memory limit** and returned an out-of-memory error rather
than rows. On 3.5.0 the limit was `488.2 MiB` and a later attempt killed the process; on 3.5.1 it is
`244.1 MiB` and the refusal is a clean `400` in 1.5 seconds.

A Phase 0 byte-scan admission check would have **admitted that query.** Its scan bound is a fraction
of 660 MB; the thing it exhausted was resident memory, on a `count(*)` returning one row.

This is RFC-0048 §3's central caution demonstrated on live data rather than argued: *"a scan-byte
bound is not the RAM guard, and Phase 0 does not add one."* Two earlier drafts of that RFC mixed
scan bytes with resident bytes and the RFC keeps both on the record for this reason. The measurement
above is the confirming case. **The bytes-to-resident expansion factor on this workload is not small
and is not measured**, which is one of the three figures RFC-0047 owes and which nothing here
supplies.

**And the guard moves with a knob that looks like a throughput setting.** Per-connection `max_memory`
is `min(512 MB, 1024 MB / permits)`. This deployment sets `NUTHATCH_SQL_MAX_CONCURRENCY=4`, so every
query gets 256 MB rather than 512. Raising concurrency to sell more queries therefore shrinks the
budget of each one, and the set of named queries a nest can answer at all changes underneath the
price list. RFC-0048 §3 already notes that raising that constant has a second cost; this is a third,
and it argues for pinning permits in the pricing manifest alongside the coefficients.

The practical consequence for an operator: **the guard that will actually fire on these nests is the
per-connection memory limit and the 30-second wall clock, not a byte threshold.** Pricing by bytes
scanned would be pricing the dimension that is not binding.

## 4. The rescan rule, and how many queries it refuses

RFC-0048 rule 3b refuses to publish any named query whose plan contains a sealed scan that can be
re-executed - nested-loop inner sides, correlated subqueries, laterals, recursive CTEs - because one
operator can mean thousands of passes and a per-operator sum charges it once.

Against the 42 builders: **one** carries a correlated scalar subquery in its SQL text,
`indexerActiveAllocationsSql`, which sums allocations per deployment against the outer row
(`nest-queries.ts:347`). One is a CTE with a `LEFT JOIN`, `poiAllocationsSql`. The rest are flat
selects, aggregates and joins.

**That is not the same as one refusal, and the difference is the rule.** Rule 1 says take the
physical plan, not the SQL text, and DuckDB decorrelates scalar subqueries into hash joins as a
matter of course. Whether `indexerActiveAllocationsSql` is publishable is a question for `EXPLAIN`,
not for a grep - and the grep is the answer to a different question, which is how much rewriting a
Phase 0 would cost. **At most one query out of 42.** The rule is affordable on this corpus.

---

## 5. What the surface can actually earn

The ceiling is not the price. It is throughput.

`/sql` admits `NUTHATCH_SQL_MAX_CONCURRENCY` queries and refuses the rest in under 3 ms. The default
is 2; this deployment sets **4**, measured by firing ten at once over three rounds and being admitted
nine times. Latencies on 3.5.1 run **0.08 s to 6.1 s**, mean about 2 seconds across the sampled
views. Four permits at two seconds is roughly **2 queries per second, or 5 M a month**, and every
permit added takes memory from each query, so the ceiling and the answerable-query set move together.

**Under ordinary dashboard load there are no refusals at all**: 380 queries admitted in 70 minutes,
zero rejected. Refusals appear the moment a second independent caller arrives, which is exactly what
a paid surface would be. So the refusal rate is not a defect to fix before pricing; it is the shape
of what selling access does to a node that protects itself.

Actual demand, for a public dashboard: about **326 queries an hour**, ~238,000 a month, which is
under 5% of the throughput ceiling. That was measured on 2026-09-06 against the client that preceded
Lodestar's Rust backend, which did not coalesce identical in-flight reads; the one that replaced it
on 2026-09-07 does, so demand per nest can only fall from that figure.

Against that, the measured cost of running all five nests: **RPC of roughly $2 a month per
tip-following cursor** at a 5-minute poll, plus one 8 GB VPS carrying all five at load 0.63. Call the
whole thing a low-tens-of-dollars figure a month, and the marginal cost of a query effectively zero.
The bill is the cursor, not the reads.

So the operator's arithmetic is:

```
revenue ceiling  = flat_price x min(demand, throughput_ceiling)
                 = flat_price x 238,000 per month at current demand
cost             = one VPS + ~$2 per cursor per month
```

At any plausible per-query price this clears its costs easily and never approaches a number worth
building a payment path for on its own. **The reason to do this is not the revenue from one box.**
That is worth writing down before anyone builds Phase 0, because the freeze is real and a slice
justified by revenue arithmetic that does not survive contact with actual demand is a slice that
should not start.

## 6. What this corpus says about RFC-0048's phases

RFC-0048 §5 sets out an order. Measured against this corpus, several of its assumptions read
differently. This section records what the measurements say about that design and is **not a
recommendation to build any of it**: billing, metering and paid access are out of scope in
`CLAUDE.md`, the RFC is a draft, and nothing here is a carve-out.

1. **The boundary test (RFC-0046 slice 0) is undisturbed by this corpus.** Delete every payment
   feature and a self-hoster of these nests loses nothing. `CLAUDE.md` §3 binds the artefact, not the
   operator.
2. **The byte-scan admission check is nearly inert here.** It would bear on at most one query in 42,
   and §3 above is why. Whatever value it has on this corpus is for the pathological case rather than
   for these statements.
3. **A byte threshold cannot be sized until the expansion factor is measured.** The binding guard
   here is memory, and a byte threshold that admits the query which OOMs the node reads as a
   guarantee without being one.
4. **Phase 1's hot tier prices a distinction this corpus does not have.** With a 16 MB hot store
   there is no meaningful difference between a point read and a full hot scan.
5. **Phase 2's manifest would be cheap here** - 42 statements, 13 views, one file.
6. **Phase 3 is a gateway question rather than a nest one**, and multi-host selection would need a
   second host, which this deployment does not have.

**One prerequisite, and it has just been met.** The serving path took 31 segfaults on 2026-09-06 on
3.5.0 ([#1165](https://github.com/nightswatchhq/nuthatch/issues/1165)); 3.5.1 gives each DuckDB
instance a private spill directory ([#1182](https://github.com/nightswatchhq/nuthatch/pull/1182)) and
the box has recorded none since 12:50 UTC. Nobody should attach a price to a surface that falls over
thirty-one times a day, and as of 15:06 UTC that is no longer the surface. One day of quiet is not a
proof, and the pricing question does not become live until it is.

---

## 7. What remains unmeasured

- **The bytes-to-resident expansion factor** for a hot temp table on this workload. §3's finding
  says it is the number that matters and nobody has it.
- **The per-query DuckDB working set** across all 42 statements, not the six sampled here.
- **A byte-scan distribution** across the corpus. This page establishes the ceiling (660 MB) and one
  outlier; it does not establish the spread, which is what §3's "leave Phase 0" benchmark keys on.
- **Whether `max_row_bytes` is enforced at write time** in the hot store. RFC-0048 calls this slice
  zero work and a precondition for Phase 1; nothing here checked it.
