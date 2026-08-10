# Sprint: circumspect-capybara (2026-08-07 - 2026-08-21)

Successor to [boisterous-badger](sprint-boisterous-badger.md), which closed on 2026-08-06 with the
2.0.0 release. Companion to [prod-readiness.md](prod-readiness.md) (the grades this sprint is trying
to move) and [verification.md](verification.md) (where the evidence lands).

**Scope is exactly the thirteen issues below and nothing else.** Every one carries the
[`circumspect-capybara`](https://github.com/nightswatchhq/nuthatch/issues?q=is%3Aopen+label%3Acircumspect-capybara)
label, which is the machine-readable form of this document - verified 2026-08-10 to be exactly these
thirteen and nothing else. This is a queue, not a plan: the reasoning for the ordering is here, the
work lives in the issues, and the issues are the source of truth for status.

**Status as of 2026-08-10: 3 of 13 closed** - #362 (branch protection), #361 (retry hints, PR #369)
and #294 (single-pass rebuild, PR #372). The count is stated here only so that a reader running the
query below is not surprised to get 10; do not maintain a status table in this file, because it will
be wrong before it is read.

**Two weeks, not one.** The predecessors ran four and seven days, but this one carries a real feature
build (item 1) alongside the standing queue, and a one-week window would only produce a slipped
deadline and a document nobody trusts. If it finishes early, close it early.

> **Listing quirk, noted so nobody concludes the label failed.** `gh issue list --label
> circumspect-capybara` returns **zero** while `gh issue view <n>` shows the label present. The REST
> route is correct and is what the count above comes from:
> `gh api "repos/nightswatchhq/nuthatch/issues?labels=circumspect-capybara&state=open&per_page=50"`.
> The web UI filter is also correct.

## Definition of done, for every item in this sprint

An item is not finished when its tests pass. It is finished when it has been **verified on two
machines**:

1. **The Linux dev box** (`/home/pepe/nuthatch`, Manjaro, GCC 15). Note every build here needs
   `CFLAGS="-std=gnu17" CXXFLAGS="-std=gnu++17"` or it dies in `mimalloc` via `dbsp`, and CI cannot
   catch that, because no GitHub runner image ships GCC 15 - not because of a pin. (Only the
   compose-fleet job pins `ubuntu-22.04`, at `ci.yml:94`, and that is a glibc constraint; every other
   job is `ubuntu-latest`.)
2. **The Hetzner production box.** Which is a live deployment running three services, so: never
   `pkill`, the units are `Restart=always`, and one of the three is not named `nuthatch*`. Health-check
   end to end afterwards rather than assuming a clean restart means a clean service.

The reason is the standing practice below - *run it, do not reason about it*. Nearly every defect in
the last two sprints was a composition or environment failure that no unit test could reach: glibc
between builder and runtime three separate times, mount ownership that only breaks on Linux, a token
guard versus Docker's port binding. **A green CI badge is not evidence, and neither is a green run on
one box.**

Record the evidence in [verification.md](verification.md) as it lands, not at the end.

## How this is ordered

**Items 1-3 are the architecture-session gaps**, front-loaded by decision. They are the outstanding
half of the design agreed in the August session: the parts that were designed and not built, or were
answered by deflecting the question rather than testing it. They come first because the design is
still fresh and because two of them are cheap decisions rather than builds.

**Items 4-13 are the standing queue**, ranked on four criteria **weighed together, not applied
strictly in order** - an earlier criterion is the heaviest input to a position, not a guarantee of
one.

1. **A live defect.** Something that is wrong right now, in shipped code.
2. **A CLAUDE.md non-negotiable at risk.** The five are not negotiable by definition, so an unproven
   or ungated one outranks any amount of polish.
3. **A published claim that is unproven.** We say it in the README or the docs and have not run it.
4. **Cost against leverage.** Cheap things that unblock expensive things go early.

**Three rows depart from a strict reading of that ranking.** Naming them, because the ordering is the
only thing this document contributes that the issue list does not, and an unexplained exception makes
the whole ranking untrustworthy:

- **#297 (criterion 1) sits at 6**, below four items on weaker criteria. It is the only live defect
  here, and by a strict reading it should lead. It does not because it fails *loudly* - a factory nest
  over the cap dies rather than corrupting - so nothing is silently wrong while it waits, and items 4
  and 5 gate a release where this does not.
- **#362 (criterion 4) sits at 7**, above items on criteria 2 and 3. Cheap-unblocks-expensive is the
  weakest criterion, but this one guards the merges every other item produces, so leaving it to its
  strict rank would mean landing twelve items through an ungated `main`. Do it first in wall-clock
  terms regardless of its number.
- **#292 (criterion 2) sits at 10**, below two criterion-3 items. A third of it landed with #348, so
  what remains is narrower than the criterion implies, and it pairs naturally with #287 directly below
  it - splitting them would mean covering the same surface twice.

> **Recorded tension.** Front-loading 1-3 puts the ≤2 GB budget work (4 and 5) behind them, and that
> budget is a non-negotiable while items 1-3 are design completeness. If the window runs short, 4 and
> 5 are the ones that must not slip - they are the only items here that a release gate depends on.

---

## 1-3: the architecture-session gaps

| # | Issue | Why here |
|---|-------|----------|
| 1 | [#364](https://github.com/nightswatchhq/nuthatch/issues/364) Early cutoff never runs in the runtime | Slice 5 shipped the mechanism and `Manifest::data_identity()` has **exactly one non-test caller**: `migrate.rs:292` (the other six call sites are all tests in `src/blob.rs`). So adoption happens during the one-time layout migration and the runtime's mount path never consults it. The published "~0.14s adoption on a 428 MB nest" figure is `nuthatch migrate`, not a live edit. |
| 2 | [#365](https://github.com/nightswatchhq/nuthatch/issues/365) DDoS: a flat request ceiling, or the gateway's job | Closed by a **decision**, not necessarily by code. A single expensive query is well bounded (50k rows, 2 concurrent, 16 KB, timeout); request *volume* is not bounded at all, and the answer currently lives in a log line. Either answer is defensible; having neither written down is not. |
| 3 | [#366](https://github.com/nightswatchhq/nuthatch/issues/366) SQL everywhere: test the case for replacing redb | We answered a different question. RFC-0035 §5 measured whether DuckDB can `ATTACH` SQLite (it cannot, re-confirmed against `duckdb` 1.10505.0) and then treated the topic as settled. The one-mental-model argument is untouched by that measurement, and **#296 is partly an artefact of using a key-value store** - a connection drawn nowhere. Sequenced behind #283. |

## 4-13: the standing queue

| # | Issue | Why here | Criterion |
|---|-------|----------|-----------|
| 4 | [#286](https://github.com/nightswatchhq/nuthatch/issues/286) The ≤2 GB budget under a large-ABI, high-event-rate contract at tip | ⛔ in prod-readiness §5. The budget is proven for sparse nests only, and CLAUDE.md says to treat it as CI-enforced rather than aspirational. Everything else is built on it. | 2 |
| 5 | [#284](https://github.com/nightswatchhq/nuthatch/issues/284) Peak-RSS regression gate for the dense multi-nest scenario | The other half of the same non-negotiable, and the more damning half: the CI gate is a single nest at `BACKFILL_BLOCKS=2000` against `MAX_RSS_MB=256` (`.github/workflows/footprint.sh:75`). The dense case was measured once, out of band, and never wired in. | 2 |
| 6 | [#297](https://github.com/nightswatchhq/nuthatch/issues/297) COR-5: a factory nest over the getLogs cap has no recovery path | The only genuine defect in the queue. It fails loudly rather than corrupting, which is why it was deferred, but factories are a headline feature and busy chains reach the cap. | 1 |
| 7 | [#362](https://github.com/nightswatchhq/nuthatch/issues/362) `main` has no branch protection, so `--auto` merges immediately | Minutes of work, and it guards everything else in this list. Demonstrated on 2026-08-07: `--auto` merged #360 with four checks still running (#360 merged 10:58Z, #362 filed 11:04Z). | 4 |
| 8 | [#283](https://github.com/nightswatchhq/nuthatch/issues/283) Entity point-read p50/p99 bench, tracked across releases | The prerequisite for the whole performance cluster. Without it #293-#296 are optimising blind and no delta can be proven. CLAUDE.md: "Benchmarks are CI artifacts… Regressions fail the build." | 3 |
| 9 | [#356](https://github.com/nightswatchhq/nuthatch/issues/356) RFC-0021: cross-cursor stall isolation is untested | A reorg is rare and we test it; a dead RPC endpoint is a Tuesday. If one chain's stall took a healthy co-tenant's cursor down with it, that reads as a nuthatch fault rather than a provider one. RFC-0021's own criterion, never extracted because that RFC has no slice table. | 3 |
| 10 | [#292](https://github.com/nightswatchhq/nuthatch/issues/292) Review bind and exposure defaults end to end | A third of it went in with #348. The two that remain - admin routes and the control plane - are exactly what an operator exposes. | 2 |
| 11 | [#287](https://github.com/nightswatchhq/nuthatch/issues/287) Re-run the full adversary pass on the untrusted surface | The last pass covered only 2.0's new surface. Pairs with #292: same gap approached from the other side. | 3 |
| 12 | [#361](https://github.com/nightswatchhq/nuthatch/issues/361) Honour a provider's own retry hint instead of guessing a backoff | Blocks #308's remaining run being cheap, and cuts provider spend directly. That spend has already cost real money once. | 4 |
| 13 | [#294](https://github.com/nightswatchhq/nuthatch/issues/294) Restart rebuild does three full scans; make it one | Operator-visible on every upgrade and every crash recovery, which is not a rare path - we hit it during the 2026-08-07 verification run. | 4 |

## The shape of it

**2 and 3 are decisions, not builds.** Both can be closed by writing down an answer and defending it,
and #366's first checkbox (correct the Turso licence gate in `backlog.md`, which is factually wrong)
takes a minute. Do these while something else compiles.

**1 is where the real build is.** It was item 2 until 2026-08-07, when the item above it (#357,
cross-nest grafting reuse) was re-parked: its acceptance criterion could not fail, because two nests
with identical derivation keys necessarily share a data identity and therefore become one dataset
under this very item. See the #357 thread.

**4 and 5 are one piece of work.** Prove the budget on a dense nest, then gate what you proved. Doing
them apart means measuring twice. **These are the items that must not slip**, per the recorded tension
above.

**6 is the only real bug** in the list. Everything else is an unproven claim, a missing gate, a
decision, or a cost.

**7 is nearly free**, and per the ranking departures above it should go first in wall-clock terms
regardless of its number, because it protects the merges that the other twelve will produce. Done on
2026-08-07: `main` now has six required contexts, `strict` and `enforce_admins` on.

### One correction to the #286 premise, found 2026-08-07

The issue says "we now own that nest", and that is true, but the Uniswap V4 nest is **not in this
repository**. It lives at [`nightswatchhq/uniswap-v4-ethereum`](https://github.com/nightswatchhq/uniswap-v4-ethereum)
and is published 🟢 in the [nests index](https://github.com/nightswatchhq/nests). Step one of #286 is
therefore to clone it, not to point at a directory here. Its shape is confirmed as the right worst
case: a single address (`0x0000…8a90`, the singleton `PoolManager`), a 27 KB ABI, ten event tables,
no factory. **No measurement has been taken yet** - this note records the setup only.

## Deliberately not in scope

Named here so that leaving them out reads as a decision rather than an oversight.

- **OBIB coverage** ([#306](https://github.com/nightswatchhq/nuthatch/issues/306),
  [#308](https://github.com/nightswatchhq/nuthatch/issues/308)) - visible and satisfying, but it is a
  claim about *speed*, while #286 is a claim about *correctness under load*. Also gated on #361 if it
  is to be run cheaply.
- **The performance cluster** ([#293](https://github.com/nightswatchhq/nuthatch/issues/293),
  [#295](https://github.com/nightswatchhq/nuthatch/issues/295),
  [#296](https://github.com/nightswatchhq/nuthatch/issues/296) - note #296 is now also named as the
  payoff in #366) - should wait behind #283 so the wins
  are measurable rather than asserted.

## The standing practice this sprint inherits

Unchanged from boisterous-badger, and reinforced again on 2026-08-07 when a green CI badge turned out
not to be evidence that the build works on a developer's own box (GCC 15 versus the vendored mimalloc
in `dbsp`):

- **Run it, do not reason about it.**
- **Ask who calls this.**
- **A skip is not a pass**, and a mutation that does not mutate is not a test.
- **Re-test gates before planning around them.** A gate recorded once and never re-checked becomes
  folklore, and folklore decides what you build next.
- **Benchmark against a real provider, not a mock**, and test the confound.
- **Keep verification.md's "what we have verified" table honest.**
