# Network endpoint deployment gates

This records both the temporary public gateway route and the still-incomplete
native facade. The native entity model and its final acceptance checks in
README.md remain incomplete.

Chief selected the ThinkPad for indexing and the Helsinki VPS's existing Caddy
for public HTTPS on 2026-09-17. The proposed service hostname under Chief's chosen
domain is `network.thenightswatch.dev`, with GraphQL at `/graphql`. Add a Namecheap
`A` record for host `network` to `89.167.109.4`; leave the apex `@` website record
unchanged. Namecheap's authoritative server and Cloudflare's public resolver both
returned the correct address on 2026-09-17 after Chief added the record.
Certificate issuance was verified through the public HTTPS route on 2026-09-19.
The ThinkPad became reachable again on 2026-09-18. A release build from the
worktree passed in a separate verification directory, without replacing any
running service. All 17 network fixtures then passed in release mode. A separate
small capacity replay measured 98,764 KiB peak RSS; neither check establishes
full-history resource use or freshness. The exact tested source and limitations
are recorded in `validation/linux-fixture-validation.json`.
Chief initially set a hard US$50 additional-spend ceiling for the full-history
backfill, then raised it to US$150 on 2026-09-19.
The RPC key cannot set the provider's account billing cap. The local budget policy
in `rpc-budget.toml.example` now reserves at most 285,000,000 units. At the
published $0.525/M CU rate, that is $149.625 before tax. This is a conservative
estimate, not access to the account's invoice, and does not cover unrelated usage.
Read-only checks confirmed that Caddy already
has `http.handlers.rate_limit` and proxies other nests to the ThinkPad over
Tailscale. Keep this nest in its own service, data directory and listener; do not
replace an existing nest or expose its SQL, MCP or administrative routes.

## Temporary public gateway route

On 2026-09-19, `https://network.thenightswatch.dev/graphql` was enabled as a
rate-limited HTTPS proxy to the grafted Network Subgraph deployment
`QmR8WQECdNR6TSUf4FfLSFW7D5RDGGkd7F4A6me56pLqJb`. Its gateway credential is
held in a root-only Caddy environment file and is not present in the URL, nest,
repository or public configuration. Caddy permits GraphQL only on `POST /graphql`
(and an explanatory response on `GET /graphql`), caps request bodies at 32 KiB,
limits traffic to 1,200 requests per ten seconds per IP and 12,000 per ten seconds
globally, and returns 404 for all other routes.

At deployment, the upstream reported block 506779393 with no indexing errors.
The verbatim indexer-rs allocation and `paymentsEscrowAccounts` monitor documents
both returned HTTP 200 without GraphQL errors through the public hostname. The
redemption document also binds and executes when its required `allocationId_in`
list is supplied. The Rust Horizon root is `paymentsEscrowAccounts`, not the
earlier reconstructed `escrowAccounts` name. This proxy is the immediate
operational endpoint. It must be replaced with the native facade only after the
full-history, parity, freshness and capacity gates below pass.

The supplied Alchemy Arbitrum endpoint passed six bounded probes: chain ID,
historical header, historical staking-proxy bytecode, historical EpochManager
`currentEpoch()` and `blockNum()`, and an allocator-aware issuance read. These
prove those checkpoints are available, not complete archive coverage, throughput
or cost. The credential must stay outside Git and content-addressed nest files.
Use the runtime `--rpc` and `--state-rpc` overrides through a restricted operator
configuration. Never include the URL in a public report or request log.

Two further requests verified EIP-1898 archive support at block 500705945:
`eth_getBlockByNumber` returned
`0x613efe2ce6309b2f5fe158a177739bd2956a9429ccd057e6140b46a910c9a64a`,
and `getAllocatedIssuancePerBlock()` using that hash with `requireCanonical: true`
returned `96584000000000000000`. These requests fit the reserved probe allowance;
they do not constitute a backfill or throughput measurement.

Twenty further `eth_getCode` probes confirmed the configured creation boundaries:
each address had zero bytes at the preceding block and the listed bytes at start.
This is archive-state evidence, not deployment-receipt verification.

| Contract | Start block | Runtime bytes |
| --- | ---: | ---: |
| EpochManager | 42449227 | 2284 |
| ServiceRegistry | 42449357 | 2284 |
| L2Curation | 42449403 | 2284 |
| L2GNS | 42449510 | 2284 |
| Staking proxy | 42449585 | 2284 |
| RewardsManager | 42449638 | 2284 |
| PaymentsEscrow | 397491106 | 1169 |
| SubgraphService | 397492865 | 1169 |
| GraphTallyCollector | 399496057 | 6608 |
| IssuanceAllocator | 486895439 | 1114 |

Before backfill, verify deployment start blocks, complete the event/call inventory,
and enforce the US$50 additional-spend budget. `--concurrency 1` bounds simultaneous
windows, not requests per second or total cost. Do not describe it as a cost cap.
Do not use a recent-history `--backfill` override for this cumulative ledger.

A separately capped genesis pilot replayed blocks 42440000 through 42460000
on 2026-09-17. The local debug binary, built from the dirty worktree based on
`6e5aef0b`, reported 76 rows including 42 resolved calls, 24 source RPC requests,
and 3.39 seconds. Source request counts exclude separate state-call billing.
The reported peak RSS was zero and is not a valid memory measurement. This small
early-chain interval is not representative of full-history or tip throughput.
At block 42460000, 35 GraphNetwork fields and all 13 selected Epoch fields matched
the fresh rebuild deployment exactly. `validation/genesis-parity.json` records
the query and both responses; `tests/fixtures/network-clients/genesis-facts.json`
captures the decoded facts and pinned calls for an offline regression test.
It proves neither current-head parity nor allocation, delegation or escrow parity.

A separate 2026-09-18 check compared two active and two closed legacy allocations
at block 200000000. Six logs from the existing Lodestar nest and six independent
hash-pinned `EpochManager.blockNum()` results match all 13 selected fields from
the reference through both SQL and the HTTP GraphQL handler. This caught the
Arbitrum L1-number/L2-hash distinction for allocation creation and closure.
Two six-call probe batches were charged conservatively against the earlier 10,000
CU probe reserve (312 CUs total); the first result was lost to a local capture error.
The fixture is scoped to these allocation IDs, not a complete indexer ledger.

On 2026-09-18 the fresh rebuild had advanced to block 506401908, making older
Horizon checkpoints queryable again. At block 502165911 its nine signers match
independent authorization/thaw history through the facade's HTTP handler. Nine
earliest deposits establish the related payer identities; those deposits are
not a complete balance history. Cancellation, revocation and reauthorization
have no real examples in this capture and remain synthetic-test coverage only.

The equivalent Horizon check at block 506000000 matches 15 fields for four
allocations through HTTP. Its fixture has 23 independently indexed lifecycle,
resize and reward logs, two directly fetched POIPresented logs, and seven
hash-pinned EpochManager reads. The seven calls plus one filtered getLogs request
reserve another 257 CUs within the earlier probe allowance. The comparison found
and fixed a string-versus-Boolean wire error in `forceClosed`.

One further 26-CU hash-pinned getter establishes the initial governor at block
42449197. The three additional genesis control fields match the reference.
At the time of those probes, no full-history replay had started. Three bounded all-GRT-log density probes
(225 CUs reserved) returned 171, 18 and 34 distinct event blocks respectively in
10,001-block windows ending at 150000000, 480000000 and 506000000. These samples
vary substantially and do not establish a whole-history cost upper bound. The
durable CU guard, not extrapolation from the sparse genesis pilot, must enforce
the allowance throughout replay.

Two bounded Linux ingestion runs on 2026-09-18 retained their independent source
facts and recorded reservations from the shared durable ledger. Blocks
505990000..506000000 produced 453 rows (34 calls) in 7.78 seconds for 2,764 units;
149990000..150000000 produced 531 rows (184 calls) in 26.14 seconds for 12,664 units.
The reports are `validation/ingestion-pilot-506m.json` and
`validation/ingestion-pilot-150m.json`. Neither interval is a cumulative ledger.
These costs do not establish that a full replay fits the allowance. Do not launch
an uncapped replay or increase the allowance on the strength of these samples.

Repeating both intervals after reusing each window's timestamp headers for its
canonical reads reduced reservations to 2,104 and 9,004 units respectively.
Both complete segment directories, including manifests, were byte-identical to
their baselines. The shared ledger then held 26,536 reserved units. The comparison
is recorded in `validation/shared-header-ingestion.json`; single-run timings are
not a sustained throughput claim. Headers are not reused across windows, and
fork/timestamp mismatches still fail before state RPC calls.

A subsequent continuous pilot covered 42440000..43440000: 98 stored rows,
including 58 calls, in 11.16 seconds, with 36 MB sampled peak RSS and 23,078
reserved units. The ledger reached 49,614 units before the resumable run below.
`validation/ingestion-pilot-genesis-million.json` records the measurement.
The retained cold segments match all 35 selected network fields and 13 epoch
fields at both 42460000 and 43440000. The latter reference capture is
`validation/genesis-million-reference.json`. Run the operator regression with
`NETWORK_COLD_REPLAY_DIR=<retained-directory> cargo test --locked --test it independently_ingested_cold_history_matches_genesis_reference -- --ignored`.
The test copies segments into a disposable fixture and does not create a hot
store or modify the captured history. These sparse early blocks do not prove
current-head financial state or whole-history cost.

At 2026-09-18 14:43 UTC, a separate resumable validation backfill started on the
ThinkPad as user unit `network-facade-backfill-20260918.service`. Its nest is
`/home/pepe/.local/state/network-facade/nest`, using the existing budget ledger
and verified release binary. The transient unit has a 15-minute runtime limit,
2 GiB memory limit, two-CPU quota and no automatic restart. The listener is
loopback-only at port 8124, and `/graph/status` returned HTTP 503 with
`historical Graph freshness policy is not configured`. No Caddy route was enabled.
This is an ingestion validation session, not a completed backfill or a permanent
service. Check its current state before resuming; do not reset its durable budget.
`deploy/run-indexer.sh` requires the existing ledger and reads the RPC credential
from the restricted operator directory rather than the nest's source files.
The first startup exposed a validator-only defect: its DuckDB connection lacked
the scalar functions available to actual queries. A regression reproduced the
spurious missing-function/view warnings, and the local fix registers those
functions for validation and error diagnosis too. That fix passed the startup
regression and 92 analytics tests; it is not in the running pilot binary and does
not change the ingestion facts. Do not interpret the older startup warnings as
proof that the populated query path has been validated at chain head.

The first resumable session was deliberately stopped cleanly at 14:51:46 UTC,
before its runtime limit, after finding unnecessary periodic header scheduling
for event-driven calls. Its retained manifest covers through 84363706 and holds
11,096 rows in 54 segments. The scheduler fix keeps sampled-call schedules
unchanged, while event-driven declarations derive their blocks only from events.
Repeating the first-million-block pilot reserved 3,038 units instead of 23,078;
the complete segment directories and manifests were byte-identical. The shared
ledger reached 1,087,551 units after that repeat, including the stopped session.

Read-back of the larger retained snapshot found two SQL issues absent from the
small genesis check. The allocation clock now normalizes its existing
empty/reverted-read fallback before ABI decoding; other malformed successful
reads still fail. GraphToken approvals and ordinary/self transfers do not save
the freshly read network clock unless they create an epoch, whereas mint/burn
does. The saved-clock projection now distinguishes those cases. At block
84363706, all 35 selected GraphNetwork fields and all 13 selected fields across
145 epochs match `validation/backfill-84m-reference.json` against the fresh
rebuild. The operator cold-replay regression includes this checkpoint when its
input manifest covers it. This remains historical parity, not live client or
current-head acceptance, and later Horizon save paths still require validation.

The corrected release resumed as `network-facade-backfill-20260918-resume1`
at 15:09:57 UTC from block 84363707, directly after the retained watermark
84363706. Its journal confirmed progress through 92005706 at 15:12:42 UTC.
The existing budget ledger was retained. This unit has the same 15-minute
runtime limit and no automatic restart. Startup no longer reported missing
scalar functions; the unrelated `version()` volatility warning remains.
The three corrected SQL views were installed before starting the unit.
See `validation/event-triggered-ingestion.json` for the binary fingerprint and
the checkpoint evidence. Progress here is a dated observation, not a live status.

A subsequent local regression covers Horizon `POIPresented`: the upstream
handler saves its allocation, not the refreshed network clock, unless its helper
creates an epoch. The regression initially returned L1 block 127 instead of the
persisted 126. The corrected projection passes both the same-epoch case and a
new-epoch presentation that must persist block 131. The resume1 unit was stopped
cleanly at 15:17:14 UTC after reporting progress through block 100605706.
The corrected view was installed while stopped; its SHA-256 is
`5750b94eb50ac0e3042246b4a694d412889cd3cd895bb972e04a45ecab653fd6`.
Other conditional save paths still need their own validation. Local verification
passed 1,401 library tests (two ignored), 367 integration tests (seven ignored),
the extended POI-clock regression separately, the captured cold-history parity
test separately, formatting and clippy with warnings denied.
At 15:17:50 UTC, `network-facade-backfill-20260918-resume2` confirmed resumption
from block 100620846 after sealing through 100620845. The installed view hash
matched locally. Its existing ledger, loopback-only listener and 15-minute
runtime limit remain unchanged; `/graph/status` still returned HTTP 503.

On 2026-09-19, the previous unit was confirmed to have stopped at its intended
15-minute runtime limit, with a graceful shutdown and 54.1 MiB reported peak
memory. Its stopped snapshot contains 73,187 rows in 113 segments through
123615752. A copied ledger reports 1,698,806 reserved units of 39,980,000; neither
the ledger nor the policy was reset. A new bounded unit,
`network-facade-backfill-20260919-resume1`, started at 08:46:43 UTC with a one-hour
runtime limit and the same memory, CPU and spending limits. Its fetch watermark
advanced to 128780286 at 08:58:13 UTC despite a provider range refusal that
succeeded when split. This is dated progress, not a throughput forecast.

The larger cold snapshot exposed a ten-second query timeout and another saved
clock discrepancy. Clock carry-forward now uses an ordered window, and the
delegation recursive fold materializes its ordered input once. Existing-indexer
delegation parameter changes only save the refreshed network clock when they
create an epoch; first-time indexer creation also saves it. The saved-clock view
now follows the indexer identity view in `48-network-clock-saved.sql`.
With those changes, all 35 network fields and 13 fields across 261 epochs match
the reference at block 123615752, within the unchanged ten-second per-query
limit. The four-checkpoint operator test passed in 21.42 seconds in one local
run. `validation/backfill-123m-check.json` records the snapshot, view fingerprints
and limitations. This does not establish concurrent serving capacity or tip
freshness. The resume1 unit was stopped cleanly at 09:00:34 UTC to install the
three corrected SQL files. Their remote hashes matched the recorded local
hashes. Its copied ledger reported 2,137,613 units reserved, without a policy
change. `network-facade-backfill-20260919-resume2` resumed at 09:01:13 UTC from
128729524, after durable sealing through 128729523. It retains the one-hour
runtime limit and no automatic restart. Startup validation showed no missing
views; the existing `version()` volatility warning remains. Readiness still
returned HTTP 503. All 19 regular network tests passed, as did the separate cold
parity run, formatting and clippy; the full repository suite was not rerun for
these SQL-only changes.

The same stopped snapshot also matches all 18 selected fields for all 39 indexers
in `validation/backfill-123m-indexers-reference.json`. This initially exceeded
the ten-second query budget. The stake-lock projection now computes cumulative
stake/allocation sums with windows and recurses only over lock-changing events;
a differential test matches all 600 intermediate rows against the original fold,
including zero amounts, large integers, same-block ordering and legacy/Horizon
lock transitions. Capacity calculations share one materialized refresh input.
The combined indexer query then passed in 8.003 seconds, with the expanded
four-checkpoint test taking 29.86 seconds. These are single-run local timings,
not sustained capacity measurements. The final changes in `45-indexer.sql` and
`46-capacity.sql` remain local; install them at the next clean stop rather than
interrupting another in-flight ingestion window. The private unit reported fetch
progress through 128976863 at 09:17:08 UTC and remains bounded to one hour from
its 09:01:10 UTC start. No public route or readiness policy was enabled.

The resume2 unit reached its scheduled stop at 10:01:10 UTC with graceful
shutdown, 151.1 MiB reported peak memory and 4,138,872 units reserved. The retained
snapshot holds 211,360 rows in 163 segments through block 129252459. This dense
hour advanced only about 600,000 blocks; early sparse replay speed is not a
whole-history estimate. The tested `45-indexer.sql` and `46-capacity.sql` changes
were then installed with matching hashes while stopped. Resume3 confirmed
resumption from 129252460 at 10:46:23 UTC, with the same spending/memory limits,
one-hour runtime limit, no automatic restart and HTTP 503 readiness.

The larger snapshot matches all 35 network fields and 13 fields across 279 epochs,
plus ten fields for all 376 deployments. The epoch query initially exceeded ten
seconds because the delegation fold traversed 19,743 events recursively. A new
local projection accumulates additive movements with windows and recurses only
over reward events, preserving the pre-event pool balance used for each reward.
All 600 synthetic intermediate states match the original sequential fold, and
the historical epoch query passes within the unchanged limit. This newest
`41-delegation.sql` change is not installed in resume3; install at the next clean
stop. See `validation/backfill-129m-check.json` for exact evidence and remaining
limitations. The indexer checkpoint is still 123615752, not the newer block.
The expanded cold replay passed in 40.22 seconds, with the 129m epoch query at
4.035 seconds and the deployment query at 0.65 seconds in that local run. The
full integration suite passed 369 tests with seven deliberately ignored; all
21 regular network tests, all 21 captured HTTP client documents, clippy and
formatting also passed. The HTTP document check still uses the sparse genesis
fixture and does not stand in for actual running agent/service/TAP processes.

All 21 captured client documents also bind and execute through the HTTP handler
against the genesis fixture. Most entities are absent at that checkpoint: this
checks the query contract, not populated current-head responses or actual client
process behaviour.

Resume3 was stopped cleanly at 11:28:58 UTC after sealing through block
129940417. The replacement binary was built separately on the ThinkPad with
Rust 1.95.0 and has SHA-256
`66cc72e802ddf21ae9f6105758b5715d6cfcd1f5e918d0d4e64f474b13812695`.
The reward-sparse delegation projection installed at the same stop has SHA-256
`96c99cdb06cad6b5911ad010d156b9931dfeaac8486c2c5c238f7abb38e36d44`.
Resume4 started at 11:29:49 UTC with the same one-hour, 2 GiB and no-restart
limits and resumed at 129940418. Its first dense window advanced about 56,000
blocks in 19 seconds, reporting 112-116 events per second, versus 33 events per
second for resume3. This is an early throughput observation, not an end-to-end
completion estimate. The isolated Caddy candidate also validated successfully
on the VPS, and `network.thenightswatch.dev` resolved to its intended address;
neither fact enables the public route.

On 2026-09-19 the operator-approved RPC ceiling was raised to 285,000,000 units
(the conservative US$150 allowance). The budget ledger migration preserves its
existing reserved balance and refuses method-price changes or allowance
reductions. The rebuilt binary passed the migration on startup. Resume6 is now
running on the ThinkPad with a 12-hour maximum runtime, 2 GiB memory cap and
no automatic restart, resuming from block 130632196. The native API remains
private on `127.0.0.1:8124` while the public Caddy route continues to serve the
gateway-backed endpoint.

Set `NUTHATCH_RPC_BUDGET` to a durable, operator-owned copy of
`rpc-budget.toml.example` renamed with a `.toml` extension. The optional guard
applies to all `RpcClient` pools in the process, including archive reads,
batches and failover attempts. It reserves the entire request cost in a durable
redb transaction before sending; failed requests are not refunded. Redirects and
implicit HTTP retries are disabled so they cannot bypass the accounting boundary.
Unknown methods, missing or truncated ledgers, missing accounting records and
changed policies fail closed. An existing ledger is never recreated as an empty
allowance. A second process
cannot open the same ledger concurrently. Keep its adjacent `.redb` and
`.budget-initialized` files across restarts and deployments. Do not reset or raise
the allowance without Chief's approval. This guard does not control other software
using the same key or establish an Alchemy account spending cap.

Before enabling Caddy:

1. Complete full-history replay, with no unavailable calls or missing source facts.
2. Pass the actual agent/service/TAP documents against the example schema over HTTP.
3. Compare every supported root at identical blocks against the grafted deployment
   `QmR8WQECdNR6TSUf4FfLSFW7D5RDGGkd7F4A6me56pLqJb`. The fresh rebuild
   `QmatH4cd25ymsfCFpgKPRQQ7uCYNgarxanVQ9WCfg4Y7TZ` was behind on
   2026-09-17 but had passed block 506 million during checks on 2026-09-18.
   Check its actual `_meta` before choosing a comparison block; deployment
   identity alone does not establish freshness.
4. Exercise reorgs, historical pagination, stale head, lost RPC and lost ThinkPad
   connectivity. Missing data must be a refusal, never an empty healthy ledger.
5. Measure tip distance, data age, query p50/p99 and peak RSS under replay and
   representative concurrent client polling. Keep the per-cursor 2 GiB budget.
6. Choose limits from that measurement. Apply both per-IP and global limits, test
   429 responses and retry behaviour, and allow legitimate 1,000-row pagination
   bursts. An IP may represent several indexers; one request per minute is not an
   adequate client budget.
7. Validate a candidate Caddy configuration before reloading. Preserve all existing
   virtual hosts. Restrict the backend listener to Tailscale and verify the only
   public data path is GraphQL. Publish indexed block/hash/time and lag separately.

The final hostname, measured rate limits and recovery procedure must be recorded
when deployed. A configured URL is not acceptance evidence.

`deploy/Caddyfile.candidate` is a standalone configuration for validation, not a
replacement for the VPS's Caddyfile. It exposes GraphQL only on `POST /graphql`,
a non-query explanatory response on `GET /graphql`, and `GET /status`, with the
latter mapping to `/graph/status` on the nest. Port 8124 was unused during
inspection, but must be checked again before binding. Its rate limits are
provisional until the capacity measurements above pass.

## Durable backfill supervision

The prior 12-hour bounded replay stopped cleanly at its configured runtime limit
on 2026-09-20 after sealing through block 216003579. That was a safety stop, not
an RPC, data, or budget failure. The resumable replay is now supervised by the
boot-enabled system service in `deploy/network-facade-backfill.service`. It has
no runtime cutoff, restarts only after failure, and must use the existing operator
state directory unchanged. In particular, do not recreate `budget.redb` or the
sentinel beside it. `systemctl status network-facade-backfill.service` and
`journalctl -u network-facade-backfill.service` are the operational checks.

This service is still loopback-only and does not authorise switching Caddy away
from the gateway-backed public route. It merely prevents a completed history
replay from being delayed by an avoidable process lifetime.
