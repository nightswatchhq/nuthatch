# RFC-0037: IPFS content resolution - a verified, content-addressed side table

**Status:** Draft (2026-08-19). Nothing is built. Depends on 0001 (decode registry and vendored ABIs),
0023 §3 (the pinned-call cache whose machinery this reuses rather than duplicates), 0013 §3 (sealed
segments the resolved documents spill into). Adjacent: 0024 (the sibling "irreducible residue" engine).
Corrects an implication in `src/subgraph_import.rs`. Blocks: subgraph ports whose entities are
IPFS-derived - today they are excluded from parity by hand, one port at a time.

## 1. What is already true

Three facts from the tree, because this RFC is smaller than it looks and the reason is that most of
the substrate exists.

**The CID is already a column.** `src/subgraph_import.rs` says it plainly:

> `file/ipfs` dataSources index the *content* behind a CID, which nuthatch does not do - it indexes
> the metadata hash as a column value and stops there.

So an event carrying `tokenURI` or a metadata hash already lands in a nest's tables with the CID
present and unresolved. **The join key exists.** Nothing about the event tables has to change for this
RFC, which is what makes it a second *source* rather than a second storage path - the same distinction
RFC-0036 §3 turned on.

**The manifest parser already sees these sources.** `ManifestSource::is_evm` returns false for
`file/ipfs`, and the kind is deliberately carried through "so we can report why they were skipped."
The scaffolder already knows what it is declining to do.

**The gateways are already wired, and the fetch is already unverified.** `DEFAULT_IPFS_GATEWAYS` lists
four (The Graph's first, then ipfs.io, then Pinata), used at scaffold time to pull manifests and ABIs.
The module's own doc comment owns the hole:

> Content-addressed, but **not verified here**: nothing recomputes the multihash over the bytes a
> gateway returns, so a hostile or compromised gateway can serve any document for any CID and this
> module will vendor it. The CID buys a stable name to ask for, not proof of what came back.

That is an integrity gap in shipped code, independent of everything else this RFC proposes, and §4
closes it first.

## 2. The property that makes IPFS not an HTTP enricher

[CLAUDE.md](../../CLAUDE.md) inherits liminal's purity rule: a component granted zero capabilities is
deterministic by definition, and **effectful components produce annotations only, never canonical
entities.** The obvious reading files IPFS with the HTTP enrichers and stops there.

That reading is wrong, and the difference is worth being precise about because the whole design hangs
on it.

An HTTP enricher can hand two operators **different answers**, and neither can tell. That is
divergence, and it is exactly what the purity rule exists to keep out of stored state.

An IPFS document cannot. `CID → bytes` is checkable: recompute the multihash and you either hold the
document the CID names or you hold nothing. Two operators asking for the same CID either **agree or
one of them has nothing at all.**

> **The failure mode is unavailability, not divergence.**

That is a categorically weaker failure than the rule was written to prevent, and it is the same
property RFC-0023 tier 3 leans on for `eth_call`: `src/calls.rs` stores a result under a `CallKey` of
`(chain, block, contract, calldata)` precisely so that "two operators who run the same declaration over
the same range produce byte-identical results and can share segments without trusting each other." A
CID is that same argument, arriving pre-made.

**Therefore:** IPFS-resolved content may feed canonical state, provided every byte of it is verified
against its CID and absence is representable rather than papered over. It does not need exile to
annotations. This is the one substantive decision in this RFC and §7 records it as such.

## 3. Design: a resolution table, joined at query time

One table per nest, keyed by CID. Not a column on the event rows, and not a mutation of anything
already stored.

| Column | Note |
|---|---|
| `cid` | The address and the primary key. Canonicalised to a single CID form on write. |
| `bytes` / `document` | The verified payload. |
| `resolved_at_block` | The cursor position when resolution succeeded, for auditability. Never part of the identity. |
| `status` | `resolved` only. Absence of a row *is* the unresolved state. |

**Present, absent and pending fall out of a `LEFT JOIN` and need no tri-state anywhere.** An event row
whose CID has no counterpart in the resolution table simply joins to null, which is the honest answer
and is already how a view would want to express it. This is the reason to make it a side table rather
than an enrichment of the event row: enrichment forces a decision at write time about data that may
arrive later or never, and a join does not.

**The identity is the CID and nothing else.** Not the nest, not the declaration, not when it ran -
deliberately mirroring `CallKey`'s reasoning. Two nests on two machines resolving the same CID hold
byte-identical rows and can share sealed segments without trusting each other. Resolution is therefore
a **shareable, cacheable public good**, in a way an `eth_call` against a private archive is not.

**Resolution is host-run.** The host fetches and verifies; components receive only data. Components
stay zero-capability and pure and may still feed entity derivation, exactly as §3 of RFC-0023 arranged
for calls.

**Resolution is never on the tip path.** Documents are resolved behind the cursor and spilled with the
sealed segments. Nothing in this RFC may make tip-following wait on a gateway.

## 4. Verification is the first slice, and is not optional

Recompute the multihash over the returned bytes and compare it to the CID. Refuse the document on
mismatch, and move to the next gateway rather than failing the nest.

This lands **before** any indexing behaviour, for two reasons. It closes a real integrity gap in
shipped scaffold-time code, where a compromised gateway can currently vendor an arbitrary ABI into a
nest. And nothing in §2's argument survives without it: unverified IPFS *is* an HTTP enricher, with
all the divergence the purity rule forbids, and would belong in annotations after all.

## 5. Slices

Each ends runnable, per the build-order rule.

**Slice 1 - verify what we already fetch.** Multihash verification in `subgraph_import.rs`'s gateway
path. Gateway returns wrong bytes, nuthatch says so and tries the next one. Retires the "pinned by CID"
implication the module doc currently flags. No new config, no new tables, no new surface.

**Slice 2 - the resolution table and a manual resolver.** The table, the CID canonicalisation, the
host-side fetch-and-verify, and a `nuthatch` subcommand that resolves the CIDs already sitting in a
nest's columns. Explicitly manual and out-of-band: it proves the storage and the join before anything
runs automatically.

**Slice 3 - declared resolution.** A config block naming which columns carry CIDs worth resolving,
resolved behind finality and sealed with the segments.

> **Slice 3 must not parse before it executes.** RFC-0036 §5.1 and issue #262 are the same lesson twice:
> a config key that validates and then silently produces nothing is the worst failure this project can
> ship, because the config looks like it worked. If declared resolution parses before the resolver
> exists, it refuses at load with a message naming this RFC, exactly as `refuse_unwired_calls` does for
> tier-3 calls today.

**Slice 4 - offline and self-hosted paths.** A local IPFS node or a pinned directory as a source,
because "four public gateways" is a third-party data dependency in everything but name, and
non-negotiable 3 does not have an exception for content addressing. Gateways stay the convenience
default; they must not be the only door.

## 6. Non-goals

- **Not an IPFS node**, and not a pinning service. Nuthatch fetches and verifies; it does not host,
  serve or guarantee availability of anything.
- **Not a required dependency.** Non-negotiable 1 names IPFS explicitly among the services embedded
  mode must run without. A nest that declares no CID resolution must behave exactly as it does today,
  and `nuthatch dev` must never touch a gateway unless asked.
- **Not retroactive re-resolution.** A document that resolves later is a new row, never a rewrite of a
  sealed segment. Same rule as decodings: version, do not revise.
- **Not arbitrary HTTP.** The entire §2 argument is about content addressing. A URL is not a CID and
  gets none of this.
- **Not parity by default.** A port claims IPFS-derived entities as parity only when the underlying
  documents actually resolved, and says which did not.

## 7. Open questions

1. **Does resolved content feed canonical state, or annotations?** §2 argues canonical, on the grounds
   that verified content addressing fails by unavailability rather than divergence. This is the
   decision the RFC exists to make and it should be argued with before slice 2 fixes it in a table.
2. **Is a re-execution that cannot fetch a document a failure or a hole?** Determinism says a
   re-execution must reproduce stored state; a garbage-collected CID makes that impossible through no
   fault of the design. Proposed answer: a hole, reported, never a silent divergence - but "reported"
   needs a shape.
3. **What canonical CID form?** v0/v1, base32/base58, raw versus dag-pb all name the same bytes and
   spell differently. One form on write, or the primary key is a lie.
4. **How much does this actually block?** Partly measured now, and it has a named customer.
   **Lodestar's `subgraph-names` and `subgraph-search` routes cannot leave The Graph gateway without
   this RFC**, because subgraph display names and metadata live in IPFS-pinned JSON behind the GNS and
   not on chain (RFC-0011 status update, 2026-08-19). That is nuthatch's only production consumer, so
   it is a real requirement rather than a hypothetical one - though it is **two routes out of 39**, so
   it does not by itself outrank RFC-0023 tier 3's missing executor. The three subgraph ports in
   [community.md](../launch/community.md) §2 remain the instrument for the rest: record which mappings
   die on `eth_call`, which on IPFS, and which on neither, then weight with a count rather than an
   intuition.
