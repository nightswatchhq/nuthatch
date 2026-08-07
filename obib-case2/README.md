# OBIB case 2 - LBTC full, the derive-first way

[Case 2](https://github.com/sentioxyz/open-blockchain-indexer-benchmark) asks for per-account LBTC
balances over blocks 22,400,000-22,500,000, and its reference implementations get them with one
`balanceOf()` call per account. **This nest makes none.**

For a plain ERC-20, a balance is fully determined by its `Transfer` history including mints and burns.
The only reason the benchmark needs the RPC augmentation is that a run windowed to 100,001 blocks
cannot know what an account held *before* the window. Index the token's whole life and the question
disappears - so we trade 2.5M extra blocks of cheap `getLogs` for zero `eth_call` round trips.

```sh
# The published number. Never commit a key - the endpoint arrives on the command line, which is
# also what OBIB's own methodology does.
nuthatch bench backfill --dir . --from 19888667 --to 22500000 --runs 3 \
  --seal-direct --concurrency 8 --rpc "$RPC"

# Then the case's actual output:
nuthatch sql --dir . "SELECT count(*) FROM case2_accounts"     -- 7634
```

## Result

| | |
|---|---|
| Accounts produced | **7,634** - the published figure, exactly |
| `eth_call` round trips | **0** |
| Wall clock | **49.18 s** (median of 3, 2,611,334 blocks, 343,845 events, 136 RPC requests) |
| Peak RSS | 325 MB |

Reference times for the same case: Sentio 7.78 min, Envio 8.54 min, Subsquid 46.85 min.

## Why the count is 7,634 and not 7,635

`0x0` appears in 975 transfer legs in the window as the mint/burn counterparty. It is not a holder -
`balanceOf(0x0)` is not a holder balance - and excluding it gives exactly the published count. That
off-by-one was the tell that the interpretation was right.

## Proven equal, not asserted

At block 22,500,000, 39 accounts were checked against `balanceOf()` on an archive endpoint: the ten
largest, the ten smallest non-zero, ten with a zero balance, and ten by address order. **39/39
matched**, including all thirteen zero-balance accounts - the hard case, where an account received and
then sent everything out, and an off-by-one in the ledger would show up as a residue.

See [`docs/bench/obib-case2.json`](../docs/bench/obib-case2.json) for the machine-readable record.
