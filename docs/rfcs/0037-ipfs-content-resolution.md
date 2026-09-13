# RFC-0037: IPFS content resolution - a verified, content-addressed side table

**Status:** **Accepted, slices 1-7 built.** Slice 7 (2026-09-13): multi-block documents are verified,
by re-encoding in Kubo's default layout or from their blocks, and an unproven document writes no row;
see §5. Slice 6 (2026-09-13): resolution completes out of band, with retry, a recorded give-up and a
seal hold; see §5. Slice 5 (2026-09-13): a CID inside JSON, from an event
column or a top-level call's calldata, with a topic filter; see §5. Slices 1-4 (2026-08-19). Slice 1 (verification) and slices 2-3
(declared resolution) shipped in PR #645; slice 4 is `--ipfs`, which takes a local node URL as readily
as a gateway, so an operator can already take every third party out of the path. Depends on 0001 (decode registry and vendored ABIs),
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

**Slice 5 - a CID inside JSON, and a call table as the source.** Edge & Node's QoS oracle posts
`submitQoSPayload(bytes)` to a DataEdge on Gnosis, and the argument is JSON:
`{"topic": ..., "hash": <CID>, "timestamp": ...}`. `cid_from_value` accepted a string or exactly 32
bytes, so every such row was skipped and nothing said so. `[[ipfs]]` now takes `cid_json_path` (one
top-level key; an object names one document, an array one per element) and `json_match` (string
equality on top-level fields, applied before any fetch). Both enter the declaration hash only when set,
so no existing nest changes identity. Two declarations may read the same column with different
matches, which is how one nest keeps both oracle topics apart.

Three faults were found on the way and fixed in the same slice, because each one alone makes the
oracle nest resolve nothing:

- Top-level calls were decoded after IPFS resolution and never offered to it, so `on` could not name a
  call table.
- The top-level call filter used the `eth_getLogs` address list, which holds only contracts with
  events. A calldata-only contract (the DataEdge ABI has none) produced no call rows.
- A row whose declared column named no CID, or was missing, left no trace. It now counts in
  `nuthatch_nest_ipfs_unreadable_total`.

Verified live on 2026-09-13 over Gnosis blocks 48,231,452 to 48,232,456: 36 calls, 36 documents (18
per topic), 37.8 MB, 0 unreadable. The declaration hash moves the decode identity in `schema.json`
(`src/project.rs`); the runtime identity guard and `/sql` provenance still hash the event registry
alone, which for an event-less nest is the hash of nothing.

Two limits were named rather than fixed. A document over 256 KiB was stored `verified = false`,
because a multi-block UnixFS root could not be re-derived from its bytes; the oracle's payloads are 0.6
to 1.9 MB, so every one of them was in that case. Slice 7 closes that. And a CID that misses the
per-window budget, or whose gateways all fail, is never attempted again: the out-of-band resolver the
budget warning refers to does not exist yet. Slice 6 builds it.

**Slice 6 - resolution completes.** Built 2026-09-13, unreleased. The per-window budget is gone.
Documents resolve out of band behind the cursor (`src/ipfs_resolve.rs`), from a work list re-derived
from the rows already in the hot store: a document's key is a function of its block's rows alone, so
nothing extra is recorded and a restart loses nothing. A failed fetch, a body cut off mid-read
included, retries with doubling backoff from 5 seconds to 10 minutes. After 10 failures the document
is given up on, recorded in store meta under its block and slot with its CID, and counted in
`nuthatch_nest_ipfs_given_up_total`. Sealing holds below the lowest block with a document neither
stored nor given up on, so a range never seals short of one that could still arrive, and
tip-following never waits on a gateway. A document is written only while the row that named it still
carries the same block hash, in one transaction, so a reorg cannot be followed by a stale document.
Slots are assigned per block rather than per fetch window, which removes a dependence on `--window`
from sealed segments.

With it: top-level call rows carry `tx_from`, the transaction sender, and a top-level-calls nest
indexed before this refuses to start and must be re-indexed. The identity guard and `/sql` provenance
cover call, `[[ipfs]]` and `[[calls]]` declarations, so an event-less nest no longer claims the hash of
nothing. `--seal-direct` decodes top-level calls and resolves their documents inline. Block bodies are
fetched 200 blocks at a time, because a whole 20,000-block Gnosis window of them held 2.3 GB with
nothing committed.

Measured against Gnosis on 2026-09-13, with the QoS nest's configuration minus `blocks = true` and
default windows, from block 48,119,000: 678 calls from the one publisher and 678 documents; 2026-09-07
complete at 288 per topic, 576 of 576, including `QmYTFzn…`, the bucket the budget had lost; killed
with `kill -9` at 120 resolved and 558 pending, restarted, and finished 2 minutes 59 seconds later
with nothing lost, given up or unreadable. The first 20,000-block window took about four and a half
minutes, because bodies come serially at 2.5 to 3.6 seconds per 200-block batch on public RPC. All 678
were stored `verified = false`, because slice 7 was not yet in that build.

**One limit found and not fixed.** Once the hold released a finalized range holding those 678
documents, about 1.1 GB of JSON, sealing it reached 3.17 GB of resident memory, past the per-cursor
budget. Seal cuts are bounded by row count and span, never bytes (`seal_cut`), and `maybe_seal` and
`seal_range_with_snapshot` hold the whole range at once, parsed. A byte bound on the cut would be
deterministic, being a property of the rows, but it changes RFC-0028 §4's cut rule, and that is a
decision to make before building it. Until then a nest whose documents run to megabytes is not fit to
deploy. The same run did not stop on SIGTERM for 26 seconds and needed `kill -9`; synchronous seal
work giving an abort nothing to act on is the likely cause, and it is not established.

**Slice 7 - multi-block documents are verified, and an unproven one writes no row.** Slice 5's live run
verified 0 of 36 oracle payloads. Two ways now prove a file past 256 KiB:

- **Re-encoding in Kubo's default layout.** The bytes are cut into 256 KiB leaves (dag-pb, or raw under
  CIDv1) and built into a balanced tree of at most 174 links, as `ipfs add` does by default, and the
  root is hashed. No gateway cooperation and no extra request. It is anchored to the network rather
  than to our own encoder: a real 152-byte oracle root block re-encodes byte for byte from its own
  links, a real leaf fixes the chunk size, and Kubo's empty-file CID (`QmbFMke1…`) holds, which also
  corrected the single-block encoder (Kubo omits an empty Data field). All 4,025 oracle payloads
  fetched for 2026-09-06 to 2026-09-12 verify this way.
- **Blocks, when re-encoding cannot.** A file imported another way is asked for as a CAR
  (`?format=car`, the trustless gateway form). Every block is hashed against the CID naming it before
  the DAG is walked, from the root we asked for and never from the roots the header claims; each
  child's bytes are checked against its parent's `blocksizes` and each node's total against its
  `filesize`. Caps of 16 MiB of file, 4,096 blocks and block visits, and 16 levels bound a hostile
  DAG; past them the document is refused and counted in `nuthatch_nest_ipfs_oversize_total`.

Which trustless forms real endpoints serve, measured on 2026-09-13 with two oracle CIDs: The Graph's
path gateway ignores `?format=raw` and `?format=car` and returns the file in 0.1 to 0.2 s, and its Kubo
RPC `block/get` and `dag/export` answer 403; Pinata serves raw blocks in 4 to 6 s each and whole CARs in
5 to 6 s; `ipfs.io` and `trustless-gateway.link` timed out at 60 s. Re-encoding is therefore the path
that works everywhere, and the CAR the fallback where a gateway offers one.

Verified live on 2026-09-13 by re-running slice 5's scratch nest from block 48,231,452 to the tip at
48,232,905: 50 documents resolved, 50 verified (25 per topic, 52.0 MB), 0 unverified, 0 oversize. The
36 inside slice 5's range are the same 36 it had stored unverified. Re-encoding costs no request, so
a document in the default layout still costs one fetch; the CAR costs one more request per offering
gateway, and only for a document re-encoding cannot prove.

**Policy.** A nest's resolver stores only proven documents. An unproven fetch writes no row, counts in
`nuthatch_nest_ipfs_unverified_total` and is retried on slice 6's policy, since a gateway that serves
the blocks may answer later; a document over a cap is given up on at once, being the same size from
every gateway. That is §2's rule: unverified IPFS is an HTTP enricher and does
not feed canonical state. The `verified` column stays, so no schema or identity changes; it is `true`
on every row written from this build on, rows older builds stored as `false` keep that value, and a
re-index either proves them or leaves them out. `init` still accepts an unproven manifest or ABI,
loudly, as slice 1 decided.

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
