# RFC-0062: Maintained views - a join-heavy view answered from a stored copy of its own evaluation

- Status: **Accepted by Chief 2026-10-07 (#1975). S1 (lazy maintained views) built; S2 to S4 not
  started.** Tracking #1973. The S0 spike lives on the throwaway branch `pete/rfc-0062-s0-spike` and is
  never merged; S1's measured results are in §8a.
- Author: Pete (cargopete)
- Date: 2026-10-07
- Depends on: RFC-0018 §1 (authored views, evaluated per request over hot ∪ sealed), #1186 and #1955
  (the answer memo and the identity it keys on), RFC-0047 C4 (the per-cursor analytics budget).
- Compared against: RFC-0041 (authored incremental entities), RFC-0059 (checkpointed folds, parked
  2026-09-26), RFC-0033 (grafting, deferred).
- Nature: new binary capability. Per RFC-0044 §8 and CLAUDE.md it is decided by Chief and recorded,
  or it does not happen.

> **S0, 2026-10-07: continue.** The spike answers a declared view from a Parquet copy of its own
> request-time evaluation, keyed on every input that evaluation reads. On BetSwirl's full history
> (dataset `f00657c7`, seeded from the public mirror, 160,802 bets) on the ThinkPad at the default
> 512 MB:
>
> - **All eight SDK shapes are under 1 s cold**, on the first request of a fresh process as well as at
>   the 20-run p50. The first `bets` page goes from 2.02 s to 0.19 s; the worst shape, skip 150,000,
>   from 3.07 s to 0.76 s.
> - **Answers are byte-identical**: the eight shapes' JSON, and every row of all seven entity views
>   (167,540 rows) dumped through `/sql`.
> - **The full history materialises in about 3.6 s** (the `bet` copy alone 2.4 to 2.6 s), into 43.4 MB
>   of Parquet. Serving from the copies lowers process memory: 151 to 426 MB high-water against 405 to
>   855 MB today.
> - **A block with no rows for a maintained view costs nothing.** On a live BSC-following copy over
>   1,875 blocks, the only rebuild was the one the first start's rewrite of the seeded segments caused.
>   A block that does carry rows costs one rebuild of each affected copy (up to 3.6 s), and until it
>   lands the request answers as today. S0 saw no such block: BetSwirl has had no bet since March.
>
> Detail and the reasons the verdict is not stronger are in §8.

## Abstract

BetSwirl's `bet` view derives 160,802 rows through about twenty left joins, five windows, six as-of
joins and a BigDecimal division written in string functions, on every request that reaches it. Three
rounds of engine work took a cold page from 10.4 s to about 2 s (#1951). What remains is planning a
650-line view tree and computing shared derivations in full, and neither is a defect.

This RFC lets an author declare such a view **maintained**. The runtime evaluates the view exactly as a
request would, by the same code path and engine, and writes the rows to an immutable Parquet copy beside
the sealed directory. The copy is named by a hash of every input the evaluation read: the authored files,
the binary and engine, the sealed segments of the tables the view reaches, and the hot rows of those
tables. A request whose inputs hash to a present copy reads the copy. Any other request answers from the
definition, as today, and the copy for its inputs is built behind it.

A copy is never served at inputs it was not computed from, so it cannot be stale and cannot differ from
the request-time view. That is the whole determinism argument, and it is the same argument the answer
memo (#1186) already rests on, one level up: from one statement to one view.

## §0 - The non-negotiables this touches, and why they hold

**4. Determinism in the core.** Maintained rows feed no stored state. They are a cache of a
deterministic function of stored facts, so the request-time view is the oracle and the copy must equal
it byte for byte at every block.

- A copy is written by `analytics::run` with the same session setup, view definitions and engine that a
  request uses. There is no second implementation of the view to drift from the first. Option (a) in §2
  would create one; this design avoids it.
- A copy is served only when its identity (§3.2) equals the identity of the request's own inputs. A
  request at any other block, after any commit, seal, reorg, view edit or binary upgrade, has a different
  identity and finds no copy.
- A view whose answer is not a function of its inputs cannot be maintained. A volatile function anywhere
  in its closure is refused at load, by the memo's `is_deterministic` list. A view that breaks ties
  arbitrarily (`row_number` over a non-unique order) is not deterministic at request time either; §3.6
  makes `check --maintained` evaluate twice to catch it.
- S0 measured it: every row of seven views and every SDK answer byte-identical (§8).

**Reorgs touch only the hot store.** A reorg rolls back hot rows. That changes the hot-row part of the
identity, so the next request either finds the copy for the restored inputs, which retention may still
hold, or answers from the definition. Nothing is retracted from a copy, because a copy is never edited:
it is either current for a request's inputs or not used.

**Sealed segments are never mutated.** Copies live in `maintained/<view>/<identity>.parquet`, beside
`segments/` and never in it. They are not in the catalogue, not in the dataset identity and not
published (§3.7). Losing every copy loses nothing but time.

**2 GB per cursor.** The state is measured, not assumed (§8). A build is one evaluation of the view,
the same work as one cold request reaching it, run one at a time per cursor and admitted against the
cursor's analytics pool (RFC-0047 C4), as a query is. On BetSwirl: a 357 MB pool peak, and 1.34 GB
process high-water with a request-time read running beside the build. Serving from copies lowers
memory, from 405 to 855 MB high-water today to 151 to 426 MB.

**Single binary, no phone-home, licence.** No new service and no new dependency: Parquet is written by
the engine's existing writer (`Session::write_parquet`, the one RFC-0059 S2 uses) and read by its
existing reader (`bind_snapshots`).

**What a nest that declares nothing sees.** Nothing changes. With no `maintained` declaration the
request path is today's, line for line; S1's acceptance includes the Lodestar release gate to prove it.

## §1 - The measurement that asks for it

From #1951, ThinkPad, `serve` cold with the memo off, default 512 MB engine limit, 4.13.0:

- planning about 0.45 s on an M-series Mac (about 1.9x the ThinkPad's speed), because analysis walks
  every inlined copy of a view (burrmill#92);
- execution about 0.55 s on the Mac, most of it shared derivations computed in full. `bs_applied` alone
  is about 0.25 s, and it is needed whole, because `token`'s counters aggregate every applied resolution;
- skip 150,000 cannot be narrowed: it keeps 150,020 rows, and pushing an offset below a left join needs
  every joined side unique on its key, which `bs_house_edge` and `bs_applied` are not.

The stopgap is a 30-minute timer warming the SDK's common shapes, which answer from the memo in 3 to
9 ms. It helps no filter or offset nobody warmed, and no other nest.

## §2 - Designs compared

| | (a) extend RFC-0041 | (b) fold-on-seal ∪ hot tail | (c) recompute at commit, write-once | **(d) input-keyed copies** | (e) more engine work |
|---|---|---|---|---|---|
| Expresses `bet` as written | no: needs outer, as-of and window lowering, CTEs, UNION, 19 scalar functions | no: wrong without a carry rewrite | yes | yes | yes |
| Second implementation of the view | yes, in `entity_expr` | yes, the author's carry form | no | no | no |
| Read cost | point read | one hot-tail step | read a copy | read a copy | about 2 s, 3 s at skip 150,000 |
| Cost per data-bearing block | O(delta) | O(window) | O(history), on the commit path | O(history), behind the reads | none |
| State | DBSP traces of every join input, in RAM | carries plus checkpoints | one copy | one copy per retained identity | none |
| Restart | reseed from history (RFC-0041 §5.3) | load checkpoint | rebuild | copy survives | none |
| Status | v1 shipped, scope refused | RFC-0059, parked 2026-09-26 | - | this RFC | three rounds done, #1951 |

**(a) Extend RFC-0041's DBSP compiler.** `entity_lower.rs` admits one table or two inner-joined, a
conjunctive `WHERE`, group keys then aggregates, and no CTE, `HAVING` or `QUALIFY`. `entity_expr.rs`
evaluates columns, literals, `i128` arithmetic, comparisons, `CASE`, `COALESCE` and `CAST`. BetSwirl's
fourteen view files hold 20 views with 94 `UNION ALL`, 16 left joins, 5 windows, 10 CTEs and 6 as-of
joins (`p.pos < r.pos` and a `max` joined back), and call 19 scalar functions the subset lacks: `substr`
58 times, `length` 43, `rtrim` 25, `repeat` 13, `chr`, `ascii`, `strpos`, `least`, `string_to_array`,
`TRY_CAST`, `nuthatch_mul_div`, `nuthatch_uint256` and `nuthatch_abi_tuple` among them. Each would be a
second implementation of a Burrmill function that must agree with it byte for byte, which is exactly
the class of defect #1790 shipped (NULL urls from one engine change). DBSP would also hold every join
input resident: `bs_placed` 160,802 rows, `bs_resolution` 160,840, `bs_applied` 160,806,
`bs_payout_multiplier` 160,060, before a single output row. And RFC-0041 §5.3 reseeds from history at
every restart, so the first request after a restart pays the derivation anyway. Incremental per block,
which (d) is not, but months of compiler work and a standing second-engine risk to win a cost BetSwirl
does not pay: it has placed no bet since March.

**(b) Fold-on-seal: materialise over sealed segments, compute the hot tail per request, union.** Only
correct when a view's rows depend on one block's facts. `bet` does not: every one of the 160,806 applied
resolutions lands in a later block than its placement (measured), so any seal between the two leaves a
sealed row that a hot resolution must change, and a union yields the stale row or two rows. `token`'s
counters aggregate the whole history. Making (b) correct means writing the view in carry-and-window
form, which is RFC-0059: an author rewrite of 650 lines, measured at about 100 to 115 ms p99 and 170 to
280 MiB per head on the Network nest, parked by Chief on 2026-09-26 and behind the off-by-default
`folds` feature. It is the right tool for an order-dependent fold whose history is too long to evaluate
at all. BetSwirl's evaluates in 3.6 s.

**(c) Whole-view recompute at commit with a write-once cache.** Correct and simple, but it puts an
O(history) evaluation on the commit path and keys on the commit, so a seal that only moves rows from hot
to sealed recomputes, and a restart loses it. (d) is (c) with those three faults removed: keyed on the
inputs rather than the commit, built behind the reads rather than in the commit, and persisted by
content address.

**(d) Input-keyed copies, chosen.** §3.

**(e) Keep improving the engine.** #1951 did three rounds. Sharing before analysis (burrmill#92) would
take some of the 0.45 s of planning; nothing found brings execution of the shared derivations or a deep
offset under a second, and each remaining idea changes plans for every nest and needs the Lodestar gate.
It stays worth doing for views nobody declares. It is not the answer for this one.

**Also considered: a per-view memo, built lazily.** The same identity, filled by the first reader. It is
what S0 builds, and S1 ships it. Its fault is that the first reader after a change waits for the whole
view (2.4 s for `bet`), longer than one request-time page (2.0 s). S2 makes the build eager.

## §3 - Design

### 3.1 Declaring

`maintained.toml` at the nest root:

```toml
[[view]]
name = "bet"

[[view]]
name = "token"
```

It is authored, so it is part of the NID, and it is outside the data identity, as RFC-0034's
`queries.toml` is: maintaining a view changes no answer, so it must not stop a nest seeding from a mirror
published without it. Open question 1 asks whether it should instead be a key in `nuthatch.toml` with an
exemption.

Refused at load, by name:

- a name that is not an authored view in `views/*.sql`;
- a view whose closure calls a volatile function (`sqlmemo::is_deterministic`);
- a view whose closure reads a relation outside the identity in §3.2. In v1 that is an RFC-0041 entity
  relation, an offchain snapshot view, `labels` or `__children`. Each is admitted later by adding its
  watermark to the identity, as the memo already does.

### 3.2 Identity

```
id(view) = H(view name,
             binary version and engine version,
             content stamp of every authored input file (analytics::cache_inputs),
             (table, file, content hash) of every sealed segment, at or below the served watermark,
               of every base table the view's closure reaches,
             each such table's hot rows, serialized)
```

This is the memo's key (`sqlmemo::Inputs`) minus the statement and plus the closure, which S0 found
necessary: keyed on every table's segments, a seal on a table the view never reads would rebuild it.

The binary version is in it because an engine change can change an answer: 4.3.0's NULL urls (#1790)
would otherwise have been served from copies made by 4.2.x as if nothing had happened, or the reverse.
The hot rows are in it by content, not by `rows_generation`, because a generation counter restarts with
the process and a copy outlives it.

**The hot rows' serialization is canonical.** Tables in name order. Within a table, rows in order of
their canonical bytes below, so a table without a `log_index` (a `[[calls]]` result) is ordered as
surely as an event table. Each row as JSON with its
keys in sorted order and its values exactly as the hot store holds them, which is already canonical
text for every column but the four `UBIGINT` counters (`seal::rows_to_batch`), and `null` for an absent
value. Each table and each row is length-prefixed, as the memo's fields are. The spike hashed rows in
the store's iteration order; S1 replaces that with this. The failure it prevents is a spurious rebuild
when the same rows come back in another order after a restart or a reorg. A non-canonical encoding can
never cause a wrong answer, because two different row sets still serialize differently: it can only miss
a copy that exists.

### 3.3 Reading

In `analytics::attempt`, after the security walk and before views are defined:

1. For each maintained view the statement reaches, compute its closure and identity.
2. If `maintained/<view>/<id>.parquet` exists, the view is a leaf: reachability does not expand its
   body, its authored definition is not registered, and its name is bound to the copy.
3. Otherwise the request proceeds exactly as today, and a build of `<id>` is requested (§3.4).

A statement pinned to a block (`as_of`) never reads a copy in v1; the copy is at the served watermark.
Neither does a statement that surveys the catalogue, which defines everything as before.

Because GraphQL breaks every order on the id (graph-node's rule, `graph_query.rs`) and maintained views
are refused unless their id is unique (open question 3), an answer is a function of the row multiset, so
a byte-identical row set gives a byte-identical answer however the plan differs.

### 3.4 Building

One builder per cursor runs one build at a time, admitted against the cursor's analytics pool and
counted in its permits, like a query. A build runs `SELECT * FROM <view>` through `analytics::run` with
the request's own hot rows and watermark, writes `<id>.parquet.tmp` with `ORDER BY ALL`, and renames it.
Builds are idempotent: two builds of one identity write equal rows, and the last rename wins.

- **S1, lazy:** a request that finds no copy queues one.
- **S2, eager:** after each commit and each seal that changes a maintained view's identity, the builder
  queues it. Latest wins: a queued build whose identity is no longer current is dropped before it runs.
  Builds never run inside the commit or the seal, and S2's acceptance checks tip lag is unchanged.

A block that carries no rows for any table in a view's closure does not change its identity and causes
no build. On BetSwirl today that is every block.

### 3.5 Retention

Per view, keep the copy for the current identity and the `recent` most recent others (default 4), so a
reorg back to a recent state answers from a copy at once. Delete the rest after each successful build.
Copies are derivable, so retention removes files and nothing else.

### 3.6 Verification

`nuthatch check --maintained` rebuilds each current copy, compares it with the stored one by
`EXCEPT ALL` in both directions and by column types, and evaluates the definition twice, which is how a
tie-breaking view that is not deterministic is found. Exit non-zero on any difference.

### 3.7 Restart, seeding, publishing

A copy survives a restart, because its identity holds no process state. A seeded nest has none, and its
first start builds them. The first `dev` after a seed folds the provisional segments (#1150), which
changes segment files without changing a row, so copies built before it are rebuilt once (S0 saw
exactly this, §8). Keying on per-table row digests, as RFC-0059 S2 did, would avoid it; S3 measures
whether it is worth it.

Copies are not published to a mirror in v1. They are derivable, a published one would have to be trusted
or rebuilt to be checked, and a seeded nest builds BetSwirl's in 3.6 s. Open question 2.

### 3.8 The hot tail, honestly

On a chain whose watermark trails the tip, a block that carries rows for a maintained view changes its
identity twice: once at commit (new hot rows), once when the seal moves the same rows to a segment.
Under S2 each change costs one build, and until it lands a request answers from the definition, at
today's cost. On BSC the watermark is about two blocks behind and BetSwirl polls every 60 s, so a
data-bearing poll costs at most two builds, about 7 s of one core, and no reader waits on them. A nest
that writes view rows every block faster than a build completes never has a current copy, and degrades
to exactly today's behaviour: never wrong, never worse, only not faster. That bound is in §6 for
authors. S3 removes the second build by recognising a seal that only moved rows.

## §4 - Goals and non-goals

### Goals

1. Under 1 s cold for every SDK shape on BetSwirl, including a fresh process's first request.
2. Answers byte-identical to the request-time view at every block, by construction and by test.
3. No change at all for a nest that declares nothing.
4. Memory measured, admitted and inside the per-cursor budget.

### Non-goals

- **Not incremental maintenance.** A data-bearing block costs a whole evaluation. Views that need
  per-block cost proportional to the delta are RFC-0041's (aggregates) or RFC-0059's (ordered folds).
- **Not stored state.** A copy feeds no circuit, no entity and no other copy's identity.
- **Not a wider query surface.** `/sql` and GraphQL keep every limit and refusal they have.
- **Not historical reads.** A block-pinned read answers from the definition.
- **Not #357.** A copy belongs to one identity in one dataset; nothing is reused across an NID change.

## §5 - Acceptance criteria

Each can fail. Every one is measured on the ThinkPad unless it says otherwise.

1. **Speed.** BetSwirl's eight SDK shapes (`bets` at skip 0, 100, 5,000 and 150,000, by
   `betTimestamp` desc, by user and `betTimestamp` desc, `tokens`, `bet(id)`), at the default 512 MB
   with the memo off: the first request on a fresh process and the 20-run p50 each under 1 s.
2. **Identity, full history.** Every row of every maintained view dumped through `/sql` is
   byte-identical to the same dump from a binary without the feature, and so is each shape's JSON.
3. **Identity, through a forced reorg.** On a `dev` copy with a reorg injected across rows the view
   reads, every shape's JSON at each head before, during and after equals the request-time answer at
   that head, and no copy file is modified.
4. **No regression where nothing is declared.** The Lodestar release gate on a Lodestar copy: PASS, no
   statement regressed, no answer differs, peak RSS within the gate's tolerance.
5. **Budget.** Process high-water under 2 GB through a full-history build with a request-time read
   beside it; the build's pool peak inside the 512 MB pool.
6. **No build without inputs.** On a live BSC-following copy for an hour, the builder counter rises only
   for blocks that carry rows for a maintained view's closure.
7. **Mutations go red.** Each of these makes a named test fail: dropping the hot rows from the identity
   (criterion 3's test), dropping the binary version (an upgrade test), dropping the closure's segments
   (a seal test), and serving a copy whose file exists for another identity (a tamper test caught by
   `check --maintained`).

## §6 - What to tell authors

Declare a view maintained when it is expensive to evaluate, its whole-history evaluation fits the
analytics pool, and its inputs change less often than it takes to build. For an aggregate that must
cost the delta, write an RFC-0041 entity. For an ordered fold whose history is too long to evaluate at
all, RFC-0059 is the design, and it is parked.

## §7 - Slices

Each slice ends runnable, and each is gated on byte-identical answers against the request-time view
and on the Lodestar release gate before a release carries it (Chief, #1973).

- **S0, the spike.** Done; §8.
- **S1, lazy maintained views.** `maintained.toml` and its load-time refusals, the identity of §3.2
  including the binary version, the read path of §3.3, the lazy builder with admission and one build
  per cursor, retention, and metrics (builds, hits, fallbacks, copy bytes) on `/metrics` and `/ready`.
  *Accept:* criteria 1, 2, 4 and 5, and the closure, upgrade and tamper mutations of criterion 7.
- **S2, eager builds and the reorg.** Builds queued after commit and seal with latest-wins, never inside
  either. *Accept:* criterion 3 with its mutation, criterion 6, and tip lag's after-seen p50 unchanged
  within the CI gate.
- **S3, verification and the double build.** `nuthatch check --maintained` (§3.6), and a seal that only
  moved hot rows reuses the copy instead of rebuilding. *Accept:* `check` fails on a tampered copy and on
  a tie-breaking view; a data-bearing BSC block causes one build per view, not two.
- **S4, BetSwirl in production.** Declare the seven entity views in the nest; its dataset identity is
  unchanged and it still seeds from the mirror; retire the warm timer. *Accept:* through the public
  endpoint, the first SDK `bets` query under 1 s, answers byte-identical to before.

## §8 - S0 measured

**Verdict: continue.** Every number S0 was asked for is below. Two things it did not measure are said
at the end.

**The spike.** Branch `pete/rfc-0062-s0-spike` (8e9e8cb4 then 7f282c34), never merged, no flag:
`NUTHATCH_S0_MAINTAINED=bet,token,user,affiliate,game_token,weighted_game_bet,weighted_game_config`
declares the views. It is §3.3 and the lazy half of §3.4 without the binary version in the identity,
admission or retention: 255 added lines in `src/maintained.rs` and `src/analytics.rs`.

**Method.** ThinkPad (32 cores, 62 GB, shared with other work, load average 14 to 20 throughout). A
fresh clone of `betswirl-bnb-nest` at 92d6588, seeded from the public mirror (dataset `f00657c7`, 346
segments, history to block 126,268,093) into `~/stopgap-perf/w1973`; production's unit, directory and
warm timer untouched. Baseline is `main` at 8fce5fac (4.13.0), both built `--features graph` on the
pinned 1.95.0 toolchain. `serve` with the memo off, default 512 MB engine limit, default analytics
threads. Scripts and raw output are in `~/stopgap-perf/w1973/results/` on the ThinkPad.

**Cold time per shape**, in seconds. p50 of 20 runs in one process, then the median of five fresh
processes' first requests.

| shape | today, p50 | maintained, p50 | today, first request | maintained, first request |
|---|---:|---:|---:|---:|
| `first: 20`, skip 0 | 2.020 | 0.192 | 1.979 | 0.230 |
| skip 100 | 2.031 | 0.192 | 2.035 | 0.229 |
| skip 5,000 | 2.159 | 0.216 | 2.233 | 0.262 |
| skip 150,000 | 3.072 | 0.760 | 3.024 | 0.838 |
| orderBy betTimestamp desc | 2.326 | 0.309 | 2.093 | 0.361 |
| where user, betTimestamp desc | 2.218 | 0.176 | 2.049 | 0.224 |
| `tokens` | 0.864 | 0.029 | 0.909 | 0.063 |
| `bet(id)` | 2.160 | 0.096 | 1.936 | 0.127 |

The slowest of all 40 fresh first requests was 0.873 s (skip 150,000). One 20-run series had a first
run of 2.03 s that the 40 fresh-process runs did not reproduce; it is reported, not explained.

**Memory**, MB: engine pool peak, then process high-water, on a fresh process per shape.

| shape | today | maintained |
|---|---|---|
| skip 0 / 100 / 5,000 | 212-213 / 838-855 | 0-21 / 189-206 |
| skip 150,000 | 302 / 672 | 173 / 426 |
| betTimestamp desc | 212 / 826 | 20 / 197 |
| where user | 212 / 846 | 0 / 239 |
| `tokens` | 41 / 405 | 0 / 151 |
| `bet(id)` | 130 / 812 | 0 / 164 |

**State on disk.** 43,414,973 bytes for seven copies: `bet` 43,018,195, `user` 293,440,
`weighted_game_bet` 88,186, `token` 7,218, `game_token` 4,890, `weighted_game_config` 2,354,
`affiliate` 690. The `bet` copy was the same size on all five rebuilds. Against 52 MB of sealed
segments, the copy nearly doubles the nest on disk.

**Time to materialise the full history**, five runs from an empty `maintained/`: `bet` 2.40 to 2.59 s,
`token` 0.78 to 0.82, `game_token` 0.15 to 0.16, each of the other four under 0.14; about 3.6 s for all
seven, built one at a time. Pool peak 349 to 357 MB. Process high-water 1,316 to 1,342 MB, with the
request that triggered the builds running beside them.

**Cost per new block.** A `dev` copy of the seeded nest followed BSC for 15 minutes with production's
flags (60 s poll, the same keyed endpoint) and the spike, answering a skip-0 page and `tokens` every
10 s. The head moved 1,875 blocks (126,272,678 to 126,274,553) with the watermark one or two
blocks behind. All seven copies were rebuilt once, on the first request: the copied copies' identity no
longer matched after the first start rewrote the seeded segments (§3.7), and that request answered from
the definition in 2.30 s. After it, no build at all across 15 polls, because no block carried a
BetSwirl row. The 86 later skip-0 pages had a p50 of 0.234 s (0.214 to 0.261) and the 93 later `tokens`
answers 0.057 s, which is the per-request cost of computing the identity included. A block that does
carry rows costs one rebuild of each view whose closure it touches, so at most the 3.6 s above, off the
request path; S0 had no such block to observe, and S2's criterion 6 is where it is counted.

**Identical answers.** All eight shapes' JSON byte-identical between the two binaries, both in the
20-run process and on fresh processes. Every row of the seven views, read through `/sql` in id order
(keyset pages of 40,000, list columns as `to_json`), byte-identical: 167,540 rows, 141.4 MB of JSON;
`bet` alone 160,802 rows and 140,922,340 bytes. Ids are unique in all seven views (checked).

**Evidence for §2, from the same data.** `bs_placed` 160,802 rows, `bs_resolution` 160,840,
`bs_applied` 160,806, `bs_payout_multiplier` 160,060, `bs_house_edge` 86, `bs_freebet` 859. All 160,806
applied resolutions are in a later block than their placement.

**What S0 did not measure.**

- **A reorg.** Not in S0's brief, and the spike's identity covers hot rows by content, which is what
  makes it safe. Criterion 3 is S2's.
- **The Lodestar gate.** The spike's request path is unchanged when nothing is declared (the
  declaration is read and found empty), but a gate run is S1's criterion 4, not an argument.

## §9 - Risks

- **History grows, and the build with it.** 3.6 s today is O(history). A nest writing view rows more
  often than a build completes gets no benefit (§3.8). The builder metrics make that visible, and §6
  names the alternatives.
- **Disk.** A copy is roughly the view's size, here 43 MB against 52 MB of segments, times retention.
- **Build memory beside reads.** 1.34 GB high-water with one request beside the build. Admission
  against the pool bounds the engine side; S1 measures the process side under concurrent reads.
- **A tie-breaking view.** Not detectable at load. `check --maintained`'s double evaluation finds it.
- **Engine-version churn.** Every upgrade rebuilds every copy once. That is the point, and it costs
  one build per view.

## §10 - Open questions

1. `maintained.toml`, or a `maintained = true` key on a view in `nuthatch.toml` with a data-identity
   exemption?
2. Should a mirror publish copies keyed by identity, so a seeded nest starts fast? They are verifiable
   only by recomputing, which costs what building costs.
3. Should a maintained view be required to have a unique `id`, or only one used by GraphQL? `/sql`
   answers without an `ORDER BY` are already unordered.
4. Should a copy be served at an `as_of` that equals the copy's own watermark?
5. Is `recent = 4` right? It is a guess sized to a reorg depth, with nothing measured behind it yet.
