# OBIB case 3 - Ethereum block

[Case 3](https://github.com/sentioxyz/open-blockchain-indexer-benchmark) asks for **100,001 block
records** over blocks 0-100,000: header metadata, one row per block, no contract involved at all.

That last part is what makes it a different shape from cases 1, 2 and 6. This nest declares **no
contracts** — the rows come from `[extract] blocks = true` (RFC-0036 §4.2), enumerated from the
window rather than derived from logs, because a blocks table has to cover blocks that emitted
nothing.

```sh
nuthatch bench backfill --dir . --from 0 --to 100000 --runs 1 \
  --keep /tmp/case3 --rpc "$RPC" \
  --out ../docs/bench/obib-case3.json \
  --label "OBIB case 3: Ethereum block, blocks 0-100,000"

# the criterion is a ROW COUNT, so verify it rather than trusting the timing line:
nuthatch sql 'SELECT count(*) FROM "blocks"' --dir /tmp/case3
```

## Result

**100,001 records — the criterion, exactly.** Blocks 0-100,000, contiguous, no duplicates.

| | |
|---|---|
| Records | **100,001** (OBIB expects 100,001) |
| Range | `min=0  max=100,000  distinct=100,001` |
| Wall clock | 162 min (10.29 blocks/s) |
| Peak RSS | 55 MB |
| RPC requests | 19,300 (~5.2 blocks/call) |

Full artifact with the honest caveats: [`docs/bench/obib-case3.json`](../docs/bench/obib-case3.json).
The 162 minutes is not competitive with OBIB's 3.19 s reference and the artifact says so plainly —
this run used concurrency 1, a non-adaptive window and a free public endpoint, chosen for a
trustworthy row count on an 8 GB machine rather than for speed.

### The row count earned its keep

The first run reported `events: 100001` on the timing line and served **100,000** rows starting at
block 1. Genesis was ingested and then unreadable:

`Store::sealed_through()` returns 0 both when the watermark sits at block 0 **and when nothing has
ever been sealed**. The hot/cold union filtered hot rows to `block_number > sealed_through`, so with
nothing sealed block 0 belonged to neither half. Any nest indexed from block 0 lost exactly its first
block until something sealed — invisible on a nest starting later, which is how it survived.

The fix gates the hot filter on whether the table *has* sealed segments rather than on the
watermark's numeric value, so a row is withheld from hot only when cold genuinely covers it and
COR-1 disjointness holds everywhere else. Re-querying the same kept store returned 100,001 with no
re-ingest, which is the tell that the store was always right and only the read path was wrong.

This is the argument for the rule in #306 that record counts must match exactly. A timing-only
benchmark passes straight over a defect like this.

## Why `--keep`

The criterion is a row count, and `bench backfill` writes to a temp directory it deletes on the way
out. Without `--keep` the run produces the rows and then destroys the evidence, which is why this
case could be "built" for weeks without ever being *answered*.

## Endpoint matters more here than in any other case

Case 3 is one `eth_getBlockByNumber` per block by definition, so it is the case most sensitive to how
an endpoint batches and rate-limits. Measured 2026-08-10 on blocks 0-200:

| Endpoint | Wall clock | RPC requests |
|---|---|---|
| `ethereum-rpc.publicnode.com` | **1.4 s** | **22** (batched ~10 blocks/call) |
| `eth.drpc.org` | 171 s (blocks 0-100) | 362 |
| `eth-pokt.nodies.app` | fails | HTTP 403 on batch |

A 120x spread on identical work. `eth-pokt` is absent from `rpc_urls` deliberately: it answers 403
to a batch, which surfaces as *"N of N headers still missing"* rather than as an auth error.
