# Lodestar migration inventory

**Issue #1074.** The denominator for "how far through the Lodestar migration are we".

Produced 2026-09-01 against `~/Projects/lodestar` at `ee78088`, and `~/Projects/graph-allocations-nest`.

## What is settled here, and what is not

**Settled, by reading the code:** which files reach a gateway, *which* subgraph each one reaches,
what entities they ask for, what views exist, and what the nest declares. Every count below is from
a command that can be re-run, and the commands are quoted.

**Not settled here:** whether a given view actually *answers* a given route's query. That is a
per-field claim and it is #1076's job, at a pinned block. Nothing in this file should be read as
parity evidence. Where a row says a view exists, it means a file with that name exists in
`graph-allocations-nest/views/`, which is a weaker statement than it looks.

## 1. There are six subgraphs, not one

This is the finding that changes the shape of the problem. `src/lib/subgraph.ts` presents one module
and fronts **six distinct gateway subgraphs**, each with its own remit answer:

| client fn | subgraph | what it is | consumers |
| --- | --- | --- | ---: |
| `subgraphQuery` | `DZz4…qmp` | **Graph Network on Arbitrum One.** The migration target. | **37** |
| `ensQuery` | `5XqP…GtH` | ENS names on Ethereum **mainnet**, reverse resolution | 5 |
| `delegationEventsQuery` | `4LLz…UWds` | Paolo Diomede's discrete delegation/undelegation events | 3 |
| `qosOracleQuery` | `CnfJ…kbN3` | Ellipfra's fork of the Gateway QoS Oracle | 2 |
| `horizonPerfQuery` | `eD1T…8Y4h` | community Horizon indexer-performance timeseries | 1 |
| `dispatchRegistryQuery` | `6qhp…iJz1` | Dispatch JSON-RPC indexer registry | **0** |

Two things follow immediately.

**`dispatchRegistryQuery` has no consumers.** It is exported, it builds a gateway URL from
`GRAPH_API_KEY`, and nothing in `src` or `scripts` calls it. It is dead client code inflating every
gateway-reference count anyone has quoted.

**"Migrate off the subgraph" was never one job.** ENS is a different chain. The QoS oracle is a
different data category. The delegation-events subgraph is a third party's. Each needs its own
answer, and #1078's framing of a single `src/lib/subgraph.ts` seam quietly assumed one.

## 2. The real size

```sh
rg -l "subgraphQuery|ensQuery|horizonPerfQuery|delegationEventsQuery|qosOracleQuery|GRAPH_API_KEY" \
  src scripts --glob '*.ts' --glob '*.tsx' | grep -v __tests__ | grep -v 'lib/subgraph.ts'
```

**56 non-test files** touch a gateway. Fourteen of them never import the shared client and reach
`GRAPH_API_KEY` directly, which is why every sweep anchored on the client has undercounted.

For comparison: #1078's surface table names **7**. #1086 adds **8**. #1079 quotes "37 files touching
the gateway" and calls that figure alarming and misleading. The alarming-and-misleading figure was
itself low by 19.

**Already on nuthatch: 11 files.** `api/sql/{query,named,catalog,receipt}`, `api/delegation-events`,
`api/delegation-flows`, `api/developer-activity`, `api/dips`, `cron/check-dips`,
`lib/notifications/dips.ts`, and `lib/nuthatch.ts` itself.

**No file uses both.** The intersection of the gateway set and the nuthatch set is empty. There is no
dual-source surface anywhere in Lodestar, which is the mechanical reason #1080 matters: every switch
performed so far has been a replacement, not a fallback.

## 3. Group C: the migration

Network state from the Graph Network subgraph. This is the numerator and the denominator both.

| surface | entities read | on-chain | nest status | blocker |
| --- | --- | --- | --- | --- |
| `api/network-stats` | `graphNetwork`, `block` | yes | `80-lodestar-network.sql` | nest undeployed (#1075) |
| `api/grt-flow` | `graphNetwork` | yes | `80-lodestar-network.sql` | nest undeployed |
| `lib/ingest/network-snapshot.ts` | `graphNetwork` | yes | `80-lodestar-network.sql` | nest undeployed |
| `api/epochs` | `epoches` | yes | `50-lodestar-epochs.sql` | nest undeployed |
| `api/token-metrics` | `epoches` | yes | `50-lodestar-epochs.sql` | nest undeployed |
| `lib/ingest/epochs.ts` | `epoches` | yes | `50-lodestar-epochs.sql` | nest undeployed |
| `lib/ingest/allocations.ts` | `allocations` | yes | `40-lodestar-allocations.sql` | nest undeployed |
| `api/poi` | `allocations`, `indexer` | yes | `40-lodestar-allocations.sql` | nest undeployed |
| `cron/tap-provision` | `allocations` | yes | `40-lodestar-allocations.sql` | nest undeployed |
| `lib/ingest/disputes.ts` | `disputes`, `fisherman` | yes | `60-lodestar-disputes.sql` | nest undeployed |
| `lib/ingest/rav.ts` | `paymentsEscrowTransactions` | yes | `70-lodestar-escrow.sql` | nest undeployed |
| `api/payments` | `paymentsEscrow*`, `graphTallyTokensCollecteds` | yes | `70-lodestar-escrow.sql` | nest undeployed |
| `api/indexers` | `indexers`, `account` | yes | **no view** | view + nest |
| `api/indexer-stake-history/[address]` | staking history | yes | **no view** | view + nest |
| `api/rewards-history` | `indexer`, `stakes`, `delegator` | yes | **no view** | view + nest (#1082) |
| `api/curators` | `curators` | yes | **no view** | view + nest |
| `lib/ingest/delegations.ts` | `delegationEvents` (3rd-party sg) | yes | **no view** | view + nest (#1082) |
| `cron/ingest-horizon-activity` | `delegationEvents`, `provisions` | yes | **no view** | view + nest (#1084) |
| `api/provisions` | `provisions` + ENS | **mixed** | **no view** | split first |
| `api/indexer/[address]` | indexer + delegators + metadata | **mixed** | partial | split first |
| `api/portfolio` | delegator/stakes/signals + ENS | **mixed** | partial | split first |
| `api/apr-provenance/[address]` | `indexer` + ENS | **mixed** | partial | split first |
| `api/subgraph` | the shared proxy route | yes | n/a | the seam itself |
| `api/feed` | network activity | yes | **no view** | unclassified, needs a read |
| `cron/check-conversions` | direct `GRAPH_API_KEY` | ? | **no view** | unclassified, needs a read |
| `lib/protocols/fetcher.ts` | direct `GRAPH_API_KEY` | ? | **no view** | unclassified, needs a read |
| `lib/refresh.ts` | 11 entities across 3 subgraphs | **mixed** | partial | split first |
| `scripts/backfill.ts`, `backfill-rav.ts` | historical loads | yes | follows the above | nest undeployed |
| `app/indexers/[address]/opengraph-image.tsx` | `account`, `indexer` | yes | **no view** | view + nest |

**Twenty-nine surfaces.** Twelve are covered by an existing view and blocked only on #1075. Nine
need a view written. Five are mixed and need splitting before they can be classified at all. Three
are unclassified and need somebody to read them.

So the honest figure today, against group C, is **0 of 29 migrated, with 12 unblockable by anything
except deploying the nest.** Not a percentage anyone should publish yet, because the five mixed rows
can move the denominator in either direction once split.

## 4. Group B: chain-derived hash, off-chain meaning

On chain you get an IPFS hash. The name, the schema and the manifest behind it do not exist on chain,
and non-negotiable 1 forbids IPFS at runtime, deliberately.

`api/subgraph-names`, `api/subgraph-search`, `api/subgraph-versions/[hash]`,
`api/subgraph-deployments`, `api/subgraph-history/[hash]`, `api/subgraph-curation/[hash]`,
`api/subgraph-fees-30d`, `lib/disassembly/signal.ts`, `lib/disassembly/source-hint.ts`,
`app/subgraphs/[hash]/opengraph-image.tsx`, `api/ens`.

**Eleven surfaces, and most are mixed rather than out of remit.** `subgraph-curation` and
`subgraph-fees-30d` read `curatorSignals` and `indexerAllocations`, which are on chain and in the
nest's declared `curation` contract; only the display name beside them is not. #1079 lists group B as
two files. It is eleven, and the interesting half of each is migratable.

## 5. Group A: the gateway is the subject

Correct as they are. A subgraph playground with no subgraph is a deleted feature.

`api/subgraph-playground/[hash]`, `api/x402/query`, `api/studio/query/[id]`, `api/bounty-query/[id]`,
`api/gateway/[key]`, `lib/gateway-probe.ts`, `components/SubgraphGraphiQL.tsx`,
`api/indexer-status/[address]`, `api/indexing-status/[hash]`, `cron/check-subgraph-health`,
`api/indexer-trends`.

**Eleven surfaces**, one more than #1079's seven, and it is a decision rather than an oversight in
two of them: `indexer-status` and `indexing-status` query indexers' own `/status` endpoints, which is
serving telemetry, not chain state.

### The QoS question, which #1083 gets backwards

#1083 argues `qos.ts` is out of remit because quality-of-service "never existed on chain". Read, it
does not query the gateway's telemetry API at all. It queries **a subgraph of the Gateway QoS
Oracle** (`allocationDailyDataPoints`, `queryDailyDataPoints`), and an oracle subgraph exists because
an oracle **posts to chain**.

So the data is on chain by construction, and the real question is not remit but **shape**: whether
the oracle's payload arrives as decodable events or as opaque calldata, because nuthatch decodes
topic0-keyed events and nothing else. That is a different question with a different answer, and it
wants checking against the oracle contract rather than reasoning about.

Affected: `lib/ingest/qos.ts`, `api/indexer-qos/[address]`, `scripts/backfill-qos.ts`.

## 6. The crons, which are the live dependency

`vercel.json` schedules **19 crons**. Eleven are gateway-backed:

| cron | every | source |
| --- | --- | --- |
| `ingest-horizon-activity` | **2 min** | Graph Network + delegation events |
| `refresh` | 5 min | three subgraphs |
| `snapshot-network` | 5 min | Graph Network |
| `tap-provision` | 5 min | Graph Network |
| `ingest-epochs` | 10 min | Graph Network |
| `ingest-delegations` | 15 min | delegation events |
| `check-subgraph-health` | 15 min | Graph Network (group A) |
| `ingest-allocations` | 1 h | Graph Network |
| `ingest-rav` | 1 h | Graph Network |
| `ingest-disputes` | 6 h | Graph Network |
| `ingest-qos` | 6 h | QoS oracle |

**Something needs settling here.** #1081 records that `scripts/cron-runner.ts` runs on the droplet
under system cron, and `vercel.json` schedules the same ingestion as Vercel crons. Either both run,
or one is vestigial. Nobody has written down which, and it matters for #1081: you cannot hand over an
ingestion path you cannot name.

## 6a. The seam is not only the ingestion layer

`docs/audits/2026-09-plan.md` §6 states the architecture as:

```
subgraph APIs  ->  scripts/cron-runner.ts (droplet, system cron)  ->  Postgres  ->  Next.js UI
```

and concludes *"the UI mostly reads Postgres. So the migration seam is the ingestion layer, not the
API routes - a conclusion reached only after first writing the issue the other way round."*

**The first framing was the right one.** Counted:

```sh
rg -l "\bsubgraphQuery\b" src/app/api --glob '*.ts' | grep -v __tests__   # 27 routes
```

**21 of those 27 routes reference no database module at all** - no `@/lib/db` and no
`@/lib/studio/db`, the only two database import paths in `src/app/api`. The two lists partition the
27 exactly:

*No database module (21).* `cron/ingest-horizon-activity`, `curators`, `epochs`, `grt-flow`,
`indexer-stake-history/[address]`, `indexer-status/[address]`, `indexer/[address]`, `indexers`,
`indexing-status/[hash]`, `network-stats`, `payments`, `poi`, `portfolio`, `provisions`,
`subgraph-curation/[hash]`, `subgraph-deployments`, `subgraph-fees-30d`, `subgraph-history/[hash]`,
`subgraph-names`, `subgraph-search`, `subgraph-versions/[hash]`.

*Has a database module (6).* `apr-provenance/[address]`, `bounty-query/[id]`,
`cron/check-subgraph-health`, `cron/tap-provision`, `rewards-history`, `token-metrics`.

Spot-checked three of the 21 by hand: `network-stats:51`, `curators:44` and `epochs:19` each `await
subgraphQuery(...)` in the request handler with nothing between the user and the gateway.

**Split by group, because the 21 is not all migration work.** Two are group A and correctly stay
(`indexer-status`, `indexing-status`, which query indexers' own `/status` endpoints). Seven are the
group B `subgraph-*` metadata routes, whose on-chain half is migratable and whose display names are
not. **Twelve are group C network state**: `cron/ingest-horizon-activity`, `curators`, `epochs`,
`grt-flow`, `indexer-stake-history`, `indexer/[address]`, `indexers`, `network-stats`, `payments`,
`poi`, `portfolio`, `provisions`.

The consequence is concrete and it is the sprint's central question: **migrating the ingestion layer
alone leaves 21 request-time gateway dependencies standing, 12 of them network state.** A completion
claim resting on the nine `src/lib/ingest/*` modules would be a claim about the cron path only, and
every one of those routes would still 503 without `GRAPH_API_KEY`. That is the failure #1086
describes, and #1086's eight is an undercount against these twelve.

This is offered as the §6 deliverable the audit plan asks for - "find a migration-relevant thing with
no issue" - and it wants filing rather than leaving here.

## 7. What this changes about the open issues

- **#1074 is not board-only and needs no GraphOps conversation.** The repository is on this laptop.
  This file is the deliverable; what remains is reading the five mixed and three unclassified rows.
- **#1078's table is 7 rows of a 29-row group.** Its `src/lib/subgraph.ts` seam is real but is one of
  six clients, so a nuthatch-backed implementation behind it migrates the Graph Network surfaces and
  touches none of ENS, QoS or delegation events.
- **#1079 should cover 11 group-A surfaces and 11 group-B, not 7 and 2**, and should say for each
  mixed row which half stays.
- **#1082 is bigger than `delegations`.** Nine group-C surfaces have no view, not one.
- **#1083's premise needs replacing** with the shape question in §5.
- **#1086's eight routes are correct and incomplete**; the direct-`GRAPH_API_KEY` set adds fourteen
  more files it did not see, and §6a raises its 8 request-time routes to 12 group-C ones (21 in all).
- **The audit plan's own module table undercounts by construction.** It lists nine `src/lib/ingest/*`
  modules and their gateway-reference counts. Those nine are real, and they are 9 of 56.
- **#1080 has already been answered once, by accident.** `lib/nuthatch.ts` records that the panels
  migrated in 4.26.0 "need a configured Nuthatch origin and fail visibly without one, with no
  alternate Graph source". Fail-visibly is therefore the *existing* policy for migrated surfaces, not
  an open choice, and `NUTHATCH_DIPS`'s "no fallback exists" is that policy rather than an exception.
  #1080's real question is whether it survives contact with 29 more surfaces.

## 8. The denominator, stated

**"100% of Lodestar's on-chain network state, served by nuthatch nests"** = group C, 29 surfaces
today, minus whatever the five mixed rows shed on splitting.

Explicitly outside it, and finished by being correct: group A's 11, group B's off-chain half, ENS,
and the QoS oracle pending §5.

Explicitly **not** the goal: zero `GRAPH_API_KEY` in the repository. Per #638, the goal is that the
key is not load-bearing for Lodestar's own dashboard. Fifty-six files touch a gateway and roughly
twenty of them always will.
