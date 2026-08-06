# Sprint: boisterous-badger (2026-07-30 - 2026-08-06) - **closed**

> Closed 2026-08-06 by the **2.0.0 release**. The road-to-1.0 plan below was overtaken: 1.0 shipped
> on 2026-08-04 and the August architecture session then produced RFCs 0032-0035, which became 2.0 -
> tenancy in the runtime, nest identity and grafting, the query allowlist, and one deliberate
> migration. The quality track survives as issues #287-#309; **those issues are the live state, this
> file is the record of what was planned.**
>
> Read it for *why*, never for *what is left*. The queue answers that:
> [open issues](https://github.com/nightswatchhq/nuthatch/issues).

Successor to [amiable-axolotl](sprint-amiable-axolotl.md), which closed with RFC-0022 built, released
(0.8.0/0.8.1), verified on a real box, and documented. Companion to [backlog.md](backlog.md) and
[prod-readiness.md](prod-readiness.md).

## Read this before planning around it

**Three of the four scope items are gated on something other than effort.** That is not an accident of
scheduling - it is what the board actually looks like now that the laptop-buildable work is finished.
Writing it down honestly beats a sprint that reads as a work queue and then stalls in week one.

| Item | Gate | Can we start today? |
|---|---|---|
| RFC-0013 DataFusion | a **benchmark** we must run and honour | Yes - the benchmark is the work |
| RFC-0023 tier 3 | ~~an archive RPC endpoint~~ **nothing** | **Yes - the gate was wrong** |
| RFC-0003 / 0014 extraction | a **colocated reth node**, deferred by decision 2026-07-29 | No |
| GraphOps | a **conversation**, not a dependency | Yes - and it is the cheapest item here |

So the honest ordering is: **talk to GraphOps first** (it costs nothing and may reorder everything
below it), then **RFC-0013's DataFusion benchmark** and **RFC-0023 tier 3** - both buildable - with
only RFC-0003/0014 genuinely staged-and-waiting on hardware.

**On that hardware (2026-07-30):** reth needs a Hetzner *dedicated* box (~2x1.92 TB NVMe). The largest
Hetzner **Cloud** instance tops out at 960 GB of local disk, and Cloud volumes are network-attached -
reth's IOPS profile makes syncing on them impractical rather than merely slow. A Cloud API token
cannot order a dedicated server. Everything else in this sprint runs on Cloud for a few euros.

---

## 1. GraphOps - hand them something concrete

The cheapest item and the one most likely to change the others' priority. Unlike every previous
iteration of this conversation, there is now something real to hand over:

- **v0.8.1**, with an embedded artifact and a `-scaled` one
- **[verification.md](verification.md)** - a falsifiable acceptance runbook, executable via
  `scripts/verify.sh`, that states which levels *we* have verified and which we have not
- **`scripts/fleet-lab.sh`** - provisioning they can read rather than trust, standing up the same lab
  we used, on their own account

**What to ask for, specifically:** a multi-machine level-5 run. It is the one claim we cannot make -
everything is verified with multiple processes against one host, which is genuinely equivalent for
every invariant tested, but is not partitions or clock skew.

**What their answer changes:** whether RFC-0022 slice 3+ needs anything else before fleet use, and
whether the scheduler's placement policy matches how they would actually operate. Both are cheaper to
learn now than after building more on top.

## 2. RFC-0013 - DataFusion, behind its gate

The only unblocked build in this sprint, and **the gate is the deliverable**: `nuthatch bench`
comparing DuckDB and DataFusion on latency and RSS, on real hardware, before anything is retired.

The temptation is to skip it because scaled mode moved and federation "obviously" belongs there. Do
not. The gate exists because DuckDB works, is embedded, and costs nothing; replacing it has to be
*earned* with a number rather than argued from architecture.

Two things to hold onto:

- Build **scaled-side first** (RFC-0013 §2/§4). Zero risk to the working embedded path, which is the
  primary deliverable and must not regress for a query-engine experiment.
- The per-cursor RAM budget applies. A federation layer that is faster and 400 MB heavier has not won.

## 3. RFC-0023 tier 3 - pinned-block `eth_call`

**Correction (2026-07-30): this was never blocked.** The gate was recorded as "an archive endpoint",
which was read as "an archive *node*". RFC-0023 §11 asks for an operator-supplied archive **RPC**, and
a stock Alchemy key serves pinned-block `eth_call` today - verified against USDC `totalSupply()` at
blocks 12,000,000 and 15,000,000, two heights, two correct answers.

So the verification half *can* start, which changes this from "design only, staged and waiting" to a
buildable item with its acceptance test already written: the same declared call at the same block
re-executes to a byte-identical result across runs and machines.

The lesson worth keeping: a gate recorded once and never re-tested becomes folklore. This one cost us
a sprint of treating a buildable item as blocked. **Re-test gates before planning around them** - the
same standing practice as "run it, don't reason about it", applied to the backlog rather than the code.

Its acceptance test is already written: the same declared call at the same block re-executes to a
byte-identical result across runs and machines.

## 4. RFC-0003 / 0014 - extraction

**Re-deferred 2026-07-30**, second time, with the cost now priced rather than vague: reth needs a
Hetzner **dedicated** box (~EUR 110/mo + setup, 2x1.92 TB NVMe minimum) and **days of sync**. The
largest Hetzner *Cloud* instance has 960 GB of local disk and Cloud volumes are network-attached, so
there is no Cloud path to it - a Cloud API token cannot order a dedicated server at all.

Everything else in the pre-1.0 set is reachable without it. This one is a purchase decision, not an
engineering one, and it is parked until someone makes that purchase. **Do not re-raise it as a
blocker** and, per §3's lesson, **do not let "blocked on reth" spread to items that are not** - it has
already happened once to RFC-0023.


**Blocked on the reth box, deferred by decision on 2026-07-29. Do not re-raise it as a blocker** - the
deferral is recorded in [backlog.md](backlog.md) Track 1.

Kept in scope so the sprint states what *would* happen if that decision changed: a full node unblocks
0003 (ExEx tip mode) and an honest tip-lag number; archive unblocks 0014's extraction, whose decode,
schemas and volume guard already shipped in slice 0. The keyspace collision recorded in RFC-0014's
slice-0 note has to be solved before extraction wires up.

---

## Landed since this was written

- **RFC-0029 slices 1-3** shipped as **v0.8.2** (2026-07-30): the Alchemy HTTP-400 classifier fix that
  was killing backfills, an honest benchmark harness, and the concurrent timestamp fan-out with a
  reorg-invalidated cache.
- **RFC-0029 slice 4** - demand-driven timestamps, built as §6b-i argued: an `init`-time declaration
  (`init --no-timestamps`), refused as an in-place edit, with a `schema_version = 2` stamp so an older
  binary refuses a timestamp-free nest rather than indexing timestamps into it. Breaking schema change
  → **0.9.0**.

Slice 5 (adaptive windows on the pipelined path) is the remainder of RFC-0029 and is unblocked.

---

## The road to 1.0 (decided 2026-07-30)

**Goal: close every RFC that does not need a reth node, then decide about 1.0.** Reth is a purchase
decision (§4), so the pre-1.0 set is defined by what a laptop, a Hetzner **Cloud** token and an archive
**RPC** can reach - which turns out to be everything else.

Ordered by dependency, not by appeal. Spend is batched deliberately: Hetzner bills hourly, so the
Cloud items come up together and go down together rather than billing while someone thinks.

| # | Item | Needs | Spend | Gate |
|---|---|---|---|---|
| 1 | ~~**0.9.0**~~ **done** - RFC-0029 complete | - | - | shipped, plus 0.9.1 and 0.9.2 |
| 2 | ~~**RFC-0022 slice 3b**~~ **done** (2026-07-31) | - | - | FE mount is `:ro` again |
| 3 | **Multi-machine verification** - **built, partly proven** | 3 Cloud boxes | ~EUR 0.03/hr | the lab distributes now; **skew PASSED across machines**, `partition` still blocked on issue #250 |
| 4 | **RFC-0013** - DataFusion | one `ccx63`, hourly | ~EUR 0.30/hr | **the benchmark is the deliverable** |
| 5 | **RFC-0023 tier 3** - foundation **done** (2026-07-31) | - | - | pinned call + content-addressing + acceptance test **against the real chain**; scheduling/sealing still to come |
| 6 | **GraphOps** | a conversation | none | cheapest item; may reorder 3-5 |

#### 2026-07-31: a release that described a fix it did not contain

Worth recording in the plan rather than only in a release note, because it changed how releases are cut
here.

**v0.9.1 shipped notes for a source fix that was not in the binary.** PR #233 was titled "timestamp
batches narrow on a size failure instead of retrying" and merged with `ci.yml`, `Cargo.lock` and two
docs files - **no `src/rpc.rs`**. A `git stash` run while switching branches removed the fix *and its
test*; what remained was committed without the file list being checked. 0.9.2 corrects it.

**CI was green throughout and proved nothing**, because the test that would have failed was in the same
uncommitted file as the fix. Every check ran against a codebase consistent with neither existing. *A
green pipeline is only evidence about the code that reached it.*

Two practices, now standing:

- **Verify a release against `git show --stat` for its range, checked against the notes** - not against
  what the commit messages intend to say. Notes claiming an RPC fix with no `src/` file in the diff is
  the entire tell, and it takes seconds.
- **Never `git stash` mid-task; commit to a WIP branch.** This is the mirror of the `git add -A` lesson
  already recorded - that one silently *adds* work, this one silently *removes* it.

The same day produced a smaller instance of the same class: a `clippy` type-complexity fix whose edit
script failed its assertion, leaving the warning in place. `grep -c` reported a nonzero count and it was
not acted on; CI caught it. **A nonzero warning count is a stop, not a note.**

### 2026-07-31: item 3 was mis-scoped, then built the same day

**Resolved.** The lab now genuinely distributes - control plane and store on one box, workers on their
own, reaching it over a private network - and **the clock-skew invariant PASSED across machines**: a
worker's clock moved 10 minutes and its lease expiry moved 66 s on the *database's* clock. `partition`
remains blocked, on issue #250 rather than on the harness.

The account below is kept because the diagnosis is the useful part:

Standing the lab up turned a one-hour run into a build item, and the plan was wrong about it in the
same way two other entries in this sprint were.

**The `multi` shape provisions three boxes and then runs the whole compose fleet on one of them.** The
other two are empty. `partition` and `skew` target hosts labelled `writer`, so they would have
firewalled and clock-skewed idle machines and reported the outcome as a result. Every document here
described level 5 as "nothing has run across real machines", which reads as *we have not got round to
it*. The truth is the harness cannot do it.

**What that build needs** (none of it a config tweak): Postgres is published on `127.0.0.1` only, so a
remote writer cannot reach it over the private network; the writer boxes need role-specific startup
pointing at the control box's private address; and the compose topology needs splitting per role.

Getting even that far took **four** tooling fixes found only by running it - a 422 network payload that
meant `multi` had never created a box at all, a missing capacity fallback, a 409 on a leftover network,
and error handling that reported `curl: (56)` instead of the provider's own reason.

**What did hold:** 10/10 level-5 checks on a clean box against the **published v0.9.2 artifacts**. Plus
a harness flaw worth knowing: checks run ~23 s after `compose up`, so 5.1b (worker registration) can
fail on a first pass and pass on a re-run. **Re-run before reporting it.**

## The quality track (runs alongside 1-6, not after)

Closing RFCs is not the same as being ready to call something 1.0, and this is the half that decides
it. These are **not** gated on anything and should be interleaved with the items above rather than
queued behind them - a security finding on day 8 is much cheaper than one on day 1 of 1.0.

| # | Item | What it actually means |
|---|---|---|
| 7 | **Security audit** | The untrusted surface is `/sql`, the admin routes, the control-plane API and anything reading a nest bundle. Prior waves found a stacked-`COPY TO` arbitrary file write (#153) and an ABI-path traversal (#149) - assume more. Re-run the full adversary pass, not just the regression tests for known finds. **Also: publish GHSA-jvjx-5528-r6mm**, fixed in 0.6.2 and still unpublished |
| 8 | **Hardening pass** | Fault quarantine (RFC-0026) covers indexing faults; the gaps are elsewhere. Property-test reorg convergence at depths we have not tried, and fuzz the decode path against malformed ABIs and logs |
| 9 | **Performance audit** | We now have honest instruments (RFC-0029 §6e) and one real workload. Extend to the RFC-0004 W1/W2/W3 set, publish `bench-report.json` per workload with provider+hardware, and set the CI regression thresholds off *measured* numbers rather than guesses. Includes the still-open `--concurrency 16` vs `1` criterion |
| 10 | **Docs pass** | The two stale-doc corrections this sprint were not bad luck - five wrong claims surfaced in one week. Verify every claim against a running binary, not against memory. `verification.md`'s "verified by us" table is the honest core; keep it honest |
| 11 | **Website** | Currently says nothing about OBIB, scaled mode, or the roost. It is the first thing GraphOps and every operator sees, and it is a release behind the product |

**Ordering note:** 10 and 11 should come *after* 1-6, or they will be rewritten twice. 7-9 should start
immediately and run continuously - they are the ones whose findings change what gets built.

**Parked, and only these:** RFC-0003 (ExEx tip mode), RFC-0014 (extraction), **RFC-0030 and RFC-0031** (adding EVM chains; parked 2026-08-04 by priority call, not difficulty - though RFC-0031's Optimism half is blocked externally, and RFC-0030 slice 3 is carved out because two shipped Arbitrum defaults cannot serve a backfill, issue #267), RFC-0024 (eth-call
execution engine - a research note behind 0023 by its own framing). **0003, 0014 and 0024 wait on the
dedicated box in §4; 0030 and 0031 do not** - they are parked by choice and buildable whenever we
choose, except RFC-0031's Optimism half, which waits on a second qualifying endpoint existing.
Nothing else is blocked on anything.

**What "1.0" should mean, before anyone assumes:** this list closes the RFCs we *can* close. It does
not by itself make the version 1.0 - that is a separate judgement about API stability and the §683
stability contract, and it should be argued on its own terms rather than falling out of a checklist.
Worth settling explicitly when items 1-6 are done, not before.

### Two corrections that shaped this list

Both were **stale documentation**, not engineering, and both cost real time - which is why the
standing practice below now includes re-testing gates:

- **RFC-0022 was marked "design only, build deferred"** in the RFC index while it had been shipped
  (0.8.0/0.8.1) and fleet-verified 10/10 for a week. The index is the first thing an operator reads.
- **RFC-0023 was marked blocked on "an archive endpoint"**, which got read as "an archive *node*". The
  RFC asks for an archive **RPC**; a stock Alchemy key serves pinned-block `eth_call`. A buildable item
  sat blocked for a week over a compressed phrase.

---

## Carried over, not scope

Small, and worth doing when they block someone rather than on a schedule:

- **Multi-machine verification** (`fleet-lab.sh up multi`, then `partition` and `skew`). Not listed
  above because it is an hour and a few euros, not a sprint item - but it is the last unverified claim
  in prod-readiness §11, so do it before telling anyone RFC-0022 is proven across machines.
- ~~**RFC-0022 slice 3b**~~ **done 2026-07-31.** Smaller than it looked: the rebuild helpers already
  took `&dyn HotStore`, and the only concrete dependency was `build_nest` calling `Store::open`
  *unconditionally* - so a query-FE handed a `store_override` still created a redb file it never read.
  The hot store is now resolved once and nothing downstream knows which backend it got. FE mount is
  `:ro`.
- **Operator-side, not ours:** rotating the credentials that went through the 2026-07-29/30 session
  transcript, and publishing GHSA-jvjx-5528-r6mm if it is to be public - the fix shipped in 0.6.2.

## The standing practice this sprint inherits

From amiable-axolotl, reinforced hard by the 0.8.0 → 0.8.1 sequence:

- **Run it, do not reason about it.** Every bug in the last stretch was a composition or environment
  failure - a migration race between processes, a token guard versus Docker's port binding, mount
  ownership that only breaks on Linux, glibc between builder and runtime three separate times, shell
  functions not crossing a subshell. None was reachable by a unit test.
- **Ask who calls this.** `reconcile::tick` had six passing tests against a live Postgres and no
  caller, so three documents described behaviour that did not exist.
- **A skip is not a pass**, and a mutation that does not mutate is not a test. Both bit this week -
  and again on 2026-07-30, twice in one afternoon: `cargo test --lib "a\|b"` silently matches **nothing**
  (cargo takes a substring, not a regex), so a mutation run reported "ok" across the board while
  testing zero tests; and a dense-range test passed against a deliberately broken controller because
  its range was too short for the runaway to show.
- **Re-test gates before planning around them.** A gate recorded once and never re-checked becomes
  folklore, and folklore decides what you build next. Two items in this sprint were mis-blocked by
  stale notes (see "The road to 1.0"). This is "run it, do not reason about it" applied to the backlog.
- **Benchmark against a real provider, not a mock.** The OBIB A/B found a backfill-killing defect
  (`fix/rfc-0029-body-read-timeout-is-narrowable`) that no mock could surface, because mocks do not
  stream slow multi-megabyte bodies. And **test the confound**: the first OBIB run was 3.9x slower than
  the second, which looked exactly like provider caching until a shifted-range control ruled it out.
- **Keep verification.md's "what we have verified" table honest.** Moving a row to ✅ without evidence
  recreates the problem the document exists to solve.
