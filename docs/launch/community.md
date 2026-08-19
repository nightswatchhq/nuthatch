# Community and distribution - where nuthatch shows up

- Status: **plan, not a commitment.** Records a strategy and a capacity limit. No dates are binding.
- Author: Pete (cargopete), with Jenny
- Date: 2026-08-19
- Related: [RFC-0007](../rfcs/0007-launch-and-validation.md) Phases 1-3, [home-turf.md](home-turf.md),
  [show-hn.md](show-hn.md), [nest-catalogue.md](../nest-catalogue.md),
  [validation/README.md](../validation/README.md),
  [strategy-review-2026-08-19.md](strategy-review-2026-08-19.md)

## The thesis in one line

Nuthatch's distribution is **nests, not announcements**. Every post that has a chance of landing is a
post about a working artifact somebody else needed, and every post that will not land is a post about
nuthatch.

---

## 1. The asset that already proves it

There is a draft in the working tree, `docs/nuthatch-subgraph-fallback-forum-post.md`, titled *"When a
subgraph is down, the data does not have to disappear."* It is the strongest community artifact this
project has, and at time of writing it is **untracked**. First action in §5 is to commit it.

Why it works, and what to copy from it into everything else:

- **It opens on somebody else's problem**, not on nuthatch. Subgraphs go unserved, fall behind, or
  become unreachable; that is a live operational pain the reader has felt.
- **It proposes a shape, not a replacement.** Keep the subgraph, keep a nest for the data you cannot
  lose, fail over when the endpoint is unhealthy. "Replace your whole stack" is a big ask from a
  stranger; "have a fallback" is not.
- **It states the boundary before anyone can catch you on it.** A nest is not a drop-in GraphQL
  endpoint; mappings that make `eth_call`s, fetch IPFS, or keep bespoke off-chain state need their
  query surface reviewed explicitly. Volunteering the limit is what makes the rest credible.
- **It carries a worked example.** The DOUDOCHAIN_V2 port: a deployment that was unreachable on the
  network, thirteen fixed Arbitrum contracts, pinned source ABIs vendored, IPFS-derived entities
  deliberately *not* claimed as parity.
- **It asks for a deployment ID, not a star.**

The machine has therefore already run once, end to end, on a real deployment. Everything below is
"run it again, deliberately, in the right rooms."

---

## 2. The repeatable machine

> **unserved or failing subgraph → nest port → post the port in that protocol's own channel**

One turn of that loop produces four things simultaneously:

1. A real user with a real problem solved.
2. A nest for the [catalogue](../nest-catalogue.md), which helps whoever hits the same wall next.
3. A post in a community where the author is visibly useful rather than promotional.
4. A pending [validation conversation](../validation/README.md). Roster slots #2 ("a team paying
   Goldsky / Envio Cloud - a real invoice") and #3 ("a Ponder production user") are exactly the people
   found at the far end of this loop.

Nothing else on the launch plan does four jobs at once.

### The lead generation is queryable, not social

Tier 0.1 of the nest catalogue calls the network subgraph "the crown jewel" on demand grounds. It is
also, and more usefully, **the customer-discovery tool**: the network knows which deployments have no
indexer allocation and which are falling behind. That is a ranked list of teams who currently have a
data problem, available without hanging about in a Discord waiting for somebody to complain.

**Sequencing consequence:** build the Tier-0 network-subgraph nest *before* the bigger Tier-1 names
(Aave, ENS). They are larger protocols; this one is the one that finds you the others. This is a
recommendation against the current catalogue ordering and is deliberate.

---

## 3. Channels, ranked, for one person

### Live channels (need presence, cost time)

| Rank | Channel | Why | Carrying what |
|---|---|---|---|
| 1 | **The Graph forum + Discord `#indexers`** | Home turf. RFC-0007 Phase 1 puts it before Show HN. The audience has the pain and the operator relationship already lives here. | [home-turf.md](home-turf.md), then the fallback post from §1 |
| 2 | **Per-protocol Discords** (Livepeer, Uniswap, POA - all published per the catalogue's 2026-08-04 note) | Turning up with a nest for their contracts is a contribution. Turning up to mention the project is not. | The specific nest, plus where it drifts |
| 3 | **Ecosystem dev channels for chains already supported** (Base, Arbitrum, Optimism) | Zero engineering cost: `src/chains.rs` already ships measured keyless endpoints for mainnet, arbitrum-one and base. Reach into an ecosystem we already index rather than integrating one we do not. | A nest on their chain |
| 4 | **r/rust**, separate day, separate framing | Engineering-first, domain-second. | The r/rust angle already drafted at the foot of [home-turf.md](home-turf.md) |

### Passive channels (one PR each, then they pay out unattended)

The highest return per hour available to a sole maintainer. None of these require showing up again.

| Channel | Note |
|---|---|
| **awesome-selfhosted** | The sleeper. Strict inclusion bar, which nuthatch passes comfortably: one binary, no phone-home, `MIT OR Apache-2.0`, self-hosted-first. Its audience is **entirely disjoint from crypto** and will never ask about a token. Do this one regardless of everything else. |
| **awesome-mcp-servers / the MCP ecosystem** | The gap nobody in this space is standing in. An MCP server compiled into the binary, serving real schema-accurate chain data offline, is unusual in an ecosystem still mostly populated by wrappers around hosted APIs. Net-new reach for something already shipped. |
| **This Week in Rust** | A newsletter PR. Free, recurring readership, zero maintenance. |
| **Awesome Rust** | Same shape. |
| **Lobsters** | Adjacent to the Show HN audience, different crowd, low effort. |

---

## 4. The capacity limit, stated so it cannot be quietly ignored

RFC-0007's "one channel per day" is correct for a launch week and **impossible as a standing
commitment for one person.** The honest steady state is:

- **Two live rooms.** The Graph forum, plus one protocol Discord at a time, rotated to whichever nest
  most recently shipped.
- **Four or five passive listings**, which need no presence at all.

Everything beyond that decays into an abandoned account containing one promotional post, which is
strictly worse than never having joined. If a channel cannot be serviced, it does not get an account.

### The anti-pattern

Joining a dozen Discords to mention nuthatch. It does not scale for a sole maintainer, it reads as
what it is, and it burns rooms that a nest would have opened properly later.

---

## 5. Next actions

1. **Commit `docs/nuthatch-subgraph-fallback-forum-post.md`.** It is currently untracked and one
   `git clean` from oblivion.
2. **Decide the hosted-fallback paragraph.** That post's closing offer of "a bounded, rate-limited
   temporary endpoint" edges toward the hosted-service line that [CLAUDE.md](../../CLAUDE.md) puts out
   of scope. It reads as an operator courtesy rather than a product, but a forum will read it as a
   product. Pete's call, made deliberately rather than discovered in a reply.
3. **Open the awesome-selfhosted PR.** Independent of launch timing.
4. **Build the Tier-0 network-subgraph nest**, and use its output as the port queue (§2).
5. **Post home-turf, then the fallback post**, in that order, on the Graph forum.

---

## 6. What is unverified

- Which nests are actually published is the [nests index](https://github.com/nightswatchhq/nests),
  not this file and not the catalogue. The five named here come from the catalogue's own 2026-08-04
  note and should be re-read before any of them is named in a public post.
- awesome-selfhosted's current inclusion criteria have not been re-read against nuthatch. Check
  before opening the PR rather than after a maintainer closes it.
- Every claim here about what a given community wants is judgement, not measurement. The only
  measured community datapoint this project has is validation conversation #1, and it is a sample
  size of one.
