# RFC-0032: The tenant runtime - retiring the roost, keying data by nest identity

- Status: **Implemented** (slices 1-5; slice 5 shipped in v2.0.0, 2026-08-06)
- Author: Pete (cargopete)
- Date: 2026-08-04
- Depends on: RFC-0012 (the nest bundle and its content address - this RFC promotes that hash to the
  storage key), RFC-0021 (one isolated cursor per chain - unchanged, and the unit a mount still joins),
  RFC-0026 (fault quarantine - the blast-radius substrate), RFC-0027 (live mount/unmount and the
  persisted mounted-nest list - this RFC replaces that list with a table), RFC-0019 (registry
  resolution, for mounting by `name@version`).
- Supersedes: the *roost as a concept* - `roost.toml` as authored config, `Roost::nest_dir`'s
  name-keyed layout, and the `dev` vs `roost dev` split. RFC-0012, 0021, 0026 and 0027 keep all their
  mechanics; only the container and the key change.
- Blocks: RFC-0034 (the query allowlist ships as mount config, which needs the mount table),
  RFC-0035 (the 2.0 breaking surface, of which this is the largest item).
- Related: RFC-0033 (nest identity and derivation grafting) adds a *finer* reuse key **below** the NID.
  The two are independent: this RFC needs only the property that any authored edit yields a different
  NID, which `blob.rs` already provides and tests.
- Nature: **full RFC.** It changes the unit of storage, the unit of deployment and the top-level CLI
  shape. Not a mini-RFC and should not be built as one.
- Origin: the 2026-08-04 architecture session with chris (GraphOps), recorded in
  the session's working notes (unpublished) §3 and §10 (decisions O-1, O-1b, O-2, O-3, O-11, O-12, O-13,
  O-14).

## Abstract

Nuthatch has two runtime shapes: a single nest (`nuthatch dev`) and a roost (`roost dev`) - a directory
of nest directories sharing a process. That split is a decision users are forced to make before they
know which one they want, and the roost's layout cannot express the thing hosted nests actually need:
**two tenants running the same nest, indexed once.**

A roost is a folder of folders. Its data lives at `<roost>/nests/<name>`. There is nowhere in that
layout to hang a reference count, and the folder name is the wrong key - two operators can mount
byte-identical nests under different names and get two independent backfills of the same chain data,
or mount different nests under the same name and collide.

This RFC retires the roost. The runtime hosts **1..N nests**, always. Data is keyed by **nest identity
(NID)** - the content address `blob.rs` already computes. Mounts are keyed by **`(tenant, NID)`** in a
table the runtime owns. A tenant is an opaque string with a real default, so single-tenant is `N=1`
rather than a special case, and nobody who does not want tenancy ever types the word.

Two tenants mounting the same nest share one dataset and it is never indexed twice. Unmounting
decrements a refcount; the data survives until the last reference goes *and* an operator prunes.

It is a breaking change and it is the headline of 2.0.

## 1. What actually happens today

**Storage is keyed by name.** `Roost::nest_dir` (`roost.rs:152`) is exactly
`dir.join(NESTS_DIR).join(name)`. The name is an operator's label, chosen freely, with no relationship
to what the nest *is*. Every consequence below follows from that one line.

**The nest set is a list of names.** `RoostMeta::nests` (`roost.rs:76`) is a `Vec<String>`. RFC-0027
made it mutable at runtime and persisted it back with `persist_mounted_nests` (`roost.rs:644`), which
serialises the whole `roost.toml` from runtime state. So the file has already been half-dissolving into
state for a release: it is authored config that the runtime overwrites.

**The router is name-nested.** `serve::run_roost` loops `app.nest(&format!("/{name}"), …)`
(`serve.rs:263`). One path per name, one name per dataset - the two are welded together.

**There is no notion of a tenant anywhere.** Not in config, not in state, not on the wire.

### 1.1 What that layout cannot express

| Want | Why the folder layout refuses |
|---|---|
| Two tenants, one dataset | Nowhere to store a refcount; two mounts means two directories means two backfills |
| Delete tenant A's nest without touching tenant B's | The directory *is* the mount record; removing one removes the data |
| Detect that two mounts are the same nest | Names are free-form; identity is not represented at all |
| Mount the same nest twice at different depths | Second mount either collides or forks a full copy |

None of these are fixable inside the roost shape. They need the mount record and the dataset to be
**different objects**, which is what this RFC does.

## 2. The decisions this encodes

All agreed 2026-08-04 and logged in the session working notes (unpublished) §10. Restated here so the
RFC is self-contained; the log is the authority for the reasoning.

| ID | Decision |
|---|---|
| O-2 | The roost is retired as a concept. A major version (2.0) is acceptable to do it properly rather than carry a deprecated alias. |
| O-1 | A tenant is an **opaque string**, not a principal. It labels a mount, never the data. No authz, no quotas, no metering - identity stays the gateway's job. |
| O-1b | Single-tenant is `N=1` with a **default tenant string**, never `Option<String>`, never null. One code path. |
| O-3 | Data is keyed by NID; mounts are keyed by `(tenant, NID)`. Shared nests are indexed once. Delete decrements a refcount. |
| O-4 | The NID **is** the content address `blob.rs` already computes - `sha256` over the canonical manifest of authored inputs, with `registry_hash` re-derived and asserted on load. |
| O-11 | Backfill depth is the **union of ranges**. The dataset holds the deepest range any mount asked for; a shallower mount sees a subset. |
| O-12 | Routing is a path per `(tenant, nest)`. Two doors, one room. |
| O-13 | Collection is **deferred**, with an explicit `prune`. Refcount zero marks data collectable, not deleted. |
| O-14 | Storage keyed by name is not a decision - it is the load-bearing implementation work of this RFC. |

## 3. Identity: the NID is not new work

The NID is `Manifest::blob_hash()` (`blob.rs:66`): `sha256` over the canonical manifest bytes, with
fixed field order and per-file `sha256` digests over the authored inputs. `load` re-derives the decode
registry from those inputs and asserts it matches the manifest's `registry_hash` (`blob.rs:560`), so a
nest that loads is provably the nest its author packed.

The property refcounting needs is precisely the property content addressing gives:

> **Any change to an authored input yields a different NID.**

So divergence forks its own dataset automatically. Two tenants can never contaminate a shared dataset
by editing, because an edit is no longer the same nest. This is why the design is sound with no
locking, no ownership and no arbitration between tenants - the same reason Nix can share a store path
between users and OCI can share a layer between images.

**Explicitly out of scope for identity:** backfill depth (O-11 - a range, not an input), the query
allowlist in phase 1 (RFC-0034 - mount config), the tenant string, and the mount path. None of these
change what a nest *computes*, so none belongs in what it *is*.

**Not decided here:** whether a finer, per-derivation reuse key exists below the NID. RFC-0033 says
yes. It does not invalidate anything in this RFC - it operates at a different granularity, on the
contents of a dataset rather than on its name.

## 4. The mount table

`roost.toml` becomes a **mount table**: runtime state the runtime owns, not authored config an operator
maintains. RFC-0027 already had the runtime persisting the mounted set back over the file; this
finishes that move honestly instead of leaving a config file that lies about who writes it.

A mount record is:

| Field | Meaning | In the NID? |
|---|---|---|
| `tenant` | Opaque string. Defaults to `"default"`, operator-configurable. | No |
| `nid` | The nest's content address. The dataset key. | Is the NID |
| `alias` | The name this mount is served under. Free-form, per-mount. | No |
| `source` | Where the bundle came from (`path` / `name@version` / registry URL), for re-resolution. | No |
| `backfill_from` | The depth *this mount* asked for. See §7. | No |
| `mounted_at` | Timestamp, for operator forensics. | No |

Primary key: `(tenant, nid)`. Mounting the same NID twice for one tenant is idempotent - it is the same
mount, and re-mounting may only *widen* `backfill_from`.

Storage moves from `<root>/nests/<name>` to `<root>/data/<nid>`. The dataset directory has no idea what
it is called or who mounted it; the table holds all of that.

### 4.1 Uniqueness of the alias

An alias must be unique **within a tenant**, not globally - `/acme/uniswap` and `/globex/uniswap` are
two mounts of possibly the same NID and must both work. Alias collision within a tenant is a mount
refusal, alongside the three admission refusals RFC-0027 already defines.

## 5. Refcounting, unmount and prune

The refcount of a dataset is the number of mount records naming its NID, across all tenants. It is
**derived, not stored** - a count over the table - so it cannot drift out of sync with the table, which
is the failure mode every hand-maintained refcount eventually hits.

- **Mount:** insert a record. If the dataset already exists at the requested depth or deeper, the mount
  is served immediately from existing data - no backfill at all. This is the payoff.
- **Unmount:** delete the record. If the count reaches zero, the dataset is **marked collectable** and
  left alone.
- **Prune:** an explicit operator command removes collectable datasets and reports what it freed.

Deferred collection (O-13) is deliberate. Re-backfilling is exactly the cost this design exists to
avoid, so an accidental unmount must not trigger one; unmount/remount stays free. The cost is disk held
by nothing, which is visible, bounded and an operator's call - the right trade in that direction.

**A tenant's unmount never inspects another tenant's state.** It deletes its own row. Whether the data
survives falls out of the count.

## 6. Single-tenant is `N=1`

The mount table always carries a real tenant value. There is no `Option<String>`, no null, no
"tenancy disabled" branch.

This is a correctness decision, not a style one: two code paths - one with tenancy, one without - means
the path almost every user is on is the one that rots, because the tenancy path gets the attention and
the bugs land in the other. One path, exercised by everyone, at all times.

What follows:

- **Zero ceremony is preserved.** `nuthatch dev --dir ./nest` behaves exactly as it does today. The
  tenant is `"default"`, nobody types it, nothing in the output mentions it.
- **The arithmetic is identical at `N=1`.** Refcounting a single mount is refcounting; there is no
  branch to get wrong.
- **Migration becomes a relabel.** Existing deployments become `tenant="default"`. Enabling hosted
  tenancy later moves no data and re-indexes nothing.

## 7. Routing, and backfill depth

**Routing (O-12).** A path per `(tenant, alias)`, both serving one dataset:

```
/acme/uniswap-v2/sql     ─┐
                          ├─ data/<nid>
/globex/univ2-prod/sql   ─┘
```

Tenant isolation stays visible at the API surface without duplicating a byte. In single-tenant mode
the tenant segment is omitted from the path so today's URLs are unchanged.

Two doors to one room means **the room is shared** - a heavy query from one tenant is felt by the
other. Nuthatch does not solve that here and does not pretend to: the `/sql` guards (timeout, row cap,
concurrency) are per-runtime node self-protection, per-tenant quotas are out of scope by CLAUDE.md,
and RFC-0034's allowlist is the mechanism that bounds the surface. Stated plainly so nobody reads
"isolated path" as "isolated resources".

**Backfill depth (O-11).** The dataset holds the deepest range any mount requested. A mount asking for
a shallower range gets a subset - served, not re-indexed. A mount asking deeper extends the dataset,
which is natural because segments are immutable and append-only.

Depth stays out of identity precisely so that differing depth does not fork datasets - which is the
single most likely reason two tenants would otherwise fail to share.

## 8. Migration

Breaking, and it is 2.0. No deprecated alias for `roost dev` (O-2).

1. **`nuthatch migrate`** reads a `roost.toml` and an existing `nests/` tree, computes each nest's NID
   from its authored inputs, moves `nests/<name>` → `data/<nid>`, and writes a mount table with
   `tenant="default"` and `alias=<name>`.
2. **Data is moved, never re-indexed.** If the migration would need a backfill, the migration is wrong.
3. **The NID must reproduce.** `migrate` re-derives `registry_hash` and refuses a nest whose inputs no
   longer reproduce its manifest - by name, with the mismatching file listed.
4. **`roost dev` exits with an error naming `migrate`**, not a silent alias to `dev`. The commands do
   different things now and pretending otherwise would hide the layout change until something else
   broke.

If two nests in one roost turn out to have the *same* NID, the migration has found a pre-existing
double-index. It merges them into one dataset, keeps both aliases, and says so.

## 9. Slices

Each ends runnable and testable. No slice ships the next one's ceremony.

| # | Slice | Ends with |
|---|---|---|
| 1 | **Address the data.** `data/<nid>` layout + `migrate`. Runtime still single-tenant, no table. | An existing roost runs unchanged off NID-keyed directories, with a parity test against the name-keyed run. |
| 2 | **The mount table.** Replace `RoostMeta::nests` with mount records; derived refcount; alias routing. Tenant hardcoded `"default"`. | Two aliases over one NID serve identical data from one dataset. Byte-parity test. |
| 3 | **The tenant string.** Real, configurable, defaulted. Tenant path segment in multi-tenant mode. | `(acme, X)` and `(globex, X)` both mount; one backfill; unmounting one leaves the other serving. |
| 4 | **Lifecycle.** Refcount to zero → collectable; `nuthatch prune`; deferred collection. | Unmount/remount round-trips with zero re-indexing, proven by a metrics assertion, not by eyeball. |
| 5 | **Retire the roost.** Remove `roost dev`, `roost.toml` as authored config, `Roost::nest_dir`. Docs and skill updated. | The word "roost" survives only in RFC history and the migration error message. |

**The test that matters** is in slice 2 and it is not a unit test: mount the same nest twice, run a
backfill, and assert the RPC request count is that of *one* backfill. Every other guarantee in this
RFC is a consequence of that one being true.

> **Amended after building slices 1-4 (2026-08-04).** Four things this section got wrong, kept here
> rather than quietly corrected, because the corrections are the useful part.
>
> 1. **The "test that matters" above does not discriminate.** RFC-0012's shared cursor already fetches
>    the *union* of its nests' logs once per window and demuxes, so two *distinct* nests on one cursor
>    already cost one nest's worth of RPC chatter. The count is identical whether or not datasets are
>    shared, and the test would have passed against the broken implementation. It is still asserted as
>    a regression guard, against a single-mount control run rather than a magic number. What actually
>    discriminates: one dataset directory on disk, `Arc::ptr_eq` on the two stores, identical sealed
>    rows through both doors, and the second mount charged zero RSS. Related: the pre-slice-2 shape did
>    not silently double-index - it called `Store::open` on one redb file twice and redb refused, so it
>    could not come up at all.
> 2. **Slice 2 could not "replace `RoostMeta::nests`" and slice 3 had to.** The alias is unique only
>    *within* a tenant, so `(acme, usdc)` and `(globex, usdc)` cannot be expressed by a flat list of
>    names at all. `[[mounts]]` became authoritative in slice 3, one slice earlier than planned. The
>    config-key *rename* still belongs to slice 5.
> 3. **A half-migrated roost needs `nests` and `[[mounts]]` unioned**, not one superseding the other -
>    `migrate` produces exactly that state whenever it refuses a nest, and treating the records as the
>    whole truth silently unmounts the nests that failed to migrate.
> 4. **Health had to follow the sharing.** Only the canonical mount is ever quarantined, so an alias
>    reporting its own state answers "indexing" while nothing is - the false-healthy report `/ready`
>    exists to prevent. `register_alias` records the sharing and `status()` resolves through it.
>
> Slice 4's criterion, by contrast, was exactly right to insist on measurement: a remount that quietly
> re-indexed leaves the same rows, the same watermark and the same content-hashes behind, so every
> obvious assertion passes against the broken version. The shipped test counts source requests across
> first-backfill / remount / prune-and-rebuild, and the third leg is what stops the second passing for
> the wrong reason.

## 10. What this does not change

- **One cursor per chain, single-writer, one observable failure boundary.** RFC-0021 stands unchanged.
  Tenancy is a property of a *mount*, never of a cursor. Two tenants on one chain share a cursor
  exactly as two nests do today.
- **Reorgs touch only the mutable hot store.** Sealed segments stay immutable and append-only, which
  is what makes union-of-ranges (§7) safe.
- **The nest bundle.** `blob.rs` is untouched and becomes *more* load-bearing, since the NID is what
  makes refcounted sharing sound. "Retire the roost" must never be read as "touch nest packaging".
- **The ≤2 GB per-cursor budget.** Sharing datasets reduces footprint; it never raises the ceiling.
- **Fault quarantine.** RFC-0026's taxonomy is per-nest and per-cursor and needs no tenant awareness.

## 11. Non-goals, stated so they are not read in

Nuthatch knows a tenant is a string. That is the whole of it. Explicitly **not** here, and not later
under this RFC's name:

- Per-tenant authentication, authorisation or identity. The gateway's job.
- Per-tenant quotas, rate limits, metering or billing. Out of scope by CLAUDE.md and unchanged.
- Isolation between mutually-untrusting paying customers. A shared dataset is shared; see §7.
- Any claim that tenants are "cooperating". The runtime cannot check that and never could - which is
  why the scope line was redrawn on 2026-08-04 to describe what nuthatch *does* (refcount an opaque
  string) rather than who the tenants *are*.

## 12. Risks

**The migration is the dangerous part.** It moves an operator's indexed history between directory
layouts. Mitigations: `migrate` is atomic per nest, it is idempotent, and it refuses on any NID that
does not reproduce. A dry-run mode prints the full plan - every source path, destination NID and
merge - and changes nothing.

> **Amended after building it (slice 1).** This section originally specified copy-then-verify-then-
> swap. The implementation renames instead, falling back to copy-verify-remove only across
> filesystems - and source and destination are both under the roost root, so the fallback is
> effectively unreachable. A rename is *atomic*: it cannot produce the half-written destination the
> copy path exists to guard against, and it does not require double the disk of an indexed history.
> The requirement was never the mechanism, it was "never lose data"; rename meets it better. Recorded
> rather than silently substituted, because the RFC named a specific mechanism and a reader would
> otherwise find the code disagreeing with it.

**The NID could not be the blob hash, and slice 1 found out why.** §3 says the NID "is the content
address `blob.rs` already computes". Not quite: `Manifest::blob_hash()` includes `generator_version`
(`env!("CARGO_PKG_VERSION")`), which is correct for a *bundle* - it pins the producing binary - and
fatal for a *storage key*, since every nuthatch release would then re-key every dataset and re-index
the lot. `Manifest::nid()` therefore hashes the manifest with that field neutralised, under its own
domain separator. Nothing is lost: the generator's *behaviour* is already pinned by `registry_hash`,
which `load` regenerates from the inputs and asserts, so a binary that decodes differently still moves
the identity. The claim in §3 is otherwise unchanged - the hash is over the same canonical manifest of
the same authored inputs.

**A shared dataset makes one tenant's mistake visible to another.** Not a data-integrity risk (the NID
guarantees the inputs are identical), but a resource one: a heavy query is felt across a shared
dataset. RFC-0034 is the answer and should land close behind.

**The refcount being derived means the table must be durable.** Losing it loses the mapping from
datasets to who wants them - the data survives, but nothing is mounted. The table is written with the
same durability discipline as the hot store, and `migrate --rebuild` can reconstruct a
`tenant="default"` table from the datasets on disk.

## 13. Status

Draft. Every decision it encodes is agreed and logged (§2); nothing here is waiting on research or on a
question to anyone. It is written to be built.
