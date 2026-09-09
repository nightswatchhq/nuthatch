# Sprint: inessential-ibis

**The second sprint after the freeze**, and the first scoped from a clear board: `halcyon-hoopoe`
closed with every labelled issue merged and no open pull request behind it. The programme is
unchanged - **RFC-0044 through RFC-0048, in full** - and this sprint takes the tranche that became
workable when the last one landed.

Two of the three items were named in `halcyon-hoopoe`'s own "explicitly not in this sprint" list, and
both for reasons that have since expired rather than for reasons that were wrong. That is recorded
here so the change reads as a consequence and not as a reversal.

## Definition of done

Every issue carrying the **`inessential-ibis`** label is closed, and no open PR is for one of them.
At filing: **#1214, #1215, #1219**. The label, not this list, is the record of scope. A newly opened
issue joins this sprint under the same rule `frugal-finch` and `halcyon-hoopoe` used: whoever files
one labels it.

## The theme

**Every item this sprint is a capability the product must still work entirely without.**

That is not a slogan reached for after the fact. It is the acceptance criterion each of the three
issues already carries, written by three different RFCs that were not coordinating:

- **#1214** swaps a ported subgraph's emit target from `views/*.sql` to `entities/*.sql`. RFC-0044 §6
  is explicit that a nest ported to views is *correct* and merely slower, so the swap must not become
  a dependency: a nest with no entities is still a nest.
- **#1215** adds an offchain namespace, and RFC-0045's acceptance ends with the clause that matters:
  *the nest still reindexes from chain with the offchain namespace absent.* The RFC calls that the
  determinism boundary and says it is not negotiable.
- **#1219** adds a paid mount option, and RFC-0046 §1's test is a build gate rather than a paragraph:
  *delete every payment feature from the tree and a self-hoster loses nothing.* #1217 exists to fail
  if it ever stops being true, and it is already on main.

So the sprint has one shared way of being wrong, and it is worth naming in advance: **a feature that
quietly becomes load-bearing.** Each item ships with the absence case exercised, not asserted.

The trap that makes this harder than it sounds is one this repository has already paid for twice.
Four tests and three RFC criteria once passed with the mechanism removed entirely, and two separate
CI gates once passed by matching the explanatory comment above the thing they were meant to guard. An
absence test that cannot fail is indistinguishable from the feature being absent-able. Every one of
these three gets mutated, and the red run gets quoted in its PR.

## The spine

### 1. #1214 - RFC-0044 S5, emit `entities/*.sql`

**First because it is the smallest thing standing between a ported subgraph and the reason anyone
would want one.** RFC-0044 §9 calls it "one file" and §6 explains why: everything difficult about the
port - manifest handling, the mapping read, the classification, the call derivation - is indifferent
to how entities are materialised, and the emit target was deliberately isolated in one place so this
slice would be a swap rather than a rewrite.

Its precondition is met. RFC-0041 shipped on 2026-08-28 and #820 and #822 are both closed, so the
target exists. S1 and S2 landed on 2026-09-09 in #1242 and #1244, so there is something to swap.

The absence case: a nest whose fields all land in views still emits, still runs, and the report still
says truthfully which of the two each field went to.

### 2. #1215 - RFC-0045 stage 1, the file drop

`halcyon-hoopoe` left this out on width, not merit: *"0045's stage 1 could start, but it is the
programme's least time-critical item and this sprint is already three workstreams wide."* The width
argument is spent, and the merit argument was never made against it.

A local CSV, Parquet or JSON file is validated, its schema inferred or asserted, converted to a
sealed Parquet segment in an offchain namespace, and recorded in a generalised provenance manifest
carrying content hash, source path, ingest timestamp and tool version. It is queried through the
federation that already exists. **No new always-on machinery, no endpoint, no daemon.**

The acceptance is the RFC's and both halves are gates. An operator joins a dropped file against chain
data in one query with provenance visible in the result; **and** the nest reindexes from chain with
the offchain namespace absent. The second clause is the one to build the test around first, because
it is the one that fails silently if the namespace ever becomes a dependency of the chain path.

### 3. #1219 - RFC-0046 S2, the counter

**Its `blocked` label is stale and this sprint removes it.** The label was right when it was applied:
`halcyon-hoopoe` recorded that "the counter and the back office wait on S0's verdict. Building the
counter before the boundary test exists is exactly the order the RFC forbids." S0 is #1217 and S1 is
#1218; both closed on 2026-09-09, and #1217's boundary test is on main. The order the RFC forbids is
no longer the order we are in.

A mount option beside RFC-0034's bounded surface: price, recipient, network. A paid mount called
without payment answers `402` with a challenge; verified, it serves. It **records the authorisation
and nothing else** - no settlement in the query path, and no third party in it either, per §5.1.

This is the item to be most careful with, and CLAUDE.md's build-order status says why in more detail
than this document should repeat. The short form: a price that cannot be turned off, a settlement
path the binary requires to start, or a key we hold has crossed from an operator's choice into a
gated product and violates non-negotiable 3. **That line is held by #1217, which is already merged**,
so the check is mechanical rather than a matter of judgement: S2 must not turn it red.

## Explicitly not in this sprint

- **#1212, RFC-0044 S3, the acceptance port.** Not deferred on merit - it is the slice that decides
  whether the skill is real, and it is the most valuable open issue we have. It is out because it
  cannot be finished without spending money: §10 requires a real subgraph ported to tip and diffed
  against the gateway field by field, which means an RPC bill and a sync of unknown length on a
  subgraph nobody has picked yet. **That is a decision for Chief, not a sprint slot I can award
  myself**, and it wants its own sprint once he has named the subgraph and the budget.
- **#1222, the segment-format version.** `halcyon-hoopoe` put it best and the reasoning has not
  moved: changing the physical Parquet type of 256-bit values "wants a decision of its own, taken
  slowly, not a sprint slot". RFC-0047 §2 lists what it actually costs - a `manifest_version` bump, a
  dual-read, a migration note, and every external reader plus `read_table_rows` accepting both. Worth
  adding that the RFC's supporting evidence is partly *"Amp converged there. We did not."*, and a
  competitor's architecture is not a benchmark.
- **#1213, RFC-0044 S4, the query table.** Gated on §12.2, which is an open question about how
  faithful the query table should be, and the RFC only records a lean rather than an answer. It stays
  gated until the gate is resolved, and resolving it is not this sprint's work.
- **#1220 and #1216**, genuinely still blocked, on S2 and on stage 1 respectively. If #1219 and
  #1215 land early they become candidates for the next sprint, not for this one.
- **RFC-0048 entirely - #1226 through #1230.** Blocked on RFC-0046 slices 0 to 3 by its own §5, and
  S3 is not in this sprint.
- **The frozen register.** `docs/frozen-for-2027.md` is unchanged and reopens one item at a time on
  its own rule. The RFCs Chief deferred on 2026-09-08 - 0003, 0023, 0031, 0033, 0034, 0036 - stay
  deferred. RFC-0042 stays parked to 2027-09-01 or a §14 trigger.

## The one piece of maintenance carried alongside

`docs/progress-log.md`'s newest entry is dated 2026-08-20 and covers up to 2026-08-19. Everything
since is unrecorded: RFC-0041 shipping, RFC-0042 closing at KEEP DuckDB, Monad, Robinhood Chain,
RFC-0040's freshness dial, RFC-0047's four commitments, x402 S0 and S1, and RFC-0044 S1 and S2.

That is the third time this log has gone quiet, and the file already carries the lesson from the
second: *"A quiet log reads as no progress to anyone who has not read `git log`."* It gets one honest
catch-up entry in the shape the two existing ones use, rather than nineteen back-filled per-push
entries at a fidelity the house style does not support. It is not a build-order item and it does not
carry the sprint label; it is recorded here so it is not mistaken for scope creep when it appears.

## How this sprint runs

**A test that passes proves nothing until it has been made to fail.** Carried forward from
`halcyon-hoopoe` unchanged, and it binds hardest here, because all three items are absence cases and
an absence case is the easiest kind of test to write inert.

**A finding is the work.** Where a slice finds behaviour that is true by accident, that gets an issue
rather than a closing sentence.

**The non-negotiables the RFCs argued past are not waived.** Each of the five leads with the rule it
appears to break and shows why it does not. None of those arguments is waived by the freeze ending,
and two of them are load-bearing on this sprint specifically: RFC-0045's determinism boundary and
RFC-0046 §1's deletion test.
