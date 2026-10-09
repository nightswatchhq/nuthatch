# Subgraph stopgap S0: the BNB and Polygon sample

**Measured 2026-10-06 for [#1940](https://github.com/nuthatch-org/nuthatch/issues/1940).** Part of the
[subgraph stopgap](subgraph-stopgap.md) programme. Nothing here is fixed; every defect is listed for S1
([#1941](https://github.com/nuthatch-org/nuthatch/issues/1941)) with a CID that reproduces it.

## Verdict

**S0's failure condition fired, under both wordings of the gate.**

- The tracker's tightened gate fails if fewer than half the sample answer at least a third of the fields
  their port report calls exact. Through the path the stopgap offers, a deployment CID and nothing else,
  **0 of 21** do. Adding public mapping source by hand where it exists, **at most 2 of 21** do (amarok
  45%, morpho 37%), and both figures come from the coverage line, which overstates (defects 2 and 4).
- The original gate fails if fewer than a third of the sample yield a serving nest with any answered
  field. From a CID, **0 of 21**. With source added, **4 of 21** answered at least one collection over
  GraphQL, against the 7 the gate needs.

Read the cause before the number. Most of the failure is not the shape of the subgraphs: `port-emit`
classifies fields by reading AssemblyScript **source**, and IPFS holds only the compiled WASM. From a CID
alone every field except `@derivedFrom` is "no mapping writes this field", no view is emitted, and the
coverage line reports 100%. A schema parser panic, views emitted without an `id`, unwired template
factories and BSC endpoints that refuse the requests stop most of the rest. Whether that is worth S1, or
the offer narrows to S2 as the tracker says it does on this result, is Chief's call; the measurements below
are what it rests on.

## BetSwirl BNB Chain

The one sample deployment that is stranded and actually used: `Qmd5oqyojVx5wWSFuWfKz3YVLPHdE3KU5458Qqq3SVeGEB`,
`bsc`, Moderate, no indexer, 104 GRT of query fees in 30 days. Its Polygon twin
`QmUa6b7voVS4kuERGo3bEDvRsW2FdTogSLeztnvtsi5DB2` has three indexers; it was not queried, because the gateway
needs a key this box does not hold.

**What the tools did.**

- `init --from-subgraph` succeeded: 34 data sources (Dice, CoinToss, Roulette, Keno, WeightedGame,
  RussianRoulette, CoinTossBattle, PvPGamesStore, Bank, Freebet, Leaderboard, VRFCoordinatorV2_5, across
  their v1 to v5 ABIs), no templates, **150 event tables**, start block 16,689,819, 109.3M blocks to tip.
- `port-emit` from the CID: **58 exact, 465 unreachable, 0 call-derived, 0 fixed point.** All 58 exact are
  `@derivedFrom`; every other field reads "no mapping writes this field". No view was emitted, and the
  coverage line reads `58 of 58 ... (100%)`.
- **No mapping source is public.** The BetSwirl GitHub organisation has no subgraph repository; the
  subgraph source is not in the published `@betswirl/sdk-core` package either.
- **It cannot be smoke-indexed on any public BSC endpoint.** `bsc-rpc.publicnode.com` refuses an
  `eth_getLogs` naming 10 or more addresses (`-32602 Request blocked`; 9 is accepted), and BetSwirl names
  34. It also refuses anything 10,000 blocks back, about 75 minutes, with "Archive requests require a
  personal token". `1rpc.io/bnb` caps a range at 50 blocks and then rate-limited the nest within minutes.
  So the backfill has no public path at all; it needs a paid BSC archive endpoint.

**What a BetSwirl frontend asks for.** `@betswirl/sdk-core` 0.1.27, the published client, carries the
documents it sends (`src/data/subgraphs/protocol/documents/`): `bet`, `bets`, `token`, `tokens`, over two
fragments. Classified **by hand** below against the deployed schema's comments and the event ABIs, since
there is no mapping to cite. This is what a port could reach, not what one reaches today, which is nothing.

| SDK field (schema name) | source on chain | class, by hand |
| --- | --- | --- |
| `Bet.id` | PlaceBet `id`, scoped by game | exact, if the id format is recovered from the WASM |
| `gameId`, `gameAddress` | the emitting contract | exact |
| `user { id }` | PlaceBet `receiver` (v1 `user`) | exact |
| `gameToken { id }` | game and token | exact, same caveat as `Bet.id` |
| `gameToken.token { id }` | PlaceBet `token` | exact |
| `gameToken.token { symbol name decimals }` | ERC-20 reads | call-derived (`nuthatch metadata fetch`) |
| `affiliate { id }` | PlaceBet `affiliate` (v5) | exact |
| `inputValue`, `betCount`, `stopLoss`, `stopGain`, `chargedVRFFees` | PlaceBet parameters (`cap` and siblings) | exact |
| `betAmount` | PlaceBet `amount`, Roll `amount` on v1 games | exact, as a join |
| `houseEdge` | not in PlaceBet; latest `SetHouseEdge` / `SetAffiliateHouseEdge` | exact only as an as-of join; otherwise call-derived |
| `betTimestamp`, `betTxnHash` | the PlaceBet log | exact |
| `resolved`, `refunded` | a Roll or BetRefunded for the id exists | exact, as a join |
| `totalBetAmount`, `payout`, `rolled`, `rollTxnHash`, `rollTimestamp` | Roll | exact, as a join |
| `payoutMultiplier` | BigDecimal of payout over amount | arithmetic in graph-node's BigDecimal; exact only if its rounding is reproduced |
| `weightedGameBet { config { id multipliers weights } }` | WeightedGame config events | exact, as a join |
| `Token` counters: `betTxnCount`, `betCount`, `winTxnCount`, `userCount`, `totalWagered`, `totalPayout` | sums and counts over PlaceBet and Roll | exact as incremental entities, not views |
| `Token` splits: `dividendAmount`, `bankAmount`, `partnerAmount`, `affiliateAmount`, `treasuryAmount`, `teamAmount` | sums of `AllocateHouseEdgeAmount` | exact as incremental entities |

So the `Bets` query, the one a betting history page sends, is event-shaped almost throughout: of its 30
leaf fields, 23 are exact from events or joins of events, 3 are token metadata reads, and `houseEdge`,
`payoutMultiplier` and the two id formats are the risks. GraphQL fails the whole query on one refused
field, so it needs all of them or the frontend changes. Today it gets none, for three reasons in this
order: no mapping source, so no view (defect 1); 34 addresses, which the BSC preset refuses (defect 11);
and a 109M-block history no public BSC endpoint will serve.

## Method

- **Binary:** nuthatch 4.10.1 at `35f50b8`, release, toolchain 1.95.0. `init` and `port-emit` from the
  default build; serving from a second build with `--features graph`, because the GraphQL routes are
  registered only in a `graph` build (RFC-0060 §5.6). The box was otherwise quiet.
- **Sample:** 20 of the 275 deployments Lodestar's `/subgraphs/migration` list carries (kittiwake
  `/api/subgraph-directory`, `network` `bsc` or `matic`, signal above zero, `indexerCount` 0), read on
  2026-10-06, plus BetSwirl. Ten per chain, stratified by the directory's `complexity` field (6 Light, 13
  Moderate, 1 Extreme in the twenty) and by manifest shape: event-shaped, templates and factories, DeFi
  and pricing, block handlers, call handlers and `file/ipfs` data sources.
- **Per deployment:** `nuthatch init --from-subgraph <CID>`, then `nuthatch port-emit` against a directory
  holding the deployed `subgraph.yaml` and `schema.graphql` from IPFS. That is everything a CID gives.
- **With source:** six sample deployments have public repositories whose `schema.graphql` matches the
  deployed one (five exactly, PancakeSwap v3 at 82% of entity fields). For those, `port-emit` was rerun on
  the deployed manifest and schema plus the repository's mappings at its default branch, so the mapping
  may postdate the deployment.
- **Smoke:** nine nests run with `dev --backfill 3000` (Polygon) or `5000` (BSC) on the chain presets'
  endpoints, then every collection probed with `{ <collection>(first: 3) { id } }`. amarok was rerun over
  400,000 Polygon blocks to look for rows. No full backfills.
- **Tips** used for backfill sizes: BSC 126,017,154 and Polygon 95,043,486, at 07:10 UTC.

## Results

"CID only" is the stopgap's real input. "With source" is the upper bound if the developer hands over a
repository. Fields are the port report's own count (every field of every entity, interfaces included).
getLogs calls are blocks to tip over the chain preset's window (BSC 320, Polygon 40), then over a
10,000-block window an address-filtered paid archive endpoint would typically allow.

| deployment | CID | chain | complexity | init | CID only: fields answered | with source: exact answered (coverage line) and GraphQL | start block | blocks to tip | getLogs, preset / 10k window | manifest features |
| --- | --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | --- |
| BetSwirl BNB Chain | `Qmd5oqyojVx5wWSFuWfKz3YVLPHdE3KU5458Qqq3SVeGEB` | bsc | Moderate | ok, 34 contracts, 150 tables | 0 of 523 | no source found | 16,689,819 | 109.3M | 342k / 10.9k | 34 addresses |
| px-test | `QmXMFezqpyMKB4Am4qtfpYdPDptNcWtAUDzaVnk7cW9Hhx` | bsc | Light | ok, 1 contract, 7 tables | 0 of 88 | no source found | 67,766,676 | 58.3M | 182k / 5.8k | - |
| thena-blocks | `QmenTzjuovnYnptb6tMpwLPvzvyiH7zGtWUMxCJaHupXJF` | bsc | Moderate | ok, 1 contract, 16 tables | 0 of 14 | no source found | 24,468,802 | 101.5M | 317k / 10.2k | blockHandlers only |
| alpaca-lending | `QmR12MwzWReXLLR6LJuaWBqyW9SJ5ytnBBF1UbbQJemB9F` | bsc | Moderate | ok, 9 contracts, 36 tables | 0 of 326 | no source found | 5,213,456 | 120.8M | 378k / 12.1k | - |
| radiant-v2-bsc | `QmeE6PhjX7dBDVwGodAR5gZympgTMz4tBKWBiWgV15U7Co` | bsc | Moderate | ok, 3 contracts, 21 tables | 0 of 347 | no source found | 26,831,937 | 99.2M | 310k / 9.9k | templates 3 (3 wired) |
| pancakeswap-v3-bnb | `QmRvGv9ksce6ZWVfP2HCAnFgpKEgn4sqW65sQg4rjjDXBn` | bsc | Moderate | ok, 2 contracts, 13 tables | 0 of 335 | 68 of 210 (32%); 3 collections answer, then the backfill aborts | 26,931,961 | 99.1M | 310k / 9.9k | templates 1 (1 wired) |
| fwx-nft-bsc | `QmZgqshtkQxNdsfgnEnTYz5GkTkEu8uqtNqHbsN8ZEK33d` | bsc | Light | ok, 1 contract, 2 tables | 0 of 11 | no source found | 45,983,406 | 80.0M | 250k / 8.0k | - |
| usdt-transfer-bsc | `QmZ2wqvS51wAQTAqt27smiVLxM4t5FbB8BQgHrd4s34f7j` | bsc | Light | ok, 1 contract, 1 table | 0 of 7 | no source found | 49,720,267 | 76.3M | 238k / 7.6k | - |
| bsc-four-meme | `QmRdGx2Fbckf2gyiWm4am2eogzpnZPavm6thobLVK43Jm8` | bsc | Light | ok, 3 contracts, 7 tables | 0 of 92 | no source found | 78,771,556 | 47.2M | 148k / 4.7k | templates 3 (0 wired) |
| uncx-lockers-v2-bsc | `QmWvtJjV3R61PwZKeQ8wsHxvvB6fSjcoKLacBgqtT689AQ` | bsc | Extreme | ok, 1 contract, 5 tables | 0 of 150 | no source found | 6,878,262 | 119.1M | 372k / 11.9k | templates 1 (0 wired), callHandlers, blockHandlers |
| aspis-bsc | `QmXUw7WawTPd7NZ97AQkhQcygZXqNHd6PmNQrmDnT8eCAn` | bsc | Moderate | ok, 2 contracts, 11 tables | 0 of 155 | no source found | 34,288,803 | 91.7M | 287k / 9.2k | templates 2 (2 wired), callHandlers, file/ipfs |
| peeranha | `QmW4Vo3ZYV79pizzYKbNZ2TTKfHQhdRQDrmGDG25UkBpuz` | matic | Moderate | ok, 5 contracts, 33 tables | 0 of 200 | 13 of 192 (6%); 4 collections answer | 29,595,889 | 65.4M | 1,636k / 6.5k | - |
| quickswap-v4-polygon | `QmUnydNYC6Xe33R5fv6wK2mJbDVTLRbN5Kvk824fQShnM4` | matic | Light | ok, 2 contracts, 19 tables | 0 of 354 | no source found | 85,606,804 | 9.4M | 236k / 0.9k | templates 1 (0 wired) |
| polygon-blocks | `QmdNFXbQooUNuy2UQGciY5Lzb3LgoKegfkn96Le6gio78p` | matic | Moderate | ok, 4 contracts, 17 tables | 0 of 87 | no source found | 48,593,998 | 46.4M | 1,161k / 4.6k | blockHandlers |
| neet-raffle | `QmUGAT4xr5nkUdFBHXLy5jt3EsSquBXpJB4arVTBtAgd5w` | matic | Light | ok, 1 contract, 22 tables | 0 of 136 | no source found | 58,521,799 | 36.5M | 913k / 3.7k | - |
| boost-polygon | `QmejhEiHCiGjKJ9Q5RfT5jeuUCWAACs4vV24RESciKvsQ4` | matic | Moderate | ok, 1 contract, 4 tables | 0 of 49 | no source found | 54,651,164 | 40.4M | 1,010k / 4.0k | file/ipfs |
| bunni-polygon | `QmSZenEn6B68ChiQt383U9KVnnU6CgLeanA5jMdtDGjYLw` | matic | Moderate | ok, 1 contract, 9 tables | 0 of 68 | 3 of 60 (5%); 0 collections answer | 34,317,600 | 60.7M | 1,518k / 6.1k | templates 1 (1 wired) |
| streamr | `QmZW3LdGcqVvHPq6ZCPmtFHFfzDGB3XT1ziSCxYGMZA13Q` | matic | Moderate | ok, 9 contracts, 48 tables | 0 of 257 | 80 of 246 (32%); every GraphQL request panics | 23,562,860 | 71.5M | 1,787k / 7.1k | templates 2 (0 wired) |
| amarok-polygon | `QmTXk9saJgRiG3hGdRgcdZZsanqDUQFPwhDfuiXnvWUZzw` | matic | Moderate | ok, 3 contracts, 29 tables | 0 of 196 | 86 of 191 (45%); 12 collections answer | 37,100,519 | 57.9M | 1,449k / 5.8k | - |
| morpho-blue-polygon | `Qmc6ayfgHsFFdxMj36DV67vQurLV9LsN9B3jmSMqr82c6m` | matic | Moderate | ok, 3 contracts, 61 tables | 0 of 792 | 141 of 380 (37%); 1 collection answers | 66,931,042 | 28.1M | 703k / 2.8k | templates 3 (1 wired), callHandlers |
| fm-polygon-test | `QmbgygZKJeiK1BMaeXZ4MozzsgcoLbFyXtMUy4DPjgt5Wg` | matic | Moderate | ok, 2 contracts, 11 tables | 0 of 48 | no source found | 57,835,065 | 37.2M | 930k / 3.7k | callHandlers |

Repositories used for "with source": `pancakeswap/pancake-subgraph` (`subgraphs/exchange-v3/template`),
`morpho-org/morpho-blue-subgraph`, `Bunniapp/bunni-subgraph`, `peeranha/peeranha-subgraph`,
`streamr-dev/network-contracts` (`packages/network-subgraphs`), `connext/monorepo`
(`packages/deployments/subgraph/src/amarok-runtime`). "Collections answer" counts entity collections
that returned data rather than an error. Every answer was an empty list: the smoke windows held no events
for these contracts, and amarok's 400,000-block rerun found none either. No row was served by any nest.

## What the sample shows

**init is not the problem.** It succeeded on all 21, on both chains, including block handlers, call
handlers and `file/ipfs` sources, each of which it names in a warning. No manifest declared `calls:`
(declarative eth_calls). The weak spot is templates: 17 across 9 deployments, 8 wired to a factory by
`init`, 9 left with candidate lists, and `port-emit` wired none of the 9 even where the source holds the
`Template.create(event.params.x)` line that decides it.

**From a CID, nothing answers.** Every field is either `@derivedFrom` or "no mapping writes this field",
no view is emitted, and every collection in the three CID-only smoke nests (px-test, neet-raffle,
alpaca-lending) is refused with an engine "table does not exist" error, or the request panics. This
includes neet-raffle, whose 22 entities are all `graph init --from-contract` scaffolds: one immutable
entity per event, carrying the event's parameters plus `blockNumber`, `blockTimestamp` and
`transactionHash`. That shape is exactly what a nest reproduces, and it is recognisable from the schema
and the ABI with no mapping source at all. Across all 275 at-risk deployments, **29 (11%) are scaffolds in
every entity, 55 (20%) in at least half, and 71 (26%) in at least one** (fully: 9 of 114 on BSC, 20 of
161 on Polygon).

**Source is mostly not available.** Six of the twenty-one had a public repository. The rest are
Studio-only names (`px-test`, `test-chequedev`, `bsc_four_meme`) with nothing to find, and BetSwirl, the
one that matters most, publishes its client but not its subgraph. Where source exists the classifier and
emitter reach 5% to 45% of the fields they call exact, in line with RFC-0053's measured outcome, and the
shape of the subgraph decides it: amarok (event records) 45%, morpho (a Messari SDK lending schema) 37% of
exact but 141 of 792 overall.

**Backfill cost is dominated by access, not request count.** Start blocks are a median of about 96M
blocks back on BSC and 43M on Polygon. At the shipped presets that is 150k to 380k getLogs calls per BSC
deployment and 240k to 1.8M per Polygon deployment, because `polygon.drpc.org` caps at 80 blocks. An
address-filtered paid archive endpoint at a 10,000-block window brings every one under 13k calls, which is
pennies at any per-request price. **On BSC the shipped preset is not a backfill endpoint at all any more:**
`bsc-rpc.publicnode.com` now serves about 75 minutes of history without a token, and no request naming
10 addresses or more. Every BSC deployment in this sample needs a paid archive endpoint before it indexes
one historical block. Two multipliers the request count hides:

- **Templates fetched topic0-only.** The morpho nest pulled 280,000 events in its first 340 Polygon
  blocks and stored none of them: no vault was created inside the window, so every log came from a
  contract that is not a known child. A template over a common event (`Transfer`, `Approval`, `Deposit`)
  fetches that event's volume for the whole chain.
- **`[[calls]]` at the row's block.** The emitted `[[calls]]` (3 for PancakeSwap, 2 for amarok) cost one
  archive `eth_call` per triggering row, so their cost is the event count, which S0 did not measure.

## Defects for S1

Each reproduces with the CID given on the default or `graph` build at `35f50b8`. None was fixed.

1. **port-emit from a CID classifies nothing.** With only the deployed manifest and schema (IPFS has
   compiled WASM, never `.ts`), every field becomes "no mapping writes this field" and no view is
   emitted. `port-emit` does not say it found no mapping source; it exits 0. Any CID above; neet-raffle
   `QmUGAT4xr5nkUdFBHXLy5jt3EsSquBXpJB4arVTBtAgd5w` is the starkest, 22 scaffold entities that map one to
   one onto its event tables, and BetSwirl `Qmd5oqyojVx5wWSFuWfKz3YVLPHdE3KU5458Qqq3SVeGEB` the costliest.
2. **The coverage line reports 100% for a nest that answers nothing.** Its denominator is the fields the
   report calls exact, and it counts every `@derivedFrom` field as answered even when the target entity has
   no view. px-test `QmXMFezqpyMKB4Am4qtfpYdPDptNcWtAUDzaVnk7cW9Hhx` prints `coverage: 1 of 1 ... (100%)`
   with 87 fields unreachable and no view. On morpho `Qmc6ayfgHsFFdxMj36DV67vQurLV9LsN9B3jmSMqr82c6m`,
   `{ accounts { deposits { id } } }` fails with `no table deposit` while the line counts all 75 reverse
   lookups as answered.
3. **GraphQL schema generation panics on descriptions containing a colon.** `src/graph_schema.rs:1242`,
   "the generated schema references types it does not declare", in a tokio worker; the connection closes
   with no response. String descriptions such as `"... e.g. http://mynode.com:3000"`,
   `"mapping (uint32 => ...)"` and `"Examples: AMM protocol fee"` are read as type references. streamr
   `QmZW3LdGcqVvHPq6ZCPmtFHFfzDGB3XT1ziSCxYGMZA13Q` (`["1", "3000", "mapping"]`), alpaca
   `QmR12MwzWReXLLR6LJuaWBqyW9SJ5ytnBBF1UbbQJemB9F` (`["AMM"]`), and morpho on introspection.
4. **Views emitted without an `id` column are unqueryable.** When the emitter cannot render the id
   expression (`event.transaction.hash.concat(Bytes.fromI32(...))`, a helper's return value), it still
   writes the view, and every query on that collection then fails with
   `Binder Error: Table "b" does not have a column named "id"`, whichever fields are asked for. The coverage
   line counts those views' fields as answered. Views with an `id`: PancakeSwap 3 of 10, morpho 3 of 18,
   streamr 6 of 13, amarok 12 of 18, peeranha 4 of 5. Morpho: `{ metaMorphoTransfers(first: 2) { hash } }`.
5. **Templates left unwired, and port-emit does not wire them from source.** 9 of 17 templates in the
   sample have no `[[factories]]` rule after `init`, so they index nothing. Where the mapping is present its
   `Template.create(event.params.x)` line names the creating event and parameter, and `port-emit` ignores
   it. streamr (`Operator`, `Sponsorship`, two candidates each), bsc-four-meme
   `QmRdGx2Fbckf2gyiWm4am2eogzpnZPavm6thobLVK43Jm8` (3), uncx `QmWvtJjV3R61PwZKeQ8wsHxvvB6fSjcoKLacBgqtT689AQ`,
   quickswap-v4 `QmUnydNYC6Xe33R5fv6wK2mJbDVTLRbN5Kvk824fQShnM4`, morpho (2 of 3).
6. **A factory nest aborts on the shipped public endpoints.** The template fetch goes out without an
   address list; `bsc-rpc.publicnode.com` and `polygon-bor-rpc.publicnode.com` refuse it with
   `-32701 Please specify an address`, and after 64 attempts at the same range the nest exits with
   "backfill made no progress". PancakeSwap `QmRvGv9ksce6ZWVfP2HCAnFgpKEgn4sqW65sQg4rjjDXBn` with
   `dev --backfill 5000` on the BSC preset; Bunni `QmSZenEn6B68ChiQt383U9KVnnU6CgLeanA5jMdtDGjYLw` on
   Polygon. `src/chains.rs` documents the BSC refusal as arriving past 500 children; here it arrived
   with none. Related: with `--backfill`, a factory whose creating events precede the window finds no
   children, so a recent-window smoke of a factory nest can never show template rows.
7. **The classifier misses writes it should see.** Three shapes, each turning a reachable field into "no
   decoded column" or "no mapping writes":
   - writes inside class methods and through compound assignment (`this._market.borrowCount += INT_ONE`,
     the Messari SDK pattern): 395 morpho fields read "no mapping writes this field";
   - an entity built in a `getOrCreate` helper and filled in a handler: Bunni's `Bribe.amount` is set from
     `event.params.amount` in `src/mappings/BunniBribe.ts:15`, but the report cites the helper's `ZERO_INT`
     and none of Bunni's 7 entities gets a view;
   - a contract read through a helper that returns a bound contract,
     `getPeeranhaNFT().getAchievementsNFTConfig(id)`, is classed **exact** (`Achievement.achievementURI`,
     `peeranha/src/achievement.ts:15`). It should be call-derived. Nothing is served wrong today only
     because no view could be emitted for it.
8. **`__schema { queryType { fields } }` returns `null`.** `__type(name: "Query") { fields }` answers, so
   this is a silent null rather than a refusal, on every graph-build nest; amarok shows it.
9. **A missing view is refused with an engine error, not by name.** Every collection without a view
   answers `no segments: Catalog Error: Table with name ... does not exist!` with a planner hint, rather
   than the per-field refusal the compatibility surface promises. Any CID-only nest.
10. **Block-handler-only sources index every ABI event instead.** thena-blocks
    `QmenTzjuovnYnptb6tMpwLPvzvyiH7zGtWUMxCJaHupXJF` is a one-entity block subgraph (number, timestamp,
    hash); `init` scaffolds 16 event tables for it with a warning, and nothing answers its `Block` entity.
    polygon-blocks `QmdNFXbQooUNuy2UQGciY5Lzb3LgoKegfkn96Le6gio78p` likewise.
11. **The BSC preset cannot index a nest with 10 or more contracts, or any history.** All contract
    addresses go into one `eth_getLogs`; `bsc-rpc.publicnode.com` answers 10 or more with
    `403 -32602 Request blocked`, which the nest reads as a credentials refusal, cools down for 300 s and
    then aborts. The same endpoint now refuses anything about 10,000 blocks back ("Archive requests
    require a personal token"), where `src/chains.rs` records the refusal at block 1,000,000, so the
    preset's own note that tip-following a static contract works now holds only for nests of nine
    contracts or fewer. BetSwirl
    `Qmd5oqyojVx5wWSFuWfKz3YVLPHdE3KU5458Qqq3SVeGEB`, 34 addresses, `dev --backfill 20000`.
12. **The adaptive window does not narrow on a provider's range cap.** `1rpc.io/bnb` answers
    `-32602 eth_getLogs is limited to 0 - 50 blocks range`, and the nest retries 320 blocks 64 times and
    exits; it needed `--window 50` by hand. PancakeSwap `QmRvGv9ksce6ZWVfP2HCAnFgpKEgn4sqW65sQg4rjjDXBn`
    with `--rpc https://1rpc.io/bnb`.

## What this does not measure

- How many of a deployment's real GraphQL **queries** a nest would answer, beyond BetSwirl's published
  client. Field coverage overstates that, as [the compatibility reference](graph-compatibility-what-it-is.md)
  says: one refused field fails the query.
- The orphan list. That is S2 ([#1942](https://github.com/nuthatch-org/nuthatch/issues/1942)); this
  sample was drawn from Lodestar's version of it, which is not S2's verified list.
- Correctness of any answered value. No nest served a row, and no sampled deployment has an indexer to
  diff against, which is the point of them. BetSwirl's Polygon twin is the one available reference, once
  someone with a gateway key queries it.
