# Launch and chain-expansion review - 2026-08-19

- Status: **opinion, not a decision.** A prioritisation memo, not an RFC. Supersedes nothing.
- Author: Pete (cargopete), with Jenny
- Date: 2026-08-19
- Reviews: an external research report on launch strategy and chain expansion, checked against the
  repo at `6196b08` (`v2.5.0`)
- Related: [RFC-0007](../rfcs/0007-launch-and-validation.md) (launch and validation),
  [show-hn.md](show-hn.md), [home-turf.md](home-turf.md), [community.md](community.md),
  [docs/grants](../grants), [benchmarks.md](../benchmarks.md)
- Revised: 2026-08-19 - port loop to P0, NLnet to the back burner (§1)

## Why this document exists

A research report landed recommending (a) a materially rewritten Show HN, and (b) Solana as a
post-launch chain beachhead. Checked against the tree, one of its load-bearing premises is false and
several of its edits argue with a draft the repo has already moved past. This memo records what
survives the check, what does not, and the order I would actually do things in.

**House rule applied throughout:** claims about this repo are grepped, not recalled. Claims about the
outside world are marked verified or unverified, and the unverified ones are not allowed to carry a
deadline.

---

## 1. The priority stack

Revised twice on 2026-08-19. The second revision is the important one: **three of the candidate items
turned out to be the same job**, which is why this table is now short.

### The collapse

The port-queue nest ([port-queue-nest.md](port-queue-nest.md)) needs `SubgraphService` (allocations)
and `L2Curation` (signal). Lodestar's `cron/ingest-allocations` and `curators` routes need
`SubgraphService` and `L2Curation`. **Same two contracts, same chain, two consumers.**

And the port loop already has its first subject. Lodestar is a real user with a real problem, running
nuthatch in production, with 39 gateway-dependent routes and a documented list of what is missing
(RFC-0011 status update, 2026-08-19). There is no need to find a stranger's subgraph to prove the loop
when the best design partner available is already here and the port is half-specified.

### Therefore, one next thing

> **Confirm `SubgraphService`, then add it and `L2Curation` to the nests already running.**

It pays out four ways simultaneously:

1. **Lodestar loses its heaviest gateway dependency.** The six `cron/ingest-*` routes hold 53 more
   hostage through `@/lib/db`; killing that group releases all of them.
2. **The port queue exists as a by-product**, not as a separate project.
3. **It is a genuine port**, so it is validation-conversation evidence and a forum post.
4. **It measures which capability is actually missing**, instead of us guessing.

Point 4 dissolves the IPFS-versus-`eth_call` question rather than answering it. Current evidence: IPFS
blocks **two routes out of 39** (`subgraph-names`, `subgraph-search`), which is real but small. If the
RFC-0023 tier-3 executor turns out to block eight, it wins on a count rather than on anybody's
intuition. RFC-0037 and RFC-0023 both exist and neither expires; they can wait a fortnight for a
number.

| # | Item | When | Why it sits here |
|---|------|------|------------------|
| **P0** | Confirm `SubgraphService`, add it + `L2Curation`, migrate the `cron/ingest-*` group | now | The collapse above. One build, four payoffs. |
| **P1** | The remaining on-chain Lodestar routes | after P0 | ~27 of 39 are plain event data and fall individually behind the per-panel flags that already exist. |
| **P2** | Show HN copy pass, then post | after P1 | The version line is fixed; the honest-limits paragraph is fixed. The headline it earns after P1 - a production dashboard serving its on-chain panels with no gateway dependency - is far better than anything currently drafted. |
| **P3** | IPFS (RFC-0037) or tier-3 executor (RFC-0023) | whichever P0/P1 proves blocking | Deliberately unresolved. See point 4. |
| **P4** | EF ESP against a named wishlist item | after the post | Now the lead grant, and gated on having something to point at - which P0 and P1 produce. |
| **P5** | HyperEVM, compliance positioning note | opportunistic | Neither has a user asking. |
| **-** | **NLnet** | **back burner** (decision, 2026-08-19) | Window verified (3 Sept - 3 Nov 2026, 12:00 CEST), [nlnet.md](../grants/nlnet.md) drafted at €38,400. Recorded, not scheduled. The P0/P1 work is exactly the evidence it would want if it comes forward. |
| **-** | **Solana** | **not now** | See §2. |

**Done since this document was first written:** the subgraph-fallback forum post is live on The Graph
forum (2026-08-17), so RFC-0007 Phase 1 has started.

**What changed and why.** Revision 1 put NLnet at P0 because it was the only item with a verified
external deadline. That was the wrong reason: a deadline makes a thing *urgent*, not *load-bearing*.
Revision 2 collapsed the port loop, the port-queue nest and the Lodestar migration into one job, once
counting Lodestar showed that its heaviest dependency and the port queue want the same two contracts.
Both revisions moved the same direction: toward the work that produces evidence, and away from the
work that consumes it.

---

## 2. The correction: Solana's cost estimate rests on something that isn't here

The report rates Solana's engineering cost **medium**, on this reasoning:

> Since Pete is *already* building Substreams/Firehose ingestion, Solana can ride largely on that
> pipeline rather than requiring a bespoke Geyser/Yellowstone integration from scratch.

There is no Substreams ingestion in this repo. `grep -rin 'solana' --include='*.rs' src tests` returns
nothing at all, and there is no Firehose client, gRPC ingestion source, or Substreams package
anywhere in the tree.

What the report found is
[RFC-0014](../rfcs/0014-firehose-class-extraction-traces-and-state.md), a naming collision. That RFC
is about extracting **traces and state diffs from a colocated reth via ExEx**. It is not about
Firehose the protocol, it does not consume a Firehose stream, and its own header says:

> Priority: **deferred.** Gated on RFC-0003 actually landing (ExEx wired to a real node). Not before
> the pilot, not before ExEx is live.

RFC-0003 has not landed. So the document cited as evidence that Firehose ingestion is underway is a
deferred design sketch for deliberately not building it. Remove that premise and Solana's cost is a
new ingestion source, a new data model (accounts, not logs), Borsh/Anchor IDL decoding, and fork
semantics that are not reorg semantics. For a sole maintainer that is the autumn, not a slice.

### It also fails the brief, not just the budget

The tractable Solana path is consuming a hosted Yellowstone gRPC endpoint or a hosted Substreams
endpoint. That is a **mandatory third-party data dependency**, which is
[non-negotiable #1](../../CLAUDE.md) gone, and a gated data service, which is #3 gone. Self-hosting a
Solana Firehose is a multi-terabyte, multi-machine proposition, which is precisely the burden nuthatch
exists so that nobody has to carry.

The report concedes this in its own caveats and recommends Solana anyway. Its own stated abort
condition -

> if Solana support can't reach parity with the EVM core's "single binary, no external services"
> promise (e.g. if it hard-requires an external hosted Yellowstone gRPC), delay the Solana Show HN

\- appears to be met *before the first line is written*, not after.

### And EVM is not airtight yet

CLAUDE.md puts non-EVM chains behind "EVM is airtight" for a reason. Current state, from the tree:

- Three chains with bundled defaults: `mainnet`, `arbitrum-one`, `base` (`src/chains.rs`). Anything
  else needs `--rpc`.
- Events only. No call or trace decoding.
- RPC polling. The reth ExEx path (RFC-0003) is designed and stubbed, not shipped.
- No GraphQL layer.

That list is the definition of not-airtight, and it is the same list `show-hn.md` already prints under
"Honest limits". Adding a second execution model on top of it buys reach at the cost of the one claim
the whole project rests on.

### What Solana is still good for

A **roadmap line**. It is free, it is honest, and it does real work in the Show HN thread by answering
"is this only ever going to be Ethereum" without committing an autumn. If a funded design partner
appears, or if a plain-RPC self-hosted path turns out to be viable, revisit with a measurement rather
than a thesis.

---

## 3. What to expand instead: HyperEVM

The report ranked HyperEVM second while making the case for first. Against Solana it is:

- **EVM**, so it is a `chains.rs` entry, endpoint measurement against the RFC-0030 §4 bar, and a
  regression run. Days, not months.
- The **same narrative**: underserved, fast-growing, and concentrated around one dominant hosted
  provider. That is the story the launch wants, and it is true here without an architecture change.
- **Non-negotiable-safe**: raw block data via a local node plus a documented public S3 bucket is
  exactly the self-hostable shape we require. *(Unverified - check the bucket and a public endpoint
  against the §4 bar before committing to it.)*

Rule for any chain we add: it ships with keyless public endpoints that pass the endpoint bar, or it
does not ship. A chain that requires somebody's API key is a chain that phones home.

---

## 4. Launch copy: what the report got right, and the one thing I'd reject

### Already done

The report's largest single recommendation is to delete the "why AGPL" section. `show-hn.md` already
says `MIT OR Apache-2.0`; the surgery happened at relicence. Half the copy edits are arguing with a
draft from before 2026-07-28.

### The actual defect

`show-hn.md` says **"It's v0.1.0 and solo-maintained."** `Cargo.toml` says `2.5.0` and there are nine
tags. In a post whose credibility rests on production-readiness, that line does more damage than
every other edit in the report combined. Fix it, and show maturity through the changelog, the CI
gates, the reorg property tests and the honest-limits list rather than through an adjective.

### The disagreement: keep the RAM figure in the title

The report says lead with DX, put benchmarks third, do not open with a number, because HN punishes
cherry-picked figures. That is correct about **comparative** benchmarks and wrong about **58 MB**.

HN punishes "3× faster than $COMPETITOR", because that is a claim about somebody else measured by
you, and the somebody else turns up in the thread. It rewards a startling **absolute** fact about your
own software stated flatly. "A live blockchain indexer in 58 MB of RAM" is not a superlative, is not
contestable by a competitor, is CI-enforced (build fails above 256 MB), and is the entire thesis in
four characters. It stays in the title.

The figure I would demote is **~289 → ~5,837 events/sec (~20× stacked)**. That one reads as a
benchmark brag, and the honest answer to "20× versus what" is "our own first attempt" - a fine answer,
a weak headline. Move it down and let the **byte-identical output across the fast and slow paths**
carry that paragraph instead. That is the sentence engineers actually stop on.
_(2026-08-22: the ~20× cited here is under re-measurement, #722, with a discrepancy open in #744;
`docs/benchmarks.md` itself no longer quotes the multiplier.)_

### Adopted from the report

- Reframe honest-limits as **scope**, not immaturity. "Events only, three chains" is a boundary, not
  an apology.
- Link `nuthatch bench backfill` with an explicit "run it yourself" and point at
  [benchmarks.md](../benchmarks.md). The house rule (every published number traces to a
  `bench-report.json`) is itself a differentiator in a space full of vendor-run benchmarks; say so.
- Keep the MCP / AI-tooling angle as the second hook, with the ETHGlobal Lisbon bounty as social
  proof rather than as a claim.
- Post Tuesday to Thursday, roughly 08:00-10:00 ET. Be at the keyboard for three to four hours.
- Repo as the landing target, not the website.

---

## 5. Grants

| Fund | Status | Action |
|------|--------|--------|
| **NLnet / NGI** | **Verified 2026-08-19** at nlnet.nl/propose: "There are currently no calls open." Next window **opens 3 Sept 2026, closes 3 Nov 2026 12:00 CEST**. | **Back burner** by decision, 2026-08-19. [nlnet.md](../grants/nlnet.md) is drafted at €38,400 and loses nothing by waiting; the port loop only improves the evidence half of it. If it comes forward: refresh the budget-versus-evidence split against the progress log first, per that document's own rule, and confirm which fund name the September call actually carries - the NGI Zero lineage is mid-transition. |
| **EF ESP** | Permissive relicence removed the licensing friction. Reported to have restructured into a wishlist/RFP model. *(Restructure and figures unverified.)* | **P3**, and now the lead grant. Apply against a specific named wishlist item, not a general inquiry. [ef-esp.md](../grants/ef-esp.md) is drafted at $50-90K. |
| **Sovereign Tech Fellowship** | Reported closed for the 2026 cohort. *(Unverified.)* | Park until the 2027 cycle. Not worth verifying now. |
| **Gitcoin** | GG24 reported concluded, GG25 unannounced. *(Unverified.)* | Watch. Meanwhile the useful work is growing the unique-contributor base, which quadratic funding rewards and which the Show HN produces anyway. |
| **Solana Foundation / Superteam** | Contingent on shipping Solana. | Not applicable per §2. |
| **Optimism RetroPGF / Base / Arbitrum** | Retroactive, impact-gated. | Revisit once there is measurable usage. Not a launch-week lever. |

Two funders and two documents. Neither is currently on the critical path: NLnet is parked by decision
and ESP is gated on having something to point at, which is what P0 and P1 produce. Do not let the
length of that table suggest a workload it does not carry.

---

## 6. Events

- **EuroRust 2026, Barcelona.** Verified 2026-08-19 at eurorust.eu: **14-17 October 2026**, Auditori
  L'illa, tickets on sale. Short-haul from Sofia, Rust-native, and exactly the audience for a
  single-binary infrastructure tool. CFP status not confirmed from the homepage and, for a
  mid-October conference, is very probably closed - assume hallway track and impl days, which is
  still the highest-ROI trip on the list.
- **ETHSofia / ETHWarsaw.** Home region, near-zero cost. Dates unverified.
- **Devcon Mumbai, Solana Breakpoint London.** Far, expensive, and in Breakpoint's case only
  justified by a Solana story we are not building. Skip both.

---

## 7. Compliance positioning, which is a paragraph and not a chain

The MiCA/DORA argument in the report is sound and cheap to act on, because it describes properties
nuthatch already has: self-hosted, deterministic, re-executable, content-addressed Parquet, data
residency determined by where the operator runs the binary. Write it as a positioning page aimed at
EU-regulated CASP and RWA teams. It is a customer segment, not a roadmap item.

Canton is the opposite. Privacy-permissioned Daml with per-participant sub-ledgers and selective
disclosure is not "another chain", it is another product, and the report is right to call it a 2027
bet contingent on a funded design partner. Nothing to do now.

---

## 8. What in here is unverified

Everything sourced from the external report is web research this repo has not confirmed. Verified as
of 2026-08-19 and safe to plan against: the NLnet window, the EuroRust dates and venue. Everything
else - ESP's restructuring and figures, Sovereign Tech's cohort, Gitcoin's calendar, the competitive
state of Solana indexing, the HyperEVM S3 bucket, every dollar and euro figure not in
[docs/grants](../grants) - is unconfirmed and must be re-checked against the live page before it is
allowed to move a date or a decision.

The report's own caveat applies with force: many of the sharpest claims about Solana indexing pain and
about competitors' maturity originate with vendors selling the fix. Direction is probably right;
magnitudes are marketing.

---

## 9. The position this all implies

Recorded because it was the reasoning behind §1 and existed nowhere in the repo.

Everything that has actually worked points somewhere narrower than the README. An infrastructure
operator proposed a partnership unprompted (validation conversation #1). A production dashboard runs
two panels on nuthatch and wants 39. The forum post that landed was about subgraphs going unserved.
The differentiator that keeps proving out is not throughput - it is **you do not have to rent this
from anyone**, plus the derive-first state work (RFC-0023 tiers 1-2) meaning you do not need a
~1.77 TB archive node either.

That is not a general-purpose indexer competing with Ponder and Envio. It is **the self-hosted data
layer for The Graph ecosystem**, a position already half-held, with two users and a partner in it.
Being the Nth EVM indexer is a fight. Being the only self-hosted answer to "my subgraph is unserved
and I am paying a gateway" is not, because nobody else wants that job.

**The standing temptation is to broaden** - another chain, another ecosystem, a bigger claim. Resist
it until Lodestar is off the gateway for everything on-chain. That single fact is worth more than any
amount of breadth, and it is roughly a month of focused work rather than a year.

This is a position, not a non-negotiable. It should be argued with when the evidence changes, and
[CLAUDE.md](../../CLAUDE.md) remains the authority on scope.
