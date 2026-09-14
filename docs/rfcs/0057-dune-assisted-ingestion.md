# RFC-0057: Dune-assisted ingestion - can Dune replace or reduce RPC for Nuthatch?

**Status:** **Draft** - research RFC: questions, hypotheses, experiments and decision gates, not an
implementation spec. Tracking #1381.

**Date:** 2026-09-14

**Author:** Pete (Nixum Ltd / Night's Watch)

| Field | Value |
|---|---|
| Type | Research RFC (questions, hypotheses, experiments, decision gates - not an implementation spec) |
| Status | Draft |
| Author | Pete (Nixum Ltd / Night's Watch) |
| Created | 2026-09-14 |
| Last updated | 2026-09-14 |
| Scope | Nuthatch ingestion economics; Night's Watch Dune plan |
| Related | NW-RFC-001 (Dune as Reference & Feed Layer, a Night's Watch document outside this repo), Nuthatch RFC-0041 / RFC-0042 |
| Budget | Dune Analyst plan (4,000 credits/mo); Alchemy (current monthly spend: `[fill]`) |
| Decision deadline | End of Phase 3 (§9), target 2026-11-30 |

---

## 1. Problem statement

Running nests costs real money in Alchemy RPC. The Dune plan is a flat $75/mo that I am paying anyway. The intuition is obvious: Dune already has every block, transaction, log and trace for the chains I care about, so why fetch them again from a metered RPC?

This RFC exists to turn that intuition into numbers and a decision. It asks three questions:

- **Q1 (replace):** Can any part of nest ingestion be sourced from Dune *instead of* RPC without breaking Nuthatch's trust model?
- **Q2 (reduce):** Can Dune act as a *hint* that lets Nuthatch make dramatically fewer RPC calls while every stored byte still comes from RPC?
- **Q3 (compare):** Is Dune even the right lever, or is the cheaper fix a different RPC strategy (own archive node, different provider, shared raw ingestion across nests)?

A "nest" in this document means one Nuthatch deployment indexing one chain for one purpose (full-chain, or scoped to a set of contracts). Correct the definition if Nuthatch's own vocabulary differs.

## 2. Prior conclusion (from the NW-RFC-001 discussion, to be tested here)

The first-pass answer was: **not a replacement, possibly a reducer.**

- The binding constraint is Dune's export meter, not compute. On Analyst, exporting results via API or CSV costs **10 credits per MB**. 4,000 credits ⇒ **≤ 400 MB/month** leaves Dune, before any compute is paid for. Compute itself is now usage-based (charged in proportion to resources consumed), so small narrow queries are cheap and large scans are not.
- Full-chain raw data is terabytes per chain. Dune cannot be a backfill *source* for a full-chain nest on this plan, by roughly four orders of magnitude.
- Dune has no state, no proofs, no reorg signals, no full headers, and lags the head by minutes. Anything touching those must stay on RPC.
- The promising hybrid is the **sparse block index**: ask Dune *which* blocks matter for a contract-scoped nest (a list of block numbers, kilobytes), then fetch only those from RPC.

Everything below is designed to confirm, refute, or quantify those statements.

## 3. Hypotheses

| # | Hypothesis | Falsified if |
|---|---|---|
| H1 | Full-chain backfill from Dune is infeasible on Analyst: required export volume exceeds monthly allowance by ≥ 100× for every chain Nuthatch supports. | Any supported chain's raw logs+txs for the nest's required range fit in < 40 GB (100× the monthly allowance). |
| H2 | A contract-scoped seed backfill (all logs for a fixed contract set) fits in one month's allowance for at least one real nest (candidate: The Graph protocol contracts on Arbitrum One). | Estimated export size > 400 MB for every candidate nest. |
| H3 | The sparse block index cuts backfill Alchemy CUs for a contract-scoped nest whose tables are all log-derived by ≥ 90 % versus range scanning, with Dune cost < 50 credits per nest. CUs, not calls, because S1 may make more calls than S0 while spending far fewer CUs, and CUs are what is billed. | Measured CU reduction < 90 % or Dune cost > 50 credits. |
| H4 | Head-following from Dune is worse than RPC on latency and on cost, and should not be attempted. Reorg visibility is not part of the claim E6 tests: Dune exposes no parent-hash boundary (§2), and a 24 h window on Arbitrum will very likely contain no reorg, in which case both sources report none and nothing has been compared. | E6 finds Dune no worse than the RPC baseline, measured on the same contract set over the same window, on p95 lag behind head or on $ per month. |
| H5 | For full-chain nests, a self-hosted archive node (or a cheaper provider) beats Alchemy on cost per backfilled block. A Dune hybrid is not part of this comparison: for full-chain nests, H1 and G1 decide whether Dune can backfill at all, and H5 is only worth deciding if they say it cannot. | Measured cost/block for self-host ≥ Alchemy cost/block after amortizing hardware over 12 months. |
| H6 | Raw ingestion is (or can be) done once per chain and shared across nests; if it isn't, that duplication dominates the Alchemy bill more than provider pricing does. | Nuthatch already shares raw stores across nests, or duplication accounts for < 20 % of spend. |

## 4. Non-goals

- Changing Nuthatch's trust model. A nest's stored data must remain reproducible from RPC alone. Dune-derived bytes never enter a sealed segment.
- Using Dune's decoded tables in place of Nuthatch's decoder.
- Enterprise-only Dune surfaces (Trino connector, data shares, dbt transformations). Out of budget and out of scope; noted in §11 for completeness.
- Any Nuthatch crate change inside this RFC. If an experiment shows a change is needed (§8), this RFC produces a separate RFC proposing it, not the change.

## 5. Ingestion decomposition

Every nest's ingestion decomposes into these concerns. The table records the *prior* assessment; experiments in §7 fill the *measured* column.

| # | Concern | Needs from source | Dune can supply? | Prior | Measured |
|---|---|---|---|---|---|
| I1 | Block headers | number, hash, parent, timestamp, roots, extra | Partial (`<chain>.blocks`: no full header) | RPC | |
| I2 | Transactions | full tx bodies | Yes, but volume | RPC (full-chain); Dune-seed possible (scoped) | |
| I3 | Receipts / status / gas | per-tx | Partial (fields on `transactions`) | RPC | |
| I4 | Logs | all or filtered | Yes, but volume | RPC (full-chain); Dune-seed possible (scoped) | |
| I5 | Traces | internal calls | Yes for many chains (`<chain>.traces`), volume | RPC only if needed at all | |
| I6 | State (`eth_call`, storage, balances-at-block, code) | archive access | No | RPC, archive | |
| I7 | Head following | sub-minute latency | No (lag + per-poll cost) | RPC / WS | |
| I8 | Reorg detection | parent-hash chain, uncle/reorg events | No | RPC | |
| I9 | Which blocks are relevant (scoped nests) | block numbers with activity for a contract set | **Yes, cheap** | Dune hint | |
| I10 | Range sanity (counts, hashes) | aggregates | **Yes, cheap** | Dune oracle (NW-RFC-001 C) | |
| I11 | Chains Nuthatch supports that Dune may not cover | raw tables for the chain | Depends on Dune coverage per chain | RPC | |

The research question reduces to: for I2/I4 in scoped nests, is Dune-seed ever cheaper than RPC (H2)? And for I9, how much does the hint save (H3)?

## 6. Candidate strategies

### S0 - Baseline (today)
Nest backfills by range-scanning RPC (`eth_getLogs` over windows, `eth_getBlockByNumber`, receipts) and follows the head via RPC/WS. Cost = Alchemy CUs per block × blocks.

### S1 - Sparse block index (Dune as hint)
1. One Dune query per scoped nest: `select distinct block_number from <chain>.logs where contract_address in (...) and block_number between a and b order by 1`.
2. Export the block list (bigint per row ⇒ ~10–20 bytes/row in CSV; 1M relevant blocks ≈ 10–20 MB ≈ 100–200 credits, which is the *worst* plausible case; most scoped nests are far sparser).
3. Nuthatch backfills only those blocks (plus whatever neighbours it needs for continuity), fetching everything from RPC.
4. Safety net: because the hint could be stale or wrong, run the NW-RFC-001 range checksum afterwards, and optionally a coarse RPC `eth_getLogs` over any window where Dune returned zero blocks (cheap when the answer really is zero).

Trust: unchanged. Dune only narrows *where* Nuthatch looks. A missed block in the hint shows up as a checksum mismatch.

Scope: the block list comes from `logs` alone, so S1 applies only to a nest whose tables are all log-derived. A block where a watched contract is called, or changes state, without emitting a log is not in the list. A nest that also decodes calldata or traces needs its relevant blocks from `transactions` or `traces` as well, which E4 and E5 do not measure; S1 is not accepted for that class without an experiment that does.

### S2 - Contract-scoped seed from Dune
Export the full log set (and optionally tx bodies) for the contract set from Dune, load into a *seed* store, then have Nuthatch *re-fetch and verify* each block from RPC before sealing. This is only worth it if verification is cheaper than fetching cold, which is doubtful, since verification still needs the RPC bytes. Kept as a candidate to measure, expected to lose to S1.

### S3 - Activity histogram for adaptive range scanning
Weaker cousin of S1 for cases where a nest's contract set is large or open-ended: Dune returns `count(*) group by block_number / N` so Nuthatch can size `eth_getLogs` windows adaptively (wide windows through dead zones, narrow through hot zones). Reduces failed/oversized range calls rather than total calls.

### S4 - Self-hosted archive node for backfill
Rent a box, sync an archive (or use a snapshot), backfill full-chain nests locally at zero marginal cost, keep Alchemy for head-following only. Not a Dune strategy, but the comparison H5 is required to avoid optimizing the wrong thing.

### S5 - Provider / plan change
Cheaper RPC provider or Alchemy plan restructuring (e.g. batch endpoints, `eth_getBlockReceipts`, free-tier fan-out). Same comment as S4.

### S6 - Shared raw ingestion across nests
If nests currently each pull raw data, refactor so one raw store per chain feeds many nests. Tested via H6 before anything else, because if duplication is the problem the Dune question is moot.

## 7. Experiments

Each experiment lists its Dune credit budget. Total research budget: **600 credits** (reserve line of NW-RFC-001 plus part of workstream C's allocation for the month); Alchemy cost is measured, not spent deliberately.

### E1 - Inventory the Alchemy bill (H6, baseline for everything)
- Pull the last 3 months of Alchemy usage by method and by app/key.
- Attribute CUs to nests; classify as backfill vs head-following vs state calls.
- Output: table of CU share per (nest, method class). If two nests on the same chain both show large backfill CUs on overlapping ranges, H6 is confirmed and S6 jumps the queue.
- Credits: 0.

### E2 - Size the impossible (H1)
- For each supported EVM chain on Dune: `select count(*), approx bytes` for logs and transactions over the range a full-chain nest needs. Use `count(*)` × measured avg row width from a 1,000-row sample export rather than exporting anything large.
- Output: GB required vs 0.4 GB/month allowance. Expect ratios in the 10³–10⁴ range.
- Credits: ~20 (a handful of count queries plus tiny samples).

### E3 - Size the plausible seed (H2)
- Candidate nest: The Graph protocol contracts on Arbitrum One (HorizonStaking, SubgraphService, GNS, Curation, RewardsManager, GraphToken transfers to/from protocol contracts).
- Count logs and txs; export a 1,000-row sample to measure CSV bytes/row; compute total MB and credits.
- Output: MB and credits for full seed; also for "logs only" and "last 12 months only" variants.
- Credits: ~15.

### E4 - Sparse block index savings (H3)
- Same candidate nest. Run the S1 query for the full range; export the block list; record credits (execution + export MB).
- Compute: relevant blocks / total blocks in range.
- Estimate RPC calls under S0 (range scan with Nuthatch's current window size) vs S1 (per-block fetch of relevant blocks + zero-check windows). Convert to Alchemy CUs using the per-method CU costs from E1.
- Output: % reduction in CUs, Dune credits consumed, and the crossover point (what fraction of blocks must be relevant before S1 stops paying).
- Repeat all of the above on a second, less sparse log-derived contract set (e.g. a DEX router), so G3 is not decided on one convenient nest.
- Credits: ≤ 200 per contract set (block list export is the only meaningful cost).

### E5 - Hint correctness (H3 safety)
- Take the E4 block list. Independently, run Nuthatch's existing S0 backfill on a 100k-block sub-range (or use an existing sealed segment) and diff the set of blocks with relevant logs.
- Output: false negatives (blocks Dune missed) and false positives. Any false negative is disqualifying unless caught by the checksum/zero-window net; measure whether it is.
- Credits: 0 (reuses E4 output).

### E6 - Head-following from Dune (H4, expected to fail fast)
- Poll a narrow "logs for contract set in last N minutes" query every 5 minutes for 24 h; measure lag vs RPC head and credits/day. Record any row Dune revises after returning it, as a note for the memo; a revision is not a reorg boundary, and it is not an axis of the comparison.
- RPC baseline, same contract set, same window: the p95 lag of Nuthatch's own RPC head-following (its tip-lag metric) and its monthly cost from E1's head-following CUs × `A_price`. Dune's cost is converted with `D_price` (§10) so both costs are in dollars.
- Output: one row per axis (p95 lag, $/month) with the Dune figure, the RPC figure, and which is worse. This table, not the kill criterion, decides H4.
- Kill criterion: stop the moment projected monthly credits exceed 200 or p95 lag exceeds 60 s. This is a budget cap: an abort ends polling, and the comparison is made on the data gathered up to it. Expect to stop within hours.
- Credits: ≤ 100 (hard cap; abort at 100).

### E7 - Self-host vs Alchemy for full-chain backfill (H5)
- Price an archive box for the heaviest chain (cloud and bare-metal quotes; snapshot availability; sync time).
- From E1, compute Alchemy cost of one full backfill of that chain.
- Output: cost per backfilled block for both, amortized over 12 months, plus the monthly break-even in backfills.
- Credits: 0.

### E8 - Dune coverage of the chains Nuthatch supports (I11)
- For each chain Nuthatch supports, check whether Dune has raw `blocks`, `transactions` and `logs` tables at all, and whether they cover the range a nest on that chain needs.
- Output: per-chain yes/no. S1 applies only where the answer is yes.
- Credits: ~10.

## 8. What would require a Nuthatch change (change check)

S1 needs Nuthatch to accept a block list (or a "relevant ranges" file) as the driver of a backfill instead of a contiguous range. Whether that exists today is the first thing to check:

- [ ] Does Nuthatch ingestion already support a block-list or ranges-driven backfill (CLI flag, config, or internal API)?
- [ ] If not, can it be approximated *outside* Nuthatch by driving many small contiguous backfills (one per run of consecutive relevant blocks) from a wrapper script? Measure the overhead; if acceptable, no binary change is needed.
- [ ] If neither, this RFC's output includes a separate RFC ("ranges-driven backfill"), numbered when it is filed and sized in the same format as RFC-0041/0042, to be decided on its own merits.

S3 (adaptive windows) likely also requires an ingestion change; S2 needs a seed-and-verify path that almost certainly does. Both are parked unless E3/E4 make them compelling.

## 9. Decision gates

| Gate | Condition | Outcome |
|---|---|---|
| G0 (after E1) | Backfill duplication across nests > 20 % of spend | S6 becomes the priority; Dune work continues but is not the headline saving |
| G1 (after E2) | H1 confirmed | Close Q1 for full-chain nests permanently; stop entertaining "backfill from Dune" |
| G2 (after E3) | Seed for the candidate nest < 400 MB | S2 stays on the table for measurement; else S2 closed |
| G3 (after E4+E5) | ≥ 90 % CU reduction, < 50 credits, and no uncaught false negatives, on both E4 contract sets (the Graph candidate and the less sparse second set) | S1 accepted as the standard backfill path for log-derived scoped nests; §8 determines whether it needs a separate RFC |
| G4 (after E6) | Dune is worse than the RPC baseline on both axes of E6's comparison (p95 lag and $/month) | H4 confirmed; head-following stays RPC forever; never revisit on this plan. Otherwise H4 is falsified: head-following still stays RPC, and the axis where Dune was no worse goes into the memo as the only ground for revisiting |
| G5 (after E7) | Self-host cheaper per block at ≥ 1 full backfill/quarter | Recommend an archive box for full-chain nests; Alchemy retained for head-following and state |

Final output of this RFC: a one-page **decision memo** (§13) stating, per nest class (full-chain vs scoped), the recommended source for each concern I1–I11, with the measured numbers.

## 10. Cost model

Parameters (fill from E1 and the Dune pricing page before computing):

```
A_cu(method)         Alchemy CUs per call, per method (E1)
A_price              $ per 1M CUs on the current plan
B                    blocks in the nest's range
w                    eth_getLogs window size (blocks) under S0
r                    fraction of blocks relevant to a scoped nest (E4)
c_exec               Dune credits for the S1 query execution (measured)
m_export             MB exported for the block list (≈ r·B·bytes_per_row / 1e6)
c_export             10 credits per MB on Analyst
D_price              $ per Dune credit on Analyst (plan price / monthly credits), for E6's cost axis
```

S0 backfill cost (scoped nest):
```
CU_S0 ≈ (B / w) · A_cu(getLogs) + r·B · [A_cu(getBlock) + A_cu(getReceipts)]
```
S1 backfill cost:
```
CU_S1 ≈ r·B · [A_cu(getBlock) + A_cu(getReceipts)] + (zero-check windows) · A_cu(getLogs)
credits_S1 = c_exec + 10 · m_export
```
Savings are entirely in the first term of CU_S0, the range scan, so S1 is only interesting when `(B / w) · A_cu(getLogs)` dominates, i.e. sparse contracts on long-lived chains. The crossover fraction `r*` where S1 stops paying is an explicit output of E4.

Full-chain nest (S0 vs S4):
```
$_alchemy_per_block  = Σ A_cu(per-block methods) · A_price / 1e6
$_selfhost_per_block = (box $/mo · 12 + snapshot/egress) / (backfills_per_year · B)
```
Both are dollars per backfilled block, which is the unit H5 and G5 compare.

## 11. Out-of-budget alternatives (for the record)

- **Dune enterprise data access** (Trino connector, data shares to a warehouse, dbt transformations): would change the export economics entirely, but is enterprise-priced and not something a $75 plan unlocks. If Night's Watch ever has a client paying for full-chain analytics, revisit.
- **Dune Sim / real-time wallet APIs**: separate product, separate pricing; solves a different problem (wallet activity) and is not a block-ingestion source.
- **Employer access**: not applicable. Personal and ecosystem projects stay on the paid team plan by design (NW-RFC-001 §4.3).

## 12. Risks specific to this research

| Risk | Mitigation |
|---|---|
| Blowing the research budget on E4/E6 | Hard caps in each experiment; E6 aborts at 100 credits |
| Concluding "S1 works" from one convenient nest | E4 runs on a second, less sparse contract set (e.g. a DEX router), and G3 requires both |
| Hint staleness at the tail (Dune indexing lag) | S1 only hints for ranges older than 24 h; the tail is always range-scanned from RPC |
| Scope creep: turning research into a Nuthatch change | §8 gate; any change ships as a separate RFC |
| Dune schema changes mid-research | Pin table names in the experiment notes; re-verify before each run |

## 13. Deliverables & tracking

Results are recorded on the tracking issue, #1381, not as files in this repo.

- [ ] E1 Alchemy inventory table
- [ ] E2 full-chain sizing table
- [ ] E3 seed sizing for the Graph-on-Arbitrum nest
- [ ] E4 sparse index savings + crossover `r*`, on both contract sets
- [ ] E5 hint correctness diff
- [ ] E6 head-following kill log
- [ ] E7 self-host vs Alchemy comparison
- [ ] E8 chain coverage table
- [ ] §5 "Measured" column filled
- [ ] §8 change check answered
- [ ] Decision memo (one page): per nest class, source per concern, numbers attached
- [ ] If S1 accepted and needs a Nuthatch change: separate RFC drafted
- [ ] NW-RFC-001 amended: sparse block index added to workstream C; credits re-allocated from research reserve

## 14. Open questions

1. Nuthatch's exact vocabulary and topology: what is a "nest", and is raw ingestion per nest or per chain today?
2. Does Nuthatch's backfill have any ranges-driven entry point already?
3. Which Alchemy methods dominate: `eth_getLogs` range scanning, per-block receipts, or state calls from transforms? (The whole analysis turns on this.)
4. What is Dune's current ingestion lag per chain, and is it published or must it be measured?
5. Are there Nuthatch-supported chains where Dune has *no* raw tables (I11)?
6. Does the results endpoint support server-side filtering that reduces exported MB for the block list, or must narrowing be done in SQL only?

## 15. Changelog

| Date | Change |
|---|---|
| 2026-09-14 | Initial draft |
| 2026-09-14 | Numbered RFC-0057 in the nuthatch repo (#1381). The 2026 feature freeze ended on 2026-09-08 and carve-outs are retired, so a change an experiment needs is proposed as a separate RFC. Nuthatch indexes EVM chains only, so I11 and E8 ask about Dune's coverage of the chains it supports. Results go on #1381 rather than in repo files. |
| 2026-09-14 | Review of #1382: H1's falsification threshold now matches its 100× claim (40 GB, not 4 GB), and S1, H3 and G3 are limited to log-derived scoped nests, since a block list from `logs` cannot see calls or state changes that emit nothing. |
| 2026-09-14 | Review of #1382: the second, less sparse contract set moves from a §12 mitigation into E4 and G3's condition, so S1 is not accepted on one sparse nest. E4 is capped at 200 credits per set; the experiments total 545 against the 600 budget. |
| 2026-09-14 | Review of #1382: H4 and G4 compare Dune with an RPC baseline measured on the same contract set and window, axis by axis, instead of against absolute thresholds that could pass while Dune was cheaper or faster than RPC. E6's kill criterion stays, as a budget cap that ends polling rather than a verdict. |
| 2026-09-14 | Review of #1382: H3 is stated in CUs, the unit E4 measures and G3 gates, since S1 can make more calls while spending fewer CUs. Reorg visibility leaves H4 and G4: a 24 h window with no reorg compares nothing, and a revised Dune row is not a reorg boundary. The self-host cost in §10 is divided by the blocks in a backfill, so both sides of H5 and G5 are dollars per block. |
| 2026-09-14 | Review of #1382: H5 is narrowed to self-host against Alchemy, which is all E7 and G5 measure. The Dune hybrid it also named is H1 and G1's question for full-chain nests, not H5's. |
