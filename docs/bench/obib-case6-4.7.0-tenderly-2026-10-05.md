# OBIB case 6 on nuthatch 4.7.0, Tenderly public gateway, 2026-10-05

The report beside this file, [`obib-case6-4.7.0-tenderly-2026-10-05.json`](obib-case6-4.7.0-tenderly-2026-10-05.json),
is the unedited `--out` of the run below. It carries the medians; this page records what it does not:
the binary, the machine and each run.

It replaces [`obib-case6.json`](obib-case6.json) (commit `dc9fcaf`, the 1.0.1 era, 49.5 s) as the
published case 6 figure. That run used an Alchemy account that is now closed, so it cannot be
reproduced and is withdrawn (#1844).

## Binary

The published release, not a local build, so anyone can run the same bytes.

- Release: [v4.7.0](https://github.com/nuthatch-org/nuthatch/releases/tag/v4.7.0), tag commit `da4dc65`
- Asset: `nuthatch-aarch64-apple-darwin.tar.gz`, sha256
  `7ed1c2d89de0ed42ee599bb2e62f71ac169ac2e5bed5057d6dfdca01dee46d2d`, checked against the release's
  `.sha256` file
- Extracted `nuthatch`, sha256 `fd9e6c25155adbf252f85ecf9a0764de4b473eac284c23aa649ea6a27edd98b0`;
  `nuthatch --version` prints `nuthatch 4.7.0`

## Nest

[`nuthatch-org/obib-case6`](https://github.com/nuthatch-org/obib-case6) at `main` as of the run:
the Uniswap V2 factory `0x5c69bee701ef814a2b6a3edd4b1652cb9cc5aa6f`, `PairCreated` discovering
children, `Swap` on each child, `block_timestamps = false`.

## Machine

- `hw.model` Mac17,8, Apple M5 Pro, 18 cores (6 performance, 12 efficiency), 48 GB RAM
- macOS 26.5.1, load average about 2 when the run started

## Endpoint

`https://mainnet.gateway.tenderly.co`, Tenderly's keyless public gateway. No key, no account.

## Command

```sh
nuthatch bench backfill --dir obib-case6 --from 19000000 --to 19010000 --runs 5 \
  --seal-direct --window-adaptive --rpc https://mainnet.gateway.tenderly.co \
  --label "OBIB case 6: UniV2 factory templates - PairCreated + Swap on discovered pairs, blocks 19,000,000-19,010,000" \
  --out obib-case6-4.7.0-tenderly-2026-10-05.json
```

`--window-adaptive` is kept so the command matches the nest's README, but the bench says it has no
effect here: the factory path always adapts its window.

Started 2026-10-05 04:43:42 UTC, finished 04:44:07 UTC. This was the only session; nothing was
discarded.

## Runs

| run | events | wall clock | events/s | peak RSS | RPC requests | children |
|---|---|---|---|---|---|---|
| 1 | 35,271 | 5.4 s | 6,537 | 205 MB | 14 | 232 |
| 2 | 35,271 | 4.8 s | 7,423 | 229 MB | 14 | 232 |
| 3 | 35,271 | 4.7 s | 7,546 | 221 MB | 14 | 232 |
| 4 | 35,271 | 4.5 s | 7,798 | 236 MB | 14 | 232 |
| 5 | 35,271 | 4.5 s | 7,786 | 251 MB | 14 | 232 |

Median as the report states it: **4.67 s**, 7,545 events/s, 229 MB peak RSS, 14 RPC requests, 232
children. The per-run wall clocks are the bench's own one-decimal rounding.

No run was throttled: the log has no retry or rate-limit line, and the event, child and request
counts were identical in all five. 35,271 is 35,039 `Swap` rows, OBIB's expected count, plus the 232
`PairCreated` rows.
