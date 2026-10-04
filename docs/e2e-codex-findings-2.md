# Nuthatch 3.13.0 runtime-admin API, second E2E pass

Tested on 2026-09-30 against `~/nuthatch-e2efix/target/release/nuthatch --version` =
`nuthatch 3.13.0`, on `thinkpad`. All runtime state was under `~/e2e-codex2/`; the
runtime listened only on `100.83.44.63:8391` and used `~/e2e-3.13/registry` read-only.
The tested NIDs were V1 `93ac4248e5bc0c2eb93245dfb95a949646ecfbacaf076254273ddedf1f7131e0`
and V2 `4f8148383f13681723b62e867c960620e3118abd213dcafb08013525feb9d0e7`.

## Findings

### Concurrent identical mounts can turn their shared job into `failed`

Severity: **blocker**

Repro commands:

```sh
B=http://100.83.44.63:8391
T=<the runtime token>
V1=93ac4248e5bc0c2eb93245dfb95a949646ecfbacaf076254273ddedf1f7131e0
for i in $(seq 1 8); do
  curl -sS -w ' %{http_code}\n' -XPOST \
    -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
    --data "{\"name\":\"race\",\"nid\":\"$V1\"}" "$B/_admin/nests" &
done
wait
curl -sS -H "Authorization: Bearer $T" "$B/_admin/mounts/race"
```

Expected: identical in-progress requests are idempotent. They should return the same job and
the job should advance to `live`, as the contract says.

Actual: all eight requests returned `202`, but the single persisted job ended `failed` with
`nest 'race' is already mounted - changing a mounted nest is \`nest upgrade\`, not a mount`.
The runtime log shows one worker mounted `race` and another worker subsequently failed the same
name. Thus a caller which received `202` can poll a terminal failure for a mount that another
request actually installed.

Evidence:

```text
# race-mount-{1..8}
{"name":"race","nid":"93ac...","phase":"accepted"} 202
{"name":"race","nid":"93ac...","phase":"joining"} 202
...

# mount-jobs.json
{"name":"race","nid":"93ac...","phase":"failed",
 "reason":"nest 'race' is already mounted - changing a mounted nest is `nest upgrade`, not a mount"}

INFO nest 'race' mounted onto the arbitrum-one cursor at block 457119519
WARN mounting 'race' failed: nest 'race' is already mounted - changing a mounted nest is `nest upgrade`, not a mount
```

## Behaviour that held up

- The first-pass invalid-name boot blocker is fixed. `../escape`, the empty string, 65-byte parts,
  three-part names, `x.y`, `usdc__moving`, and `default/usdc` each returned `400`; 64-byte alias
  and tenant/alias parts mounted and unmounted successfully. Rebooting thereafter left an empty
  roster.
- Authentication now precedes body parsing. An unauthenticated `POST /_admin/nests` with body
  `{broken` returned `401`, not a JSON error.
- The suspended tenant repro is fixed. `acme/usdc` returned `503` before and after a hard restart,
  then `POST /_admin/resume/acme/usdc` returned `202`, reached `live`, and served V1 at its same
  tenant-qualified route.
- The interrupted-move repro is fixed. A move of `moving` from V1 to V2 was killed after acceptance;
  `mounts.toml` contained no `__moving` record, the next boot bound 8391, and the retained move job
  reached `live` serving V2.
- Shared-dataset rehoming held in the exercised three-mount case. V1 was mounted as
  `acme/usdc`, `one`, and `globex/three`; `acme/usdc` was suspended while `one` moved to V2,
  then resumed unchanged; `acme/usdc` then moved to V2 while `globex/three` remained on V1.
  The log records the old cursor key passing to `globex/three`. Unmounting V2's first mount returned
  `kept`, unmounting V1's final mount returned `reclaimed`, and unmounting V2's final mount returned
  `reclaimed`; both `data/<nid>` directories were absent afterwards.
- Root `/metrics` is present before mounts and includes process metrics. With one mounted nest,
  480 concurrent root scrapes all returned `200`, and the response carried
  `nuthatch_nest_hot_store_bytes{nest="scrape"}`.
- The missing-registry-NID behaviour from the first report now matches the revised contract: with a
  registry configured, the request is accepted and later reports a failed job rather than a
  synchronous `404`.

The runtime started from a fresh directory for each isolated section. I did not count a default-
tenant or mid-suspend hard-kill result as held: the independent harness for those final cases was
interrupted by its own background-process waiting error after the already-recorded concurrency
failure, rather than by a runtime assertion. They need a clean follow-up run.
