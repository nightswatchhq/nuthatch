# Sprint: punctilious-pipit

**Finish the fold runtime with head snapshots, then port the first folds whose failure costs an indexer
money.**

## Definition of done

Every issue carrying the **`punctilious-pipit`** label is closed, and no open PR is for one of them. The
label, not this document, is the record of scope.

## Order

1. **#1512, put the fold port under version control.** The nine network folds, their generator and the
   S0/S1 harness exist only in `~/fold-port` on the ThinkPad. Step 3 builds on them, so they go into
   `graph-network-nest` before anything else does.
2. **#1513, RFC-0059 S3: head snapshots.** Coalesced evaluation, retention for hash-pinned reads and
   touched-key deltas. This completes RFC-0060 S2. Head evaluation already sits at 280.0 MiB against a
   300 MiB ceiling, so the snapshot bound is measured, not assumed.
3. **#1514, RFC-0060 S3 step 1: allocations, the saved clock and epochs.** They decide eligibility and
   the agent's lifecycle, so they are first in the payment-risk order. Each lands with its differential.
4. **#1516, RFC-0060 S3 step 2: escrow accounts, signers and escrow transactions.** What decides whether
   a TAP receipt is backed and a RAV worth redeeming. Step 1 and step 2 together must still hold the
   head gate.

## Rules

- **The head targets are a gate.** 500 ms p99 and 300 MiB peak, measured inside the running process.
  If S3 cannot hold them, that is reported, not argued around.
- **The deletion test binds every change behind `folds` and `graph`.**
- Fields outside RFC-0060 §2's closure are not built.

## Not in this sprint

- RFC-0059 S4 (serving), the rest of RFC-0060 S3 (deployments onward) and RFC-0060 S4 onwards. S3's differentials evaluate folds directly and do not
  need serving.
- The parked items: #1446, #1324, #1313, #1288, #1280, #1263.
- `docs/frozen-for-2027.md` and its reopening rule.
