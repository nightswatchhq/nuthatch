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
