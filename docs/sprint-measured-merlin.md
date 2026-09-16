# Sprint: measured-merlin

**Make nuthatch tell an operator what it is doing and which data it served, then make the gate that
proves it reliable enough to use every day.**

## Definition of done

Every issue carrying the **`measured-merlin`** label is closed, and no open PR is for one of them.
The label, not this document, is the record of scope.

## Order

1. **#1417, the actual priority.** Split `nuthatch_sql_rejections_total` by refusal reason while
   retaining the aggregate. A bad query and an overloaded node are not the same incident. The test
   must show a bind error and a busy refusal increment distinct series.
2. **#1430 and #1416, identity at the boundary.** A solo `dev` nest's `/sql` provenance must name
   the same NID as `/nest`; a running binary must report its release through `/ready` or `/metrics`.
   These are small repairs to the answer an operator or caller can cite.
3. **#1400 and #1401, make the gate fail for a reason.** Replace the finality test's quiet-window
   stop with an observed-finality condition under a deadline. Move the process-wide RSS assertion
   out of the shared test binary or test its classification with injected readings. Each fix must
   demonstrate that the former contention path waits or classifies, rather than flaking.
4. **#1429 and #1396, account for the machine time.** Consolidate integration-test binaries only if
   a cold `cargo test --no-run` is under 10 GB and the expected suite total still passes. Measure the
   CI critical path over clean runs, retain the release gate plainly, and make any PR-path reduction
   without deleting coverage by implication.

## Rules

- Metrics retain their existing aggregate where dashboards depend on it. New labels distinguish
  causes; they do not make historic totals incomparable.
- A version, NID, watermark, or rejection reason is an operator-facing claim. Test the endpoint that
  publishes it, not merely the function which happened to calculate it.
- A timing fix exchanges a false failure for a bounded wait, never for an unbounded sleep. A CI speedup
  names the retained release checks and comes with before-and-after timings.
- The 2 GB per-cursor budget and the existing release, publish, storage, compatibility and performance
  gates remain gates. Moving a check off the common PR path requires an explicit equivalent release
  path, not a hopeful sentence.

## Not in this sprint

- The parked Dune sidecar and recorded Dune run (#1362, #1360), warehouse acceptance runs (#1263),
  and cross-nest SQL (#1324). Each needs an explicit unpark decision.
- RFC-0057 (#1381), offchain work (#1206, #1216), the physical 256-bit format decision (#1222),
  `seal-direct` gap recovery (#1410), and the board-only crates.io item (#1299).
- `docs/frozen-for-2027.md` and its reopening rule.
