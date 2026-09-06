# Sprint: frugal-finch

**Every open issue, including the ones opened while it runs.** A sprint is a labelled set, not a
calendar. Filed the night the Alchemy bill arrived, on Chief's instruction that the next sprint cover
the whole open backlog, and after the earlier sprint labels were taken off the open issues (they
named sprints that had closed; a label that names a finished sprint is scenery). Chief's second
instruction, the same morning: **a newly opened issue joins this sprint too.** There is no unlabelled
queue beside it; whoever files an issue while frugal-finch is open labels it `frugal-finch`.

## Definition of done

Every issue carrying the **`frugal-finch`** label is closed, and no open PR is for one of them. At
filing that was #1078, #1147, #1148, #1160, #1165, #1169, #1170 and #1173; #1178 (bound the
seal-direct refetch) joined the same morning, and anything opened while the sprint runs joins on
filing. The label, not this list, is the record of scope.

## The theme

**The Lodestar nests for under $100 a month, and a migration somebody can call finished.**

What one Arbitrum cursor costs was measured on the box on 2026-09-06, over 45 seconds, at tip:

| nest | requests/min | `eth_getBlockByNumber` | `eth_getLogs` | `eth_blockNumber` |
|---|---|---|---|---|
| graph-allocations-nest-next (8107) | 413 | 268 | 61 | 84 |
| graph-gns-nest-next (8113) | 456 | 273 | 92 | 91 |

At Alchemy's published costs (20, 60 and 10 compute units) that is ~9,900 CU a minute, ~430M a
month, roughly **$185 a month per cursor** on pay-as-you-go. The box was running five Arbitrum
cursors and a Monad backfill against one key; the backfill alone was ~57,000 CU a minute (~$990 a
month) for a nest nothing could read. That night: perpl stopped and disabled, the old allocations
nest stopped, the horizon and staking cursors shown redundant to 8107 row for row (their retirement
is a Caddy re-point in `/root/rel340/retire-old-cursors.sh`, and lodestar#91 stops the code
depending on the redirect), and the Alchemy URL removed from the DIPS pool.

What remains on the key is what Lodestar needs: 8107 whole (~9,900 CU/min) and nothing else. The
target is **under $100 a month for every nest Lodestar reads**, and the measurement says that is not
a matter of trimming. Two facts decide the shape of the work:

- **The bill is the cadence, not the data.** Of the last day's 345,600 Arbitrum blocks, **95** carried
  a Graph event (2,151 rows). The code already buys headers only for event-bearing blocks (#765).
  The 268 headers a minute are the poll loop's own: a reorg check per two-second poll, a checkpoint
  hash and a `finalized` probe per committed window. Polling every five minutes instead of every two
  seconds removes roughly 99% of the bill without changing what is indexed.
- **The consumers refresh on crons of 2 to 15 minutes.** Nothing reading these nests needs a
  two-second cursor. Five minutes of staleness is invisible to every panel on the dashboard.

## The spine, in the order it has to run

1. **#1173 - the freshness dial.** Carve-out 5 of the 2026 freeze, recorded in CLAUDE.md by PR 1174
   (this sprint's first merge; until it lands, the standing brief still reads four). Two operator
   flags, neither of them nest identity: `--poll-interval` (RFC-0040 §3 knob 1), how long a
   caught-up cursor waits before asking for the tip again, default the current two seconds; and
   `--finality-only` (knob 2), which caps the cursor at the chain's finality boundary so nothing it
   indexes can reorg and the reorg check is not paid. Both loops, solo and runtime. `/ready` states
   the mode and the interval, and its stall threshold scales with the interval so a five-minute
   cursor is not reported stalled at ninety seconds. Acceptance, stated so that only one
   implementation satisfies it: (a) a nest run under either flag holds exactly the rows a
   tip-following run holds for the same blocks; (b) the sealed segments are byte-identical to the
   ones tip following produces - CLAUDE.md's carve-out gate, unchanged. That is achievable and it is
   why it is the gate: the tip path cuts a segment where the rows say to (`tip_seal_cut`, #1067:
   `SEAL_DIRECT_BATCH` rows, then the block that carried the buffer past the threshold), a function
   of the rows and not of when finality advanced or how wide a window was, so two cursors on
   different schedules already seal identical files. The dial touches no sealing code. The proof is a
   determinism run, not an argument: the same fixture indexed under `--poll-interval 5m`, under
   `--finality-only`, and on the default path, with segment content addresses compared (the shape
   `e2e_seal_determinism` already uses). An earlier draft of this paragraph said boundaries were
   timing-dependent; that was wrong, and the review that caught it is why the gate stands as written.
   **Mutate it**: fake the ceiling
   back to `tip` under `--finality-only` and the test must go red; quote the failure in the PR.
   Knob 3 (timestamp interpolation) is not in scope, and knob 4 is #1170 below.
2. **Release 3.5.0** carrying #1173, the DuckDB memory budget that landed after 3.4.1 (PR 1172) and the
   `serve` role fix (PR 1171). Roll onto 8107, 8113 and 8104 with `--poll-interval 5m`. Then
   measure again - but not the same way. A 45-second sample of a two-second cursor is a fair
   average; a 45-second sample of a five-minute cursor sees zero polls or one depending on where it
   starts, and normalising that to a minute proves nothing. The closing measurement is the
   difference in `nuthatch_rpc_methods_total` per method between two scrapes **at least one hour
   apart** (twelve or more intervals, so phase is noise), divided by the elapsed minutes, and
   repeated over a full day before the number is written down. The sprint's number is that table.
3. **#1165 - the 3.4.0 segfaults.** 3.4.1 has held on 8107 since the evening of 2026-09-05 with the
   concurrency permit at one. Close when 3.5.0 has run a day on 8107 and the replay soak
   (`replay-soak.py`, the 44 captured statements, concurrency four) completes without a crash under
   the memory budget. A day without a crash under a single permit is not that evidence.
4. **#1160 and #1078 - Lodestar without the key.** Every `NUTHATCH_*` flag has been on in production
   since 20:38 UTC on 2026-09-05 with parity exact on fifteen routes. The final PR removes every
   `subgraphQuery` fallback and the three gateway-only scripts, then `GRAPH_API_KEY` leaves Vercel.
   Both issues close on the same evidence: the dashboard serving with the variable absent, not
   merely unused.

## What runs in parallel, and does not wait for the spine

5. **#1170 - after an hour of 429s the window had collapsed to ten blocks and stayed there.** That
   is the observed fault, not the requirement. Required: a refusal may lower the window (RFC-0040 §3
   knob 4 - slow the cursor rather than retry the batch harder), and once refusals stop the ceiling
   **must** widen again on its own, so a rate-limited hour costs an hour and not the rest of the
   backfill. An implementation that leaves the window at ten fails this item. This is the fix that
   makes public endpoints usable for a backfill, which is what 7 needs.
6. **#1169 - `seal_direct_completed` runs tens of millions of blocks ahead of the durable watermark.**
   A restart resumes from the watermark, so the counter is a claim the store cannot back. Make the
   counter follow durability or name it for what it is.
7. **#1178 - bound how far the seal-direct fetch runs ahead of the durable watermark.** Filed from
   #1169's fix, which made the gap visible on `/ready` and deliberately did not bound it: cutting a
   segment on block span as well as row count moves segment boundaries on sparse ranges, and a
   boundary is part of a segment's identity. Weigh the three options in the issue and decide; the
   47.6M-block redo on the gns nest is the cost of not deciding.
8. **#1147 and #1148 - Monad in the field, and the Perpl nest.** The nest exists and is 26% through
   its backfill (seal_direct_completed 67,004,198 of 102,151,917). It does **not** restart on the
   Alchemy key. After #1170 lands it restarts on the three public Monad endpoints its config already
   lists, with a poll interval, and either finishes the backfill for nothing or produces the evidence
   that public Monad endpoints cannot sustain it - in which case both issues are parked with that
   measurement attached, and the park is the close.

## The call

**Everything stays, and everything new joins.** The instruction was the whole backlog, and each item
is either the cost work itself, the evidence the cost work needs, or the migration's last mile.

**The number that closes the sprint is measured, not projected.** Step 2's table - counter deltas
over at least an hour, then a day, never a 45-second sample of a five-minute cursor - with the
Lodestar nests under 1,000 CU a minute between them. At Alchemy's rates that is under $20 a month;
the $100 target has headroom for a second key or a worse month.

**Perpl does not cost money again.** If public Monad endpoints cannot carry it, it parks.

## Explicitly not in this sprint

- Every `frozen` issue. The fifth carve-out (PR 1174) is the one this sprint spends; there is no sixth.
- Hosting the Arbitrum nests behind one cursor (RFC-0021 mounts). Shipped capability and a real
  saving in RAM and public-endpoint pressure, but an operations job with its own issue if wanted;
  it does not move the Alchemy bill once the cadence is fixed.
- A prefer-free, fall-back-to-paid order in `select_rpcs`. The pool round-robins evenly by design;
  an ordering is new capability and would need its own carve-out.
- RFC-0040 §3 knob 3, timestamp interpolation. It changes sealed content.
- Nothing found while doing this work is out of the sprint: a new finding is filed as an issue and
  labelled `frugal-finch` on filing.

## How this sprint runs

**A test that passes proves nothing until it has been made to fail.** Every gate in this sprint
gets mutated and the red run quoted in the PR.

**Measure where the bill is, not where the theory says it is.** RFC-0040 argued for finality-only
mode as "the big one". The measurement says the poll interval is; finality-only is worth having for
what it removes from the reorg surface, not for the money. The RFC's ordering was wrong and the
number said so before a line was written.

**A nest nobody reads is a bill, not an asset.** Perpl ran nine days for nobody. `/ready` and Caddy
between them can always answer "who reads this"; ask before the invoice does.

**Anything worth remembering has an open issue.** A closing sentence is not a queue.
