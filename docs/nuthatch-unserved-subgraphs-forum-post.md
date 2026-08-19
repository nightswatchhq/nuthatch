# Draft: forum follow-up on the unserved subgraphs

_Draft for the Graph forum, follow-up to "When a subgraph is down, the data does not have to
disappear". Not posted. Numbers verified 2026-08-19; re-check before posting._

---

a while back i posted about running a nest as a fallback for when a subgraph is unreachable. since
then i stopped waiting for people to tell me theirs was down and just went looking, because the
network already knows. curation signal and indexer allocations are both onchain, so you can index
SubgraphService and L2Curation and ask which deployments have signal sitting on them and no allocation
against them. that took about a day and the answer is 3853 deployments with live signal and nobody
serving them, though most of that is noise, at over 1000 signal its 63 and thats a list you can
actually read.

i took two off the top and ported them. peeranha on polygon, 10673 GRT signalled, five contracts, 33
tables, backfilled 62.7m blocks. spookyswap on fantom, 21491 GRT signalled, factory plus every pair it
creates, still backfilling as i write this because its a much bigger nest than the signal suggested.
both were scaffolded with `nuthatch init --from-subgraph <deployment>` straight off the manifest, and
in both cases it worked out the factory rule from the manifest itself rather than me writing it.

the bit thats worth showing is what the gateway says if you ask it for either of them

    {"errors":[{"message":"subgraph not found: no allocations"}]}

thats not slow or stale, it just cant answer. meanwhile the nests do. so the same claim as last time
but with the receipts this time, and on subgraphs i didnt pick, the queue picked them.

what i like about it is the diagnosis got confirmed three separate ways that share no code. our nest
joining allocations against signal from raw events, the network subgraph reporting zero active
allocations for the same deployments, and then the gateway refusing outright. hard to argue with that
one :pepeD:

being honest about what a port is though, its event data matching the manifest and nothing more. the
derived entities are views over that, and the ones that are pure functions of the events come out
exactly, but anything that reads back its own previous output doesnt. uniswaps derivedETH is the
obvious one, it reads the stored derivedETH of other tokens so the answer depends on write order, not
just on the events. you can compute a fixed point instead and its defensible but its a different
number and i'd rather say that than pretend. eth price itself is fine, thats one pools sqrtPriceX96
and we match the subgraph to 35 digits.

repos are at github.com/nightswatchhq/peeranha-nest and github.com/nightswatchhq/spookyswap-nest,
both MIT/Apache, run them yourself, no allocation to wait for.

if your subgraph is on that list or just unserved and youd rather it wasnt, send me the deployment id
and the queries that matter and i'll tell you whats event derived and whats not, and probably just
build the nest.
