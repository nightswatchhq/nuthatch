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

`docs/nuthatch-subgraph-fallback-forum-post.md`, *"When a subgraph is down, the data does not have to
disappear."* It is the strongest community artifact this project has.

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

### The stopping rule, because the loop has no natural end

"Port subgraphs for whoever asks" is unbounded, bespoke, and indistinguishable from freelancing. It is
possible to disappear into it for a quarter and ship nothing to the core. So the loop is time-boxed by
count, not by calendar:

> **Three ports, then Show HN.**

Three is enough to have real users and a hardened limits list; few enough that this stays a launch
activity rather than a business model. A fourth request arriving mid-loop goes in a queue, not in the
loop.

**Pick the three for what they teach the product, not by who asks loudest:**

1. One **factory or template** protocol, which tests the catalogue's central thesis that a good nest is
   a family rather than a singleton.
2. One already suspected to **hit the `eth_call` or IPFS boundary**, so the parity limit becomes a
   precise empirical statement instead of a caveat written from first principles.
3. One **boring, high-demand and straightforward**, which proves the process is not all artisanal
   effort.

### Why this goes before Show HN

- **There is roughly one Show HN per project.** Posted from the current state, the thread's hardest
  question - some form of "is anyone actually running this" - is answered with one operator
  conversation. Three ports in, it is answered with three names and three working repos. Same post,
  different reception, and the delay costs nothing that cannot be recovered.
- **It hardens the honest-limits list empirically.** "Events only, no `eth_call`, no IPFS" is currently
  reasoned from first principles. The DOUDOCHAIN_V2 port already converted part of it into fact
  (IPFS-derived entities deliberately not claimed as parity). Two more do the same for the rest, and
  that is better copy as well as better engineering.
- **RFC-0007 already sequences it this way.** Home turf is Phase 1 and Show HN is Phase 2. This is a
  sharper Phase 1, not a departure from the plan.

### Where the queue comes from - reversed twice, and here is why

This section has changed position twice and the reasoning is worth keeping rather than tidying away.

**First draft:** build the Tier-0 network-subgraph nest, use its output as the port queue.

**Second draft, corrected:** do not. Building lead-generation tooling for a pipeline nobody has
demonstrated exists is a standard trap. Find the first ports by posting and asking, and build the nest
at port three.

**Current, and the reason for it:** build it first after all, because the cost estimate the second
draft was priced against turned out to be wrong. `graph-gns-nest` and `graph-staking-nest` are
**already running in production on Arbitrum**, and the gns nest already indexes `SubgraphPublished`,
which is the deployment universe. The queue is a three-contract join on live substrate, not a subgraph
port. When the price of tooling drops that far, the objection to building it early goes with it.

The design, the filter that makes it worth having (**curation signal with no open allocation**, not
merely "no allocation"), and the one unverified claim it rests on are in
[port-queue-nest.md](port-queue-nest.md). Read §5 before building §3.

**Ask anyway, in parallel.** Posting the fallback piece and asking on the forum costs nothing, runs
while the nest is built, and is not blocked on it. The first port should come from whichever arrives
first.

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

In order. Nothing here blocks on anything below it.

1. **Settle the hosted-fallback paragraph** in the forum post. Its offer of "a bounded, rate-limited
   temporary endpoint" edges toward the hosted-service line that [CLAUDE.md](../../CLAUDE.md) puts out
   of scope. It reads as an operator courtesy rather than a product, but a forum will read it as a
   product. Decide it deliberately rather than discovering it in a reply.
2. **Post [home-turf.md](home-turf.md) on the Graph forum**, then the fallback piece. In that order.
3. **Ask for the first unserved deployment** in the same thread and in `#indexers`. No tooling.
4. **Run the loop three times**, chosen against the criteria in §2.
5. **Build the port-queue nest** ([port-queue-nest.md](port-queue-nest.md)), starting by confirming
   the one unverified claim in its §5. Runs in parallel with 3 and 4, blocks neither.
6. **Then Show HN**, per [strategy-review-2026-08-19.md](strategy-review-2026-08-19.md) and RFC-0007
   Phase 2.
7. **Open the awesome-selfhosted PR** whenever. It is independent of all of the above and costs an
   hour.

**Not on this list:** NLnet. Its window (3 Sept - 3 Nov 2026) is real and verified, but it is on the
back burner by decision as of 2026-08-19. Recorded so that if it comes forward again, the date is
already known rather than rediscovered.

## 6. What is unverified

- Which nests are actually published is the [nests index](https://github.com/nightswatchhq/nests),
  not this file and not the catalogue. The five named here come from the catalogue's own 2026-08-04
  note and should be re-read before any of them is named in a public post.
- awesome-selfhosted's current inclusion criteria have not been re-read against nuthatch. Check
  before opening the PR rather than after a maintainer closes it.
- Every claim here about what a given community wants is judgement, not measurement. The only
  measured community datapoint this project has is validation conversation #1, and it is a sample
  size of one.
