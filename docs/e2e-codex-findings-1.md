# Nuthatch 3.13.0 runtime-admin API E2E findings

Tested on 2026-09-30 against `~/e2e-3.13/bin/nuthatch --version` = `nuthatch 3.13.0`, on `thinkpad`, using only `~/e2e-codex/` and `100.83.44.63:8391`. The runtime had the Arbitrum public endpoints specified in the brief and a filesystem registry at `~/e2e-3.13/registry`.

The operator pages and `docs/admin-api.md` were read before testing. The published hosting-nests page currently carries a footer saying it was checked against 3.8.5.

## Findings

### Unvalidated mount names can persist a table which the runtime can never restart

Severity: **blocker**

Repro commands:

```sh
B=http://100.83.44.63:8391
T=e2e-codex-9c518431044c824a
V1=93ac4248e5bc0c2eb93245dfb95a949646ecfbacaf076254273ddedf1f7131e0
curl -si -XPOST -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
  --data "{\"name\":\"../escape\",\"nid\":\"$V1\"}" "$B/_admin/nests"
sleep 3
cat ~/e2e-codex/mounts.toml
kill -9 "$(cat ~/e2e-codex/runtime.pid)"
NUTHATCH_ADMIN_TOKEN="$T" nohup ~/e2e-3.13/bin/nuthatch dev --dir ~/e2e-codex \
  --listen 100.83.44.63:8391 --registry ~/e2e-3.13/registry >>~/e2e-codex/runtime.log 2>&1 &
sleep 3; tail -1 ~/e2e-codex/runtime.log
```

Expected: invalid route/tenant input is rejected at the API boundary, or at minimum cannot be persisted in a form that stops a runtime booting.

Actual: the POST answered `202`; the mount became `live`; the table contained `tenant = ".."`, `alias = "escape"`. The next boot exited before binding the port.

Evidence:

```text
HTTP/1.1 202 Accepted
{"name":"../escape",...,"phase":"accepted",...}
Error: tenant '..' is invalid (allowed: letters, digits, '_', '-')
```

The same run accepted `name: ""` and a 300-character name. Mounting the empty name logged `thread 'tokio-rt-worker' panicked at src/serve.rs:548:19: Nesting at the root is no longer supported`, and left its job in `joining`.

### A suspended tenant mount is resumed on boot and its recorded admin name no longer works

Severity: **blocker**

Repro commands:

```sh
B=http://100.83.44.63:8391; T=e2e-codex-9c518431044c824a
V1=93ac4248e5bc0c2eb93245dfb95a949646ecfbacaf076254273ddedf1f7131e0
curl -sS -XPOST -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
  --data "{\"name\":\"acme/usdc\",\"nid\":\"$V1\"}" "$B/_admin/nests"
sleep 4
curl -si -XPOST -H "Authorization: Bearer $T" "$B/_admin/suspend/acme/usdc"
kill -9 "$(cat ~/e2e-codex/runtime.pid)"
NUTHATCH_ADMIN_TOKEN="$T" nohup ~/e2e-3.13/bin/nuthatch dev --dir ~/e2e-codex \
  --listen 100.83.44.63:8391 --registry ~/e2e-3.13/registry >>~/e2e-codex/runtime.log 2>&1 &
sleep 3
curl -si "$B/acme/usdc/ready"
curl -si -XPOST -H "Authorization: Bearer $T" "$B/_admin/resume/acme/usdc"
cat ~/e2e-codex/mounts.toml
curl -si "$B/usdc/ready"
```

Expected: the docs promise that a suspended mount remains suspended across restart, answers `503` at its existing route, and resumes through its existing name.

Actual: before restart, `/acme/usdc/ready` returned the documented `503` JSON. After restart it returned `404`; `POST /_admin/resume/acme/usdc` returned `404` saying it was not suspended. The runtime served it live at `/usdc/ready` instead.

Evidence:

```text
HTTP/1.1 503 Service Unavailable
{"error":"'acme/usdc' is suspended; POST /_admin/resume/acme/usdc to resume it","suspended":true}

# after restart
HTTP/1.1 404 Not Found
{"error":"'acme/usdc' is not suspended"}

[runtime]
name = "e2e-codex"
suspended = ["acme/usdc"]
...
HTTP/1.1 200 OK                 # GET /usdc/ready
```

`GET /_admin/mounts` then showed two contradictory records: the stale `acme/usdc` job still `suspended`, and a new `usdc` job `live`.

### Restart during a move leaves an invalid internal `*.moving` mount and prevents boot

Severity: **blocker**

Repro commands:

```sh
B=http://100.83.44.63:8391; T=e2e-codex-9c518431044c824a
V1=93ac4248e5bc0c2eb93245dfb95a949646ecfbacaf076254273ddedf1f7131e0
V2=4f8148383f13681723b62e867c960620e3118abd213dcafb08013525feb9d0e7
curl -sS -XPOST -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
  --data "{\"name\":\"move\",\"nid\":\"$V1\"}" "$B/_admin/nests"
sleep 4
curl -si -XPOST -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
  --data "{\"nid\":\"$V2\"}" "$B/_admin/move/move"
sleep 2
curl -sS -H "Authorization: Bearer $T" "$B/_admin/mounts/move"
kill -9 "$(cat ~/e2e-codex/runtime.pid)"
NUTHATCH_ADMIN_TOKEN="$T" nohup ~/e2e-3.13/bin/nuthatch dev --dir ~/e2e-codex \
  --listen 100.83.44.63:8391 --registry ~/e2e-3.13/registry >>~/e2e-codex/runtime.log 2>&1 &
sleep 5; curl -si -H "Authorization: Bearer $T" "$B/_admin/mounts"
tail -1 ~/e2e-codex/runtime.log
```

Expected: unfinished jobs survive a restart and resume. A move must retain the old serving mount until the new one is ready.

Actual: the move job was `joining` immediately before `kill -9`. The restart did not bind port 8391. Its persisted table had a second mount `alias = "move.moving"`, and boot rejected it.

Evidence:

```text
{"name":"move","nid":"4f8148...9d0e7","phase":"joining",...}
Error: nest name 'move.moving' is invalid (allowed: letters, digits, '_', '-')
curl: (7) Failed to connect to 100.83.44.63 port 8391
```

### The documented runtime metrics endpoint does not exist

Severity: **should-fix**

Repro commands:

```sh
curl -si http://100.83.44.63:8391/metrics
curl -si http://100.83.44.63:8391/acme/usdc/metrics
```

Expected: the operator documentation and `docs/admin-api.md` state that runtime `/metrics` has per-nest series labelled `nest="<name>"`, including `nuthatch_nest_hot_store_bytes`, `nuthatch_nest_sealed_segments_bytes`, `nuthatch_nest_last_block`, and `nuthatch_nest_tip_lag_blocks`.

Actual: runtime `/metrics` returned `404` with an empty body. The nest-local endpoint returned `200` and unlabelled solo metrics such as `nuthatch_last_block` and `nuthatch_tip_lag_blocks`; it is not the documented runtime aggregation surface.

Evidence:

```text
HTTP/1.1 404 Not Found
content-length: 0

HTTP/1.1 200 OK                 # /acme/usdc/metrics
nuthatch_last_block 458259518
nuthatch_tip_lag_blocks 52031385
```

### Missing registry NIDs are accepted asynchronously despite the documented 404

Severity: **doc**

Repro commands:

```sh
B=http://100.83.44.63:8391; T=e2e-codex-9c518431044c824a
curl -si -XPOST -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
  --data '{"name":"missing","nid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}' \
  "$B/_admin/nests"
sleep 2
curl -si -H "Authorization: Bearer $T" "$B/_admin/mounts/missing"
```

Expected: the API contract's refusal table says `404` when the runtime does not hold a NID and the registry does not contain it.

Actual: the POST returned `202 accepted`; only the job later became `failed`.

Evidence:

```text
HTTP/1.1 202 Accepted
{"name":"missing",...,"phase":"accepted",...}
HTTP/1.1 200 OK
{"name":"missing",...,"phase":"failed","reason":"fetching nid ... not found in this registry",...}
```

## Behaviour that held up

- Admin authentication worked: omitted or wrong bearer token returned `401`; `?token=` and a correct bearer token returned `200`.
- JSON media-type enforcement worked: missing and `text/plain` `Content-Type` on POST both returned `415`.
- Invalid NID syntax, including `../../etc/passwd`, returned `400` before mounting.
- Eight simultaneous identical mounts of `acme/usdc` and V1 were idempotent: all returned the same `202` job; changing that in-progress name to V2 returned `409`.
- A normal mount progressed `accepted` -> `fetching` -> `joining` -> `live`; `/acme/usdc/ready` returned `200` while it backfilled.
- Dry run fetched and verified V2, returned `200` with `fetched: true`, installed the dataset, and left `GET /_admin/mounts` empty, as documented.
- Shared-dataset reclaim behaved correctly for ordinary names. V1 mounted as `a` and `b` returned `409 kept` with both names; unmounting `a?reclaim=true` kept it for `b`, which remained ready; after unmounting `b`, dataset reclaim returned `200 reclaimed` (3,724,585 bytes), and a second reclaim returned `404 absent`.
