# OBIB case 2 on nuthatch 4.10.1, Tenderly public gateway, 2026-10-05

The report beside this file, [`obib-case2-4.10.1-tenderly-2026-10-05.json`](obib-case2-4.10.1-tenderly-2026-10-05.json),
is the `--out` of the run below with one edit: the release binary reports no commit, so `commit` is
the v4.10.1 tag's commit, `12759bb`, as the 4.7.0 case-6 report did. It carries the medians; this
page records what it does not: the binary, the machine and each run.

It is the current-release measurement beside [`obib-case2.json`](obib-case2.json) (commit `5a81a37`,
2026-08-07, 49.2 s on an Alchemy account that is now closed). That report keeps the correctness
half of the case, which this run does not repeat: 7,634 accounts and 39 of 39 sampled balances
matching `balanceOf()` at block 22,500,000, verified on an archive endpoint. Every run here decoded
the same 343,845 `Transfer` rows that derivation starts from.

## Binary

The published release, not a local build.

- Release: [v4.10.1](https://github.com/nightswatchhq/nuthatch/releases/tag/v4.10.1), tag commit `12759bb`
- Asset: `nuthatch-aarch64-apple-darwin.tar.gz`, sha256
  `714ad73e92b2e9698db135416f3361ea3acf208f823462bb0deb4f77fcef4bce`, equal to the release's
  `.sha256` file
- Installed by `curl -fsSL https://nuthatch-indexer.com/install.sh | sh`; the extracted `nuthatch`
  has sha256 `13e1ea6d542e20c89cdd29a8c31b4066171ea44574f0f14a4dba1a64f4e7d431` and `--version`
  prints `nuthatch 4.10.1`

## Nest

[`obib-case2/`](../../obib-case2/) at the tree's `main` as of the run: LBTC
`0x8236a87084f8b84306f72007f36f2618a5634494`, `Transfer` only, `block_timestamps = false`, from its
deployment block 19,888,667.

## Machine

- `hw.model` Mac17,8, Apple M5 Pro, 18 cores (6 performance, 12 efficiency), 48 GB RAM
- macOS 26.5.1, load average about 3 when the run started

## Endpoint

`https://mainnet.gateway.tenderly.co`, Tenderly's keyless public gateway. No key, no account. It
throttled: the log has 55 `HTTP 429` lines across the three runs, and the longest wait it asked for
was 29.8 s. The wall clock below is the gateway's number as much as the indexer's.

## Command

```sh
nuthatch bench backfill --dir obib-case2 --from 19888667 --to 22500000 --runs 3 \
  --seal-direct --concurrency 8 --rpc https://mainnet.gateway.tenderly.co \
  --label "OBIB case 2 (derive-first): LBTC Transfer, deployment-22,500,000, balances derived not fetched; 4.10.1 release binary, Tenderly keyless gateway" \
  --out obib-case2-4.10.1-tenderly-2026-10-05.json
```

Started 2026-10-05 20:14 UTC, finished 20:18 UTC. A single-run session a few minutes earlier
(73.3 s, 183 requests, 345 MB) was discarded in favour of this three-run one.

## Runs

| run | events | wall clock | events/s | peak RSS | RPC requests |
|---|---|---|---|---|---|
| 1 | 343,845 | 73.4 s | 4,687 | 356 MB | 174 |
| 2 | 343,845 | 79.2 s | 4,341 | 408 MB | 175 |
| 3 | 343,845 | 51.3 s | 6,708 | 403 MB | 174 |

Median as the report states it: **73.4 s**, 4,687 events/s, 403 MB peak RSS, 174 RPC requests. The
event count was the same in every run; the request count moved by one.
