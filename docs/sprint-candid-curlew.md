# Sprint: candid-curlew

Filed after bashful-bittern. **Seven issues.** A sprint is a labelled set. It has no calendar.

RFC-0041 slice 2, decomposed. This is the first sprint since nocturnal-nightjar that builds a
capability rather than repairing one, and it is a carve-out from the 2026 freeze — one of exactly
two, recorded in CLAUDE.md's build-order note.

## Definition of done

Every issue carrying the **`candid-curlew`** label is closed, and no open PR is for one of them.
That is seven: #864, #865, #866, #867, #835, #837, #839. #821 is the tracker and closes with them.
Work discovered in flight is filed **unlabelled**. Pulling it into scope needs a board reply.

## The theme

**Evidence that can fail.**

Slice 0 asked whether authored SQL can become an embedded DBSP circuit and answered yes. The audit of
2026-08-25 found the answer was right and the evidence was not: the captured-Horizon parity run
pre-aggregates in DuckDB before handing rows to the circuit, so the join, the filter and the
aggregate are all provably inert on that corpus, and the tell was in the published table all along -
876 accepted input rows, 876 result rows. A `GROUP BY` that emits as many rows as it consumed has
grouped nothing.

Slice 2 is where that stops being a documentation problem. Feeding entities from the real ingest path
produces raw weighted deltas by construction, so the parity finally has something to bite on. Every
criterion below is written so it can go red - which, on the evidence of slice 0, is the part that
needs saying out loud.

## The four new pieces

`#821` carried thirteen acceptance criteria and five failure injections. That is a slice, not a
ticket: one PR that size is unreviewable, and an unreviewed slice is exactly how slice 0's evidence
got through. The criteria are unchanged, only distributed.

### 1. #864 - one circuit for backfill, tip, reorg and restart

Criteria 1-5. Backfill and tip are batches of `+1` through the same circuit; a reorg feeds removed
facts at `-1` before deletion; a warm restart computes one finalized seed from local Parquet and
replays only the redb hot tail. Randomized apply/retract sequences converge byte-for-byte.

Entity state is **derived and rebuildable, not a new durable cold store**. Writing mutable cold state
into sealed history is forbidden by the standing reorg rule, and v1 deliberately does not introduce
durable snapshots - if a real restart measurement says the seed is too slow, that is a follow-up RFC,
not a thing to invent early.

### 2. #865 - prove entity rebuild makes zero historical RPC calls

Criterion 6, and its own issue because of *how* it must be proved: "a tape miss or RPC counter proves
this rather than an empty mock endpoint accidentally passing."

This is the criterion most likely to pass for the wrong reason, and the repository has the scars to
say so. Both honest instruments already exist: `ReplaySource` holds no `RpcClient` and makes a miss a
loud failure, and `request_count()` must read zero. Use both - the tape proves it *cannot* ask, the
counter proves it *did not*.

### 3. #866 - a catching-up or dead entity is visibly so, never silently current

Criteria 7, 8 and 13. RFC-0041 §5.2 is blunt: *"Serving frozen derived state as healthy is not
graceful degradation; it is a lie with a pleasant HTTP status."*

#846 fixed the analogous defect one layer down three days ago - `/ready` suppressed every stall term
during seal-direct and put nothing in their place, so a pass frozen for ten hours answered
`HTTP 200 {"ready":true}`. A catching-up entity is the same shape: legitimately not current,
indefinitely, and indistinguishable from dead unless something watches it advance. Stamp when the
applied-through block moves and judge on progress, not on elapsed time.

### 4. #867 - accounted against the per-cursor budget, and the bound bites

Criteria 9-12, against the **shared per-chain cursor** budget rather than a fictional per-nest one.
Two slice-0 faults not to inherit: the bound covers one input of two (#838 - fifty thousand indexer
facts admitted at a declared `max_rows` of 1), and the published per-row figure measures a DuckDB
Parquet scan rather than the entity (#837).

§7 is worth quoting on what does not count: *"DBSP can spill" is not a memory measurement.*

## Carried in by board decision, 2026-08-25

#835, #837 and #839 - the slice-zero evidence questions - were accepted as slice-2 work rather than
as a gate in front of it. #835 resolves naturally once entities are fed from ingest. #837 must land
before criterion 9's per-row estimate means anything. #839 is one stale sentence in the evidence doc
that should go when this slice starts.

## Explicitly not in this sprint

- **#822, slice 3.** Serving, and the measured disappearing-scan proof. It follows this.
- **#849, RFC-0042.** Unfrozen but sequenced behind RFC-0041 by construction.
- **#863.** The backfill no-progress guard - a data-path behaviour change that wants its own review.
- **#829 and #830.** Release integrity, still their own pair.

## How this sprint runs

Standing from nocturnal-nightjar, unchanged:

1. **Scope is the board's.** Discovered work is filed unlabelled.
2. **`Reviewed-by:` names the party who read the diff.**
3. **Acceptance is above.**

Also standing: never `git add -A`; `Closes` is one keyword per issue, and never `Closes part of #N`,
which GitHub does not parse.

From assiduous-avocet: **prove the mutation applied before believing it went green.** And from this
sprint's own subject matter, the harder version - **prove the fixture can distinguish a pass from a
failure before quoting what it measured.** A corpus that pre-computes the answer is not a test.
