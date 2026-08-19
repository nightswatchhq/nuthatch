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

Revised 2026-08-19 after the community pass ([community.md](community.md)): the port loop moves to the
front, and NLnet moves to the back burner by decision.

| # | Item | When | Why it sits here |
|---|------|------|------------------|
| **P0** | Post [home-turf.md](home-turf.md) on the Graph forum, then the subgraph-fallback piece, then ask for an unserved deployment | now | RFC-0007 Phase 1, and it is what generates every item below it. No tooling required to start. |
| **P1** | Run the port loop three times ([community.md](community.md) §2) | before Show HN | Three ports converts "is anyone running this" from an awkward question into three names, and converts the honest-limits list from reasoning into measurement. |
| **P2** | Show HN copy: version-number honesty pass, then post | Tue-Thu, ~08:00-10:00 ET, after P1 | `show-hn.md` says "It's v0.1.0". `Cargo.toml` says `2.5.0`. One line, and it currently undercuts the entire post. Phase 2 of RFC-0007. |
| **P3** | EF ESP inquiry against a named wishlist item | after the post, with the thread and the ports as evidence | [ef-esp.md](../grants/ef-esp.md) exists. The permissive relicence removed the friction that made this awkward. |
| **P4** | HyperEVM as the chain expansion | ~1 week of work, opportunistic | EVM, so it is a `chains.rs` entry plus endpoint measurement. Same "underserved, one dominant provider" story as Solana at ~5% of the cost. |
| **P5** | Compliance / data-residency positioning note | a page, whenever | Attaches to the product that already exists. Costs nothing to write and nothing to maintain. |
| **-** | **NLnet** | **back burner** (decision, 2026-08-19) | The window is real and verified (3 Sept to 3 Nov 2026, 12:00 CEST) and [nlnet.md](../grants/nlnet.md) is drafted at €38,400. Recorded, not scheduled, so that if it comes forward the date is known rather than rediscovered. Missing this window means waiting for the next cycle. |
| **-** | **Solana** | **not now** | See §2. The case for it rests on a pipeline that does not exist, and the honest build violates non-negotiables 1 and 3. |

**What changed and why.** The first version of this table put NLnet at P0 on the grounds that it was
the only item with a verified external deadline. That was true and is still true, and it was the wrong
reason to rank it first: a deadline makes something *urgent*, not *load-bearing*. The port loop is
load-bearing, because it feeds the Show HN, the validation roster, the honest-limits list and any
future grant application at the same time. NLnet was subsequently put on the back burner by decision,
which settles the question either way.

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
