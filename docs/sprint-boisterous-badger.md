# Sprint: boisterous-badger (2026-07-30 - )

Successor to [amiable-axolotl](sprint-amiable-axolotl.md), which closed with RFC-0022 built, released
(0.8.0/0.8.1), verified on a real box, and documented. Companion to [backlog.md](backlog.md) and
[prod-readiness.md](prod-readiness.md).

## 0. RFC-0029 first - the backfill does not finish

**Ahead of everything below, because it is the only item that decides whether a backfill completes at
all.** [RFC-0029](rfcs/0029-the-fastest-indexer.md) §2: a documented, default-path backfill on Alchemy
aborts.

The cause is a classification regression, not a performance problem. `classify_status` enumerates 401,
403, 413 and 429 and falls through to `Transient`; **Alchemy returns its oversized-range refusal as HTTP
400**. RFC-0028 was grounded on a measured HTTP *200* carrying a JSON-RPC error, so the 400 shape walks
past every mechanism built for it: the marker list becomes unreachable, the provider's suggested range is
discarded into a truncated log line, and the window is retried five times *at the same width* before the
backfill dies.

RFC-0028's speculative split cannot save it either, and the RFC is honest about why: the split is
deliberately non-recursive because "a genuine size failure re-triggers the classified path on the halves
anyway" - and here the halves are misclassified too. The safety net was designed against the right
failure mode and defeated by the wrong classification.

**The durable rule, which matters more than the fix:** RFC-0028 said a marker list is a liability; this
extends it to status codes. `Transient` is the *absence* of a classification rather than a positive
finding, so it must never outrank direct textual evidence of a cap. Adding 400 to the list is belt to
that braces - on its own it would be the same mistake with a different number.

Sequencing follows the RFC's own slices: **the classifier fix (slice 1) is this sprint's first task**,
then the honest harness (slice 2) *before* any optimisation, because §4b shows the current bench measures
a strawman. Slices 3-5 are performance and can follow the items below if the window is short.

## Read this before planning around it

**Three of the four scope items are gated on something other than effort.** That is not an accident of
scheduling - it is what the board actually looks like now that the laptop-buildable work is finished.
Writing it down honestly beats a sprint that reads as a work queue and then stalls in week one.

| Item | Gate | Can we start today? |
|---|---|---|
| RFC-0013 DataFusion | a **benchmark** we must run and honour | Yes - the benchmark is the work |
| RFC-0023 tier 3 | an **archive RPC endpoint** | Partly - design and config, not verification |
| RFC-0003 / 0014 extraction | a **colocated reth node**, deferred by decision 2026-07-29 | No |
| GraphOps | a **conversation**, not a dependency | Yes - and it is the cheapest item here |

So the honest ordering is: **RFC-0029 slice 1 first** (§0 - it is a live defect, not an improvement),
**talk to GraphOps** in parallel since it costs nothing and may reorder everything, **run the DataFusion
benchmark** as the next unblocked build, and treat the remaining two as staged-and-waiting rather than
in-flight.

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

**Needs an archive endpoint**, so the verification half cannot start. What *can*: the nest-level
declaration of irreducible reads, the config surface, and the content-addressing scheme for
`(chain, block, contract, calldata)`.

Do that much only if it is genuinely useful on its own. RFC-0014 slice 0 was worth building ahead of
its source because the decode carried most of the correctness risk; **check whether the same is true
here before assuming it is.** If the risk lives in the verification rather than the declaration, this
waits.

Its acceptance test is already written: the same declared call at the same block re-executes to a
byte-identical result across runs and machines.

## 4. RFC-0003 / 0014 - extraction

**Blocked on the reth box, deferred by decision on 2026-07-29. Do not re-raise it as a blocker** - the
deferral is recorded in [backlog.md](backlog.md) Track 1.

Kept in scope so the sprint states what *would* happen if that decision changed: a full node unblocks
0003 (ExEx tip mode) and an honest tip-lag number; archive unblocks 0014's extraction, whose decode,
schemas and volume guard already shipped in slice 0. The keyspace collision recorded in RFC-0014's
slice-0 note has to be solved before extraction wires up.

---

## Carried over, not scope

Small, and worth doing when they block someone rather than on a schedule:

- **Multi-machine verification** (`fleet-lab.sh up multi`, then `partition` and `skew`). Not listed
  above because it is an hour and a few euros, not a sprint item - but it is the last unverified claim
  in prod-readiness §11, so do it before telling anyone RFC-0022 is proven across machines.
- **RFC-0022 slice 3b** - move the view rebuilds onto the `HotStore` trait so an FE stops opening a
  local redb. That is what lets the compose FE mount go back to `:ro`, and the comment in
  `docker-compose.scaled.yml` is the reminder.
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
- **A skip is not a pass**, and a mutation that does not mutate is not a test. Both bit this week.
- **Keep verification.md's "what we have verified" table honest.** Moving a row to ✅ without evidence
  recreates the problem the document exists to solve.
