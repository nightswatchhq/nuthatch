# Sprint: halcyon-hoopoe

**The first sprint after the freeze.** Chief lifted the 2026 feature freeze on 2026-09-08, in full,
and chose the work: **RFC-0044 through RFC-0048, built in full.** `CLAUDE.md`'s build-order status
is the record; the carve-out mechanism is retired with the freeze it governed.

**This sprint is not that programme.** The programme is twenty-one implementation issues across five
tracking issues (#1205, #1206, #1207, #1208, #1209) and will take several sprints. A sprint whose
done condition cannot be reached in a sprint is a wish. This one takes the tranche that is genuinely
independent, plus the one open defect the board was already carrying.

**A newly opened issue joins this sprint**, as in `frugal-finch`. Whoever files one labels it
`halcyon-hoopoe`.

**Retiring `guarded-goshawk`.** It was scoped earlier the same day, around #1204 alone, and never
started. Its theme is preserved below rather than lost, because the observation behind it still
stands.

## Definition of done

Every issue carrying the **`halcyon-hoopoe`** label is closed, and no open PR is for one of them. At
filing: **#1204, #1210, #1211, #1217, #1218, #1221, #1223, #1224, #1225**. #1204 closed on 2026-09-08
in #1232, and **#1234 joined the same day** under the sprint's own rule. The label, not this list, is
the record of scope.

## The theme

**Specify what is already true, before building on top of it.**

That is not a slogan chosen for the sprint; it is what the four unfrozen design RFCs each turn out to
be doing. RFC-0045 found the offchain pattern already shipped twice, in `src/lists.rs` and the IPFS
side table. RFC-0046 found a counter already answering `402 TAP-Receipt header required` and a
working x402 buyer and seller sitting in the lodestar tree. RFC-0047 says outright that "point another
engine at the same directory" is true by accident rather than by contract. Not one of the five
proposes a new mechanism; each proposes a contract for behaviour that already happens.

#1204 is the same shape from the other end. The runtime root's `/ready` already has correct per-nest
verdicts available to it and does not consult them, and `docs/operators.md` already promises the
behaviour the code does not have.

## The spine

### 1. RFC-0047, the lakehouse commitments - #1221, #1223, #1224, #1225, #1234

**First because it is the only item in the programme whose cost grows while we do not do it.**
Segments are immutable. Row-group sizing, column order, sort, statistics, bloom filters and
compression are fixed at seal and cannot be improved for data already sealed, so every month the
writer runs on unaudited defaults is a month of segments carrying those defaults permanently.

**The internal order changed on 2026-09-08, and the reason is worth stating.** `Segment.hash` is
`sha256` of the Parquet file bytes (`src/seal.rs:218`) and `write_parquet` (`src/seal.rs:402`)
hardcodes SNAPPY, so changing the codec changes the content address **for identical rows**. The claim
that two operators indexing the same chain produce identical segments then holds only within one
writer configuration - and that is a metadata problem rather than a release-notes one. So the writer
change is split into **#1234 and blocked on #1223**: the manifest must name the profile before the
profile changes. The precedent is already in the struct, where `registry_snapshot` records which
factory-discovered set produced a segment for exactly this reason.

That makes the order **#1223, then #1234**, with the rest independent. **#1224 is not blocked** and is
still the right thing to pick up first: it is the audit half, writing down the settings as they
actually are and running the one-time footer audit on a real nest by the #889 method. Any deviation
between the spec table and the footer is a writer-config bug and gets its own issue.

The remainder is documentation and config over walls that already exist: the normative 256-bit
contract and the "Reading Nuthatch segments without Nuthatch" page (#1221), and operator-visible
DuckDB resource governance (#1225).

**The release shape is decided** (recorded on #1224): a minor version, not a major. Nothing already
sealed changes, no configuration breaks, no operator action is required and there is no migration to
run - RFC-0035's precedent for a major was breaking the operator surface, and this breaks nothing an
operator does.

**#1222 is deliberately not in this sprint.** Changing the physical Parquet type of 256-bit values is
a segment-format version. It wants a decision of its own, taken slowly, not a sprint slot.

### 2. RFC-0044 S1 and S2 - #1210, #1211

The cheapest acquisition path we have, and it depends on nothing in flight: RFC-0038's importer and
RFC-0041's entities both shipped. S1 classifies a subgraph's schema and mappings into §5a's four
classes with a source citation each and emits the report; S2 emits the nest on top of
`--from-subgraph` rather than reimplementing it.

**Acceptance is the part to hold.** S1 runs against horizon, livepeer and lodestar, where the answer
is already known from three hand ports. A field the tool classifies differently from the hand port is
investigated, never averaged away.

S3 (the acceptance port on a subgraph somebody actually runs) is the slice that decides whether the
skill is real, and it belongs in the next sprint with a target named, not squeezed into this one.

### 3. RFC-0046 S0 and S1 - #1217, #1218

S0 is a gate rather than a feature, and it comes before any payment code exists: with payment absent
the binary's behaviour is byte-identical to today, and no payment code is reachable without explicit
configuration. **The RFC states the failure mode plainly - if that test cannot be written, the design
is wrong** - so this slice is allowed to come back with bad news, and that is worth finding out in
week one rather than after the counter is built.

S1 is the pure verifier, ported from the tested TypeScript in `lodestar/src/lib/x402-seller.ts`.
§7's testing discipline is not optional here: verify against an *independent* EIP-712 implementation,
recompute the typehash in a test, assert every field against **our** configuration rather than the
payment's own claims. A bad digest construction recovers to *some* address rather than erroring, so it
presents as a forged payment rather than as our bug.

### 4. #1204 - the runtime root `/ready` reports quarantine only

**Closed 2026-09-08 in #1232**, before the sprint started; kept here because the reasoning below is
what the rest of the sprint inherits. Carried over from `guarded-goshawk`, and the only defect on the
board when this sprint was scoped.

`roost_ready` (`src/serve.rs:520`) answers from the quarantine set alone, so `wedged`, `poll_stalled`,
`initial_poll_failed`, `entities_stalled` and `tip_seal_stalled` are computed correctly per nest and
never reach the root. That is how the `horizon` nest behind `nuthatch-ds-upstream` sat 753,000 blocks
behind on its seal while reporting ready, on the surface that issues TAP receipts.

**It opens with Chief's decision, not with code**, because it is about blast radius: aggregate
per-nest readiness at the root, or stop promising it at `docs/operators.md:895`. Whichever is chosen,
the other artefact moves in the same PR.

If aggregation is chosen, the implementation fork wants settling before any code. The stall verdicts
are computed inside the async per-nest `ready()` handler including store reads; `RuntimeHealth` holds
cheap counters and none of those verdicts. So either the root fans out and pays N store reads on a
surface a supervisor hits every few seconds, or the nests publish their verdicts into `RuntimeHealth`
and the cost stays flat in the number of nests. The second is almost certainly right and is wider than
the issue's one-line summary suggests.

**Reproduce the field case, not only the unit.** A hand-built `AppState` proves the handler and never
the wiring; drive it through the real composition or the test passes on a runtime nobody runs.

## The theme guarded-goshawk was carrying, kept because it still stands

Four health instruments failed in seven days and every one of them read as healthy: #1163 reported
`sealed_through 0` after a completed backfill, #1169's counter ran tens of millions of blocks ahead of
what the store could back, #1190 said 300 s while the cursor polled every 2 s, and #1199 could not
tell a dead seal from a working one. The data was correct in all four. What was wrong each time was
the answer to "is this thing working", which is the only question an unattended box is ever asked.

## Explicitly not in this sprint

- **RFC-0045 and RFC-0048 entirely.** 0045's stage 1 could start, but it is the programme's least
  time-critical item and this sprint is already three workstreams wide. 0048 is blocked on RFC-0046
  slices 0-3 by its own §5 and cannot start.
- **RFC-0046 S2 and S3.** The counter and the back office wait on S0's verdict. Building the counter
  before the boundary test exists is exactly the order the RFC forbids.
- **#1222**, the segment-format version. See above.
- **#1213**, RFC-0044 S4, which is gated on §12.2 and stays gated.
- **The frozen register.** `docs/frozen-for-2027.md` is unchanged by the unfreeze and reopens one
  item at a time on its own rule. The RFCs Chief deferred on 2026-09-08 - 0003, 0023, 0031, 0033,
  0034, 0036 - stay deferred.

## How this sprint runs

**A test that passes proves nothing until it has been made to fail.** Every gate gets mutated and the
red run quoted in the PR. This applies with particular force to #1217, whose whole purpose is to fail
if the design is wrong.

**The non-negotiables the RFCs argued past are not waived by the unfreeze.** Each of the five leads
with the rule it appears to break and shows why it does not. Those arguments are now load-bearing on
live work rather than on a draft. RFC-0046 §1's test is the one to keep in view: *delete every payment
feature from the tree and a self-hoster loses nothing.*

**Specify before you extend.** Where a slice finds behaviour that is true by accident, the finding is
the work, and it gets an issue rather than a closing sentence.
