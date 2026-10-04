# Potential users: where people need data we can give them

*Started 2026-10-04. Listings and posts tell people nuthatch exists; this page is about where it makes a
difference, which is wherever someone's data has just stopped. Every claim names its source and how far
it was checked: **verified** means we read the primary source ourselves on the date given, **reported**
means it came from research we have not yet re-checked.*

---

## The short version

Three groups have a real, current data problem that a nest solves, in this order of priority:

1. **Teams whose subgraphs were stranded** when Subgraph Studio dropped their chain.
2. **Protocols that are shutting down**, whose history goes dark with their API.
3. **Small analysts and teams who lost free data** when hosted options closed or went paid.

The approach for all three is the same: build the nest ourselves for a handful of real cases, check it
answers, and then contact those people one at a time. Never a mass issue-blast. Envio filed bulk "I
migrated your subgraph" issues across many repositories in January 2026 and it landed badly (reported);
we do not repeat that.

---

## 1. Stranded subgraphs (highest priority)

The Graph has been removing Subgraph Studio support chain by chain through 2026. Every subgraph on those
chains belongs to a team whose app or dashboard lost its data path, and they are findable: the
`subgraph.yaml` in a public repository names the network. nuthatch can scaffold a nest from a subgraph
manifest (`--from-subgraph`, with `port-report` saying what ports and what does not), and a nest needs
nothing from The Graph to run.

**Chains removed from Studio, from `graphprotocol/networks-registry` commit history:**

| Date | Chains | Check |
|---|---|---|
| 2026-04-13 | Abstract, Metis, Manta, Fraxtal, others | reported |
| 2026-05-21 | Berachain, Ink, MegaETH, ApeChain, Cronos | reported |
| 2026-05-28 | Katana, Ronin | reported |
| 2026-08-13 | Fantom, Monad, polygon-zkevm, Rootstock | **verified** 2026-10-04 |
| 2026-08-18 | Moonbeam, Moonriver, mbase | **verified** 2026-10-04 |

**At risk this week:** on **8 October 2026** the Studio staging environment is deprecated for BNB Smart
Chain and Polygon (The Graph Foundation, [forum topic 7084](https://forum.thegraph.com/t/bringing-subgraph-studio-traffic-to-the-graph-network/7084),
24 September, **verified**). Subgraphs that no indexer picks up on the network stop answering. A
Discord-sourced count of about 450 affected subgraphs is recorded in nightswatchhq/graph-support#45
(reported). A competing provider was already offering migrations at $75 per subgraph on 1 October
(reported).

**What a nest can and cannot give a stranded subgraph.** It reproduces everything taken straight from
events: ids, addresses, timestamps, parameters. It does not run AssemblyScript mappings, so running totals,
prices and other accumulated state have to be redeclared as SQL views or incremental entities. See
[graph-compatibility-what-it-is.md](graph-compatibility-what-it-is.md). Say this up front.

**How we approach it:**

1. Scan GitHub for active subgraphs on the stranded chains and rank them (results below).
2. Port the top three to nests ourselves and check they answer.
3. Contact those three teams individually, through the route their README gives.
4. Turn what we learn into one forum post for everyone else.
5. For BNB and Polygon, ask the Foundation first, so a fallback offer reads as complementing the
   network rather than competing with its migration. Asked publicly in the graphprotocol Discord
   `#subgraph-development` on 2026-10-04; no answer yet.

### Scan results

*Pending: the scan was started on 2026-10-04 and its ranked list will be added here.*

---

## 2. Protocols shutting down

When a protocol winds down, its history goes dark with its API, and nobody is paid to keep it. An open,
reproducible archive nest is a public good that nobody else is offering.

**Balancer** (first case). [BIP-928](https://forum.balancer.fi/t/bip-928-orderly-winddown-of-balancer-and-distribution-of-the-treasury/7107)
passed on 2026-09-29. Pools move to withdrawals only on **30 October 2026**, v3 pools that partners keep
stay live until **30 November**, and infrastructure steps down from 1 November. The proposal says
"whatever finds no operator stays open source, with documentation and archives published". No reply in
the thread's 23 posts offers a data archive (**verified** 2026-10-04). A fork is being organised in the
same forum, and its authors would also want the history.

Contracts, **verified** 2026-10-04 by `eth_getCode` and a Sourcify exact match named `Vault` on Ethereum,
Arbitrum One and Base:

- V2 Vault `0xBA12222222228d8Ba445958a75a0704d566BF2C8`
- V3 Vault `0xbA1333333333a1BA1108E8412f11850A5C319bA9`

Status: an archive nest is being scoped. The event counts and backfill cost per chain come first, because
V2 mainnet history since 2021 is too large for free public endpoints. The offer goes in the BIP-928 thread
once the nest exists.

Watch for the next ones. Any wind-down proposal is a candidate.

---

## 3. People who lost free data

- **Alchemy Subgraphs** shut down on 2025-12-08 and pointed users to Goldsky (Alchemy's docs, reported).
- **Dune's free plan**: accounts created before 21 July became view-only from 10 September (Crypto
  Briefing, 27 August, a secondary source; Dune's FAQ exists but was not read).
- **Flipside** sold its data business to SonarX, and its export ended on 17 June (reported).

The people affected are independent analysts and DAO contributors who now have no free SQL over their
own contracts. "Your contract's full history, as SQL, on your own machine, free" is exactly the offer.
Where those communities gather has not been established yet.

---

## Chains with an indexing gap

Goldsky and Envio cover nearly every chain, so the gap is never "no indexer at all". It is "nothing free,
self-hosted, or Graph-native". Ranked by research on 2026-10-04 (reported unless marked):

1. **Robinhood Chain.** Built into nuthatch. The registry lists Firehose/Substreams only, so Studio
   rejects deploys; a real user's deploy failed (nightswatchhq/graph-support#38). It has no docs listing
   page. Reach it through Arbitrum's channels.
2. **Arc (Circle).** Studio only. Its agent-first culture suits the MCP server. Discord is linked from
   docs.arc.io. Its native USDC emitter has no ABI to fetch, so one has to be supplied.
3. **Ink.** Studio dropped. Its docs list Alchemy Subgraphs, which has shut down (**verified**). nuthatch
   works there with `--chain ink --rpc <url>` and a supplied ABI (**verified** 2026-10-04 on 4.3.1).
   Listing PR: inkonchain/docs#696.
4. **Plasma.** Not in the registry at all. Its listing page has no public repo.
5. **Katana.** Studio dropped. Its docs repo merges outside how-tos, but TVL has fallen sharply.

Checked and set aside: **HyperEVM** (`eth_getLogs` capped at 50 blocks on the public RPC, reported),
**MegaETH** and **Berachain** (activity collapsing), **Tempo** (ships its own SQL indexer), **Abstract**
(outside docs PRs sit unmerged), and the Graph rewards-tier chains (no gap).

---

## Where to show up

| Place | What it is good for | State |
|---|---|---|
| graphprotocol Discord `#subgraph-development` | stranded subgraphs, the 8 October fallback | posted 2026-10-04 |
| graphprotocol Discord `#mcp-servers` | the MCP server, next to the Subgraph MCP | posted 2026-10-04 |
| Monad Developers Discord | node-operator channels only for members; rules say no promotional posts and not a support server | answer only, nothing posted |
| r/rust | architecture; This Week in Rust now takes project links only from r/rust | posted 2026-10-04 |
| users.rust-lang.org `#announcements` | project announcements, no karma gate | posted 2026-10-04 |
| r/selfhosted | the self-hosted audience | removed twice; note the AI-assisted flair |
| awesome-selfhosted | permanent listing | needs a first release more than 4 months old: eligible from **2026-11-14** |
| Show HN | launch | refused until the posting account has history |
| Apache DataFusion "Known Users" | engineering credibility | apache/datafusion#26025 |
| awesome-mcp-servers | MCP discovery | punkpeye/awesome-mcp-servers#15700 |
| chain.love | provider directory | Chain-Love/chain-love#4146 |

---

## Weak or unconfirmed

- **AI agents needing on-chain history.** The space is crowded (Envio, SQD, Bitquery and Dune all ship
  MCP servers) and no unanswered demand was found. The angle would be a free, self-hosted MCP server.
  Low confidence.
- **Grants.** Not pursued. nuthatch is self-funded and will not apply.
