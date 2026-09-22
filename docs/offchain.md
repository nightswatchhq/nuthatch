# Offchain data: file drops and scheduled price pulls

Offchain tables sit **outside** the reindex-from-chain guarantee (RFC-0045 §6). A chain table can be
deleted and rebuilt from the chain to the same bytes. An offchain table cannot, because its source
may have changed or gone. Each one is instead **reproducible by snapshot**: every snapshot is kept
immutably, and `offchain/manifest.json` records its content hash, source, ingest time and tool
version.

They live in their own namespace, `offchain__<table>`. They are queried through the ordinary `/sql`
surface, and deleting the `offchain/` directory leaves every chain table answering as before.

## Stage 1: drop a file

`nuthatch offchain drop prices.csv --table prices` seals one CSV, JSON array or Parquet file as a
snapshot. The file is read once, at drop time. Indexing and queries never read the path.

## Stage 2: pull a price feed on a schedule

`nuthatch offchain pull https://feed.example/prices.json --table prices --format json` fetches one
JSON array of objects and seals it through the same snapshot path. **It is a one-shot command.** It
never runs from the indexer's cursor or from a query. Scheduling it belongs to the host, so a feed
outage cannot stall indexing and a slow provider cannot slow a query.

**What one run does.**

- **Refuses before fetching** a non-`https` URL, a URL with credentials in it, and a format other
  than the declared one. `--allow-loopback-http` admits plain `http` to a loopback address, for a
  local fixture, and nothing else.
- **Makes up to `--attempts` requests** (default 3). Each is bounded by `--timeout-secs` (default
  30, response body included). Connection failures, timeouts, HTTP 429 and 5xx are retried after
  1 s, then 2 s. Any other status, including a redirect, fails at once. Redirects are never followed.
- **Refuses a response larger than `--max-bytes`** (default 16 MiB).
- **On success, appends a snapshot,** unless the body is unchanged since the last one, in which case
  it appends nothing. It records the SHA-256 of the bytes the source served beside the snapshot.
- **On failure, appends nothing.** Every earlier snapshot stays, so `offchain__<table>` keeps
  answering with the last good rows, and the failure is recorded against the table.
- **Exits non-zero on failure,** so the scheduler records it.

**Credentials are runtime configuration only.** `--header-env Authorization=PRICE_FEED_AUTH` sends
the value of `$PRICE_FEED_AUTH` as the `Authorization` header. It is read at run time and never
written anywhere. The recorded source drops the URL's query string (`https://feed.example/p?[redacted]`),
because a query string is where providers put keys. Neither the URL nor a key is part of the nest's
identity.

### Is the table fresh?

Every pulled table has a status view beside it:

```sql
SELECT * FROM offchain__prices__status;
```

| column | meaning |
| --- | --- |
| `source` | the URL, query string redacted |
| `attempted_at`, `succeeded_at` | `unix:<seconds>`; `succeeded_at` stays at the last success after a failure |
| `error` | the last attempt's failure, or NULL |
| `stale_after_secs`, `age_secs` | the declared cadence, and seconds since the last success |
| `stale` | true after a failed attempt, before any success, or once `age_secs` exceeds `stale_after_secs` |
| `snapshot`, `fetched_sha256` | the snapshot the last successful pull resolved to, and the SHA-256 of the bytes the source served. An unchanged body resolves to an existing snapshot, perhaps a dropped file, and is still recorded here |

Set `--stale-after-secs` a little above the timer's period. Without it, a timer that silently stops
firing leaves the last successful pull reading as fresh for ever.

A table whose **first** pull fails has a status view but no `offchain__<table>` at all. A query
against it fails with an unknown-table error, rather than answering from an empty, apparently
healthy price table.

### A timer-driven deployment

A systemd service and timer, pulling every five minutes and calling the table stale after three
missed runs:

```ini
# /etc/systemd/system/nuthatch-prices.service
[Unit]
Description=Pull the price feed into offchain__prices

[Service]
Type=oneshot
User=nuthatch
EnvironmentFile=/etc/nuthatch/price-feed.env   # PRICE_FEED_AUTH=...; mode 0600
ExecStart=/usr/local/bin/nuthatch offchain pull https://feed.example/prices.json \
    --table prices --format json --dir /srv/nest \
    --header-env Authorization=PRICE_FEED_AUTH --stale-after-secs 960
```

```ini
# /etc/systemd/system/nuthatch-prices.timer
[Timer]
OnBootSec=1min
OnUnitActiveSec=5min
RandomizedDelaySec=20s

[Install]
WantedBy=timers.target
```

A oneshot unit never runs twice at once. Pulls of different tables may overlap safely, because
the manifest is updated under a lock. A running `nuthatch dev` needs no restart: the next query
sees the new snapshot.

### Outage recovery

There is nothing to repair. While the source is down, `offchain__prices` keeps answering with the
last good snapshot, and `offchain__prices__status` says `stale = true` with the error.
`systemctl status nuthatch-prices` and the unit's journal show each failed run. The first run after
the source returns appends a fresh snapshot and clears the status. Every snapshot from before and
after the outage stays in the manifest.
