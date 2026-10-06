# README check, 2026-10-05

Every command in `README.md` checked against the published **4.10.1** release (#1923): run as written,
with its exit code and the first lines of what it printed, except the four the tables mark otherwise.
Two write into the user's home (`claude mcp add`, the skill copy) and were not run; `nuthatch worker`
and `nuthatch serve` need a Postgres and were checked in `--help` only; one row lists commands
present in `--help` whose README invocations `tests/doc_command_check.rs` resolves against clap. A
record of a moment: it is not rewritten when the README changes; the next check is a new file.

Binary: `nuthatch-aarch64-apple-darwin.tar.gz` from v4.10.1, sha256
`714ad73e92b2e9698db135416f3361ea3acf208f823462bb0deb4f77fcef4bce` (equal to the release's `.sha256`);
the installed `nuthatch` has sha256 `13e1ea6d542e20c89cdd29a8c31b4066171ea44574f0f14a4dba1a64f4e7d431`.
Linux: `nuthatch-x86_64-unknown-linux-gnu.tar.gz`, extracted binary sha256
`a62bec10ccf9d8926e276feac1944a4d18fb8d9baa52a99372fe90d7c455a5ae`.

## The quickstart, timed

The README's first five lines, verbatim, in a shell with an empty environment, a temporary `HOME`
and the system `PATH` only (`env -i HOME=$(mktemp -d) PATH=/usr/bin:/bin:/usr/sbin:/sbin sh`), then
`nuthatch sql "SELECT count(*) FROM weth__transfer"` every two seconds until it printed a non-zero
count. The clock starts at the `curl`.

| | macOS, M5 Pro (18 cores, 48 GB), macOS 26.5.1 | `ubuntu:24.04` container on the ThinkPad (glibc 2.39) |
|---|---|---|
| install done, `nuthatch 4.10.1` | 6 s | 7 s |
| `init` done | 11 s | 17 s |
| first non-zero count | **13 s** (3,835 rows) | **21 s** (4,762 rows) |
| every exit code | 0 | 0 |

The container has no `curl`; `apt-get install curl ca-certificates` ran before the clock started. Both
runs backfilled 301 blocks from a cold start (`cold start: backfilling from block 26128160 (tip
26128460)` on the Mac) and `dev` exited 0 on SIGTERM. On Linux `init` printed `chain id check for
https://mainnet.gateway.tenderly.co timed out after 2s - leaving it in the pool` and carried on.

`init` output, both platforms:

```
→ no --chain given; probing known chains for 0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2…
  ✓ found on mainnet
→ resolving ABI for 0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2 on mainnet…
  ✓ ABI resolved via Sourcify
  · alias weth
  ✓ deployed at block 4719568
✓ scaffolded nest 'tmp.OTaLuIbGy0' (1 contract(s), 4 table(s)) in .
```

## Install section

| command | exit | output |
|---|---|---|
| `curl -fsSL https://nuthatch-indexer.com/install.sh \| sh` | 0 | `downloading nuthatch-aarch64-apple-darwin.tar.gz… verifying checksum… installed to …/.local/bin/nuthatch`, then the PATH line and the three `next:` commands |
| `gh attestation verify nuthatch-x86_64-unknown-linux-gnu.tar.gz --repo nightswatchhq/nuthatch` | 0 | silent unless a TTY; with one: `Build workflow: .github/workflows/release.yml@refs/tags/v4.10.1`, signer repo `nightswatchhq/nuthatch` |
| `rustup toolchain install 1.95.0` | 0 | already installed |
| `cargo +1.95.0 install --git https://github.com/nightswatchhq/nuthatch nuthatch` | 0 | `Installed package nuthatch v4.10.1 (…#fd163e94)`, 5m 59s compile; `--version` prints `nuthatch 4.10.1`. Two earlier attempts died on crates.io index downloads timing out on this network, not on the command |
| `objdump -T nuthatch` on the Linux binary (ThinkPad) | 0 | newest versioned symbols `GLIBC_2.35` (`hypot`, `hypotf`); `NEEDED`: `libgcc_s.so.1`, `libm.so.6`, `libc.so.6`, `ld-linux-x86-64.so.2`. The README's floor and link list hold on 4.10.1 |
| `docker manifest inspect ghcr.io/nightswatchhq/nuthatch:4.10.1` and `:4.10.1-scaled` | 0 | both present |
| `nuthatch init 0xADDR --chain world-chain --rpc https://your-endpoint.example` | 1 | `Error: could not read the chain id for 'world-chain' from --rpc` (placeholder host, as expected) |
| `nuthatch init 0xADDR --chain world-chain` (no `--rpc`) | 1 | `Error: unknown chain 'world-chain' (try: mainnet, arbitrum-one, base, optimism, polygon, gnosis, bsc) - or pass --rpc <url> …`. The remedy is there; the list names seven of the nine built-ins: **#1928** |
| `nuthatch doctor --rpc https://your-endpoint.example --address 0xADDR` | 0 | `getLogs window FAILED - no probe succeeded`, `JSON-RPC batch FAILED`, `archive depth UNKNOWN` (placeholder host) |
| `nuthatch doctor --rpc https://mainnet.gateway.tenderly.co --address 0xA0b8…eB48` | 0 | `getLogs window up to 160 blocks (recommend --window 80)`, `JSON-RPC batch 10 - narrow`, `archive depth UNKNOWN - refused (plan/quota)` with the 429 body |
| `nuthatch init 0xADDR --chain arbitrum-one --rpc https://your-endpoint.example/arbitrum` | 1 | leaves the unreachable endpoint in the pool with a note, then `Sourcify had no verified ABI, chain 42161 has no keyless Blockscout ABI endpoint, and ETHERSCAN_API_KEY is not set` (USDC's mainnet address is not verified on Arbitrum; the command shape is what was under test) |

## Querying section, against a running `nuthatch dev --backfill 300` on a USDC nest

| command | exit | output |
|---|---|---|
| `nuthatch sql 'SELECT "from" AS sender, count(*) AS n FROM usdc__transfer GROUP BY 1 ORDER BY n DESC LIMIT 5'` with `dev` stopped and no store yet | 1 | `no store at ./nuthatch.redb and nothing answering there. Is nuthatch dev running…` |
| the same with `dev` running | 0 | a five-row aligned table, `0xb92f…ff4f 548` first, `(5 rows)` |
| `nuthatch sql --json 'SELECT count(*) FROM usdc__transfer'` | 0 | `{"count_star()":5175}` |
| `curl 'localhost:8288/sql?q=SELECT%20count(*)%20FROM%20usdc__transfer'` | 0 | `{"count":1,"truncated":false,"degraded":false,"degraded_tables":[],…"rows":[{"count_star()":5175}],…"provenance":{…"source":"hot+sealed",…}}` |
| `nuthatch sql 'SELECT count(*) FROM usdc__transfers'` (misspelt table) | 1 | engine error kept, then `hint: no table usdc__transfers; the closest is usdc__transfer` |
| `curl localhost:8288/ready` | 0 | `{"version":"4.10.1","ready":true,"stalled":false,…"lag_blocks":237,…}` |
| `curl localhost:8288/metrics` | 0 | 118 `nuthatch_` series, among them `nuthatch_tip_lag_blocks`, `nuthatch_rows_decoded_total`, `nuthatch_rows_sealed_total`, `nuthatch_reorgs_total`, `nuthatch_sql_queries_total`, `nuthatch_rss_bytes` |
| `curl localhost:8288/_admin/` | 0 | HTTP 200 on localhost |

## Point an AI at it

| command | exit | output |
|---|---|---|
| `nuthatch mcp --print-config` | 0 | `Claude Code (one-liner): claude mcp add nuthatch -- <path>/nuthatch mcp --url http://127.0.0.1:8288`, then the `.mcp.json` block |
| `claude mcp add nuthatch -- nuthatch mcp --url http://127.0.0.1:8288` | not run | it writes the user's Claude Code config; `claude mcp add --help` answers, and the line is what `--print-config` prints |
| `cp -r skills/nuthatch-builder ~/.claude/skills/` | not run | a copy into the user's home; the source directory exists in the tree |

## Everything else

| command | exit | output |
|---|---|---|
| `nuthatch recipe add total_supply` | 0 | `✓ wrote ./views/total_supply.sql - a derived total_supply view (no eth_call)` |
| `nuthatch init … --no-timestamps` | 0 | scaffolds, with the same proxy-history warning as the plain `init` |
| `nuthatch metadata fetch`, `nest bundle`, `publish {sync,verify,status,finalise}`, `seed --from --dir`, `doctor --publish`, `dev --publish-target` | 0 | each present in `--help` on 4.10.1; `tests/doc_command_check.rs` resolves every backticked invocation in the README against clap |

## Production section

`nuthatch worker` and `nuthatch serve` are in `--help` (`SCALED:`); they need a Postgres and were not
run locally. `scripts/version-check.sh`: `no stale references in anything a reader would copy-paste`.

## Figures

Thirty-odd figures were traced. Sourced to a committed `docs/bench` report, a release note or a dated
measurement, and now cited inline: the OBIB case 1, 2 and 6 tables; the 99.3 % network share
(`docs/benchmarks.md`, 2026-08-23); the 22.6x timestamp cost (RFC-0029 §4c, which replaces the older
"about 85 %" estimate the RFC itself calls too low); the 2.5x Burrmill-to-DuckDB ratio (v4.1.0 notes,
2026-10-01); the 2.15 s to 87.7 ms panel (`docs/bench/3.0.0-alpha-live.md`); the 2.6 s / 232-event
harness defect (#310); the 48.2 s cold control (`obib-case6-cold-control.json`); OBIB's reference
times and counts (its README, checked today). Re-measured on 4.10.1: OBIB case 2, three runs against
Tenderly's keyless gateway, `docs/bench/obib-case2-4.10.1-tenderly-2026-10-05.json`. Removed, no
source found: Arcaidia's "within two hours", the August "17 s to 57 s" band, and "N nests for roughly
one nest's RPC cost". `cargo test --locked --test it bench_citations` passes.

## Links

Every external link answers 200 (`curl -sIL`): arcaidia.io, github.com (repo, CI badge, #978,
burrmill, obib-case6, OBIB and its PR #3), graphops.xyz, lodestar-dashboard.com, nuthatch-indexer.com
(root, /stories, /install.sh, /mirror, the DuckDB post). Every relative link and `docs/operators.md`
anchor resolves.
