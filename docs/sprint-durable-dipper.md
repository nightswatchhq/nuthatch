# Sprint: durable-dipper

**Make external and incomplete data visible, bounded, and versioned before it becomes a quiet
property of a sealed answer.**

This sprint began when `measured-merlin` closed on 2026-09-23.

**Closed 2026-09-23.** Every issue carrying `durable-dipper` is closed and no open PR is for one of
them. #1410 records what seal-direct gave up on; #1222 was decided by RFC-0061, accepted by Chief, with
no segment-format change; #1216 closed on #1448 and #1453, with the offchain entity checked against
the DuckDB oracle.

## Definition of done

Every issue carrying the **`durable-dipper`** label is closed, and no open PR is for one of them.
The label, not this document, is the record of scope.

## Order

1. **#1410, name a seal-direct gap.** When a document reaches the five-minute give-up limit, record
   its identity under the resolver's existing `ipfs_gave_up:` evidence. The operator must be able to
   list what a sealed range omitted. This is observation, not a covert recovery scheme: no late
   segment, retained hot range, or altered cut rule without an accepted RFC.
2. **#1222, decide the 256-bit segment boundary.** Write and accept the RFC decision for the next
   physical Parquet representation, the format version, old-segment reads, and what an external
   reader sees across the boundary. No migration is smuggled in as a type tidy-up.
3. **#1216, one out-of-band price pull.** Build the RFC-0045 stage-two connector only once its
   namespace and provenance rules are kept: it runs behind the cursor, remains optional, records
   freshness, and degrades to stale-and-labelled on an outage rather than returning a guessed value.
   Its incremental entity must agree with the DuckDB oracle.

## Rules

- A missing document, stale price, and changed segment encoding are different states. Each is made
  observable at the boundary where a caller could otherwise mistake it for complete current data.
- Nothing external is fetched inline while indexing. With no connector configuration, a nest makes no
  new network call and remains reindexable from chain data alone.
- A format change names the reader and migration behaviour before any writer emits its first new
  segment. Existing sealed bytes do not change.
- The deliverable for a real-account or provider claim is a recorded run. Credentials remain with the
  operator and never enter the repository, logs, or issue text.

## Not in this sprint

- Dune-assisted ingestion (#1381), the Dune row-insert sidecar and recorded Dune run (#1362, #1360),
  warehouse recipes (#1263), and cross-nest SQL (#1324) remain parked.
- The accumulator, block-string, and `tx_from` RFC gaps (#1313, #1288, #1280) remain parked.
- Crates.io (#1299) is board-only.
- `measured-merlin`'s #1396 remains its own release-gate accounting work until its before-and-after
  runs close it.
