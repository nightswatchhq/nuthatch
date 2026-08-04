# RFC-0035: The 2.0 breaking surface - one migration, batched deliberately

- Status: **Draft** (2026-08-04)
- Author: Pete (cargopete)
- Date: 2026-08-04
- Depends on: RFC-0032 (retiring the roost - the reason 2.0 exists at all), RFC-0033 §9 (removal of
  `nest diff` / `nest upgrade`), RFC-0034 (the allowlist, whose refusal responses land in the HTTP
  review).
- Nature: **coordination RFC.** It designs almost nothing. It decides what breaks together, what
  explicitly does not break, and what an operator has to do about it.
- Origin: decision O-2 accepted a major version to retire the roost properly rather than carry a
  deprecated alias. Once a major is on the table, the question becomes what else should ride with it.
  Scope agreed in the 2026-08-04 session working notes (unpublished) §10.

## Abstract

Current version is 1.0.2. Retiring the roost (RFC-0032) cannot be done compatibly, so there is a major
coming. A major is a **rare licence to fix shape**, and unrelated breaking changes are better batched
into one migration than dribbled through 1.x as a series of deprecations nobody reads.

So 2.0 is deliberately not minimal. Its scope is:

1. **Retire the roost** (RFC-0032) - the headline, and the only unavoidable item.
2. **Config surface cleanup** - remove what is shipped-but-unused and what the roost leaves behind.
3. **HTTP route and response review** - one pass over the wire format, including the `provenance`
   block, which grafting changes the meaning of.
4. **Remove `nest diff` and `nest upgrade`** (RFC-0033 §9).

**SQLite is not in scope**, having been in an earlier draft of this list. See §5 - it was removed by
measurement, not by preference, and the record is kept because the original answer was given on an
assumption that turned out to be false.

## 1. Retiring the roost

Fully specified in RFC-0032. What it costs an operator, summarised here because this is the document
they will read:

- `roost dev` is gone. It exits with an error naming `nuthatch migrate`, not a silent alias.
- `roost.toml` stops being authored config and becomes a runtime-owned mount table.
- Storage moves from `<root>/nests/<name>` to `<root>/data/<nid>`. **Data is moved, never re-indexed.**
- Every existing deployment becomes `tenant="default"`. Nobody who does not want tenancy sees the word.

`nuthatch migrate --dry-run` prints the entire plan - every source path, destination NID, and merge -
and changes nothing.

## 2. Config surface cleanup

A major is the only chance to remove config without a deprecation cycle. Candidates, each to be
confirmed against the code during the slice rather than assumed here:

- **`starlark_config`** - RFC-0018 §2 retired the Starlark front-end on 2026-07-21. The module is still
  compiled into the binary (`lib.rs:64`) and reachable from nothing. Shipped-but-unused code is a
  maintenance tax and a source of confusion for anyone reading for the config surface.
- **Roost-shaped keys** - `RoostMeta`'s single-chain fields (`chain`, `chain_id`, `rpc_urls`) that
  RFC-0021's `[[chains]]` superseded, and the `nests` name list RFC-0032 replaces.
- **Anything with two ways to say it.** The rule for the slice: where two keys express one thing, keep
  the one RFC-0021 or RFC-0032 introduced and remove the older.

Deletions only. This slice adds no configuration.

## 3. HTTP route and response review

One pass over the serving surface, which has accreted across RFC-0008, 0010, 0016, 0025 and 0026
without ever being reviewed as a whole. Scope:

- **The `provenance` block** (`serve.rs:956`) must be reviewed against grafting. It currently stamps
  `as_of`, `sealed_through`, `source` and `registry_hash`. Under RFC-0033 a result may be served from
  data computed by a *different* NID that backdated to the same output - which is correct, and which
  the current stamp cannot express. An agent citing an answer needs to know what actually produced it.
  This is the one item in the review with a design question inside it.
- **Route consistency** - `/entity/{id}` versus `/entities`, `/table/{name}` versus `/tables`,
  `/balance/{address}` versus `/balances`. The pattern is consistent; the review confirms it holds
  everywhere and that nothing is a historical accident.
- **Status codes.** RFC-0032 §7 and RFC-0034 both add refusals; they should join the existing scheme
  (the `503`-not-`400` reasoning at `serve.rs:965` is the standard to match) rather than invent one.
- **The tenant path segment** - present in multi-tenant mode, absent in single-tenant, so today's URLs
  are unchanged for everyone not using tenancy.

## 4. Removing `nest diff` and `nest upgrade`

Both are subsumed by grafting. RFC-0033 §9 carries the one thing that is not automatically
replaced - the compatible-versus-breaking classification - and moves it to **mount-time detection**:
the runtime refuses or warns, rather than an operator remembering a command.

Removal happens in 2.0; the mount-time detection must land **with or before** it, or the capability
disappears for a release.

## 5. Why SQLite is not here

An earlier version of this scope read "retire the roost + SQLite (gated) + config cleanup + HTTP
review". The gate was whether DuckDB could `ATTACH` a SQLite hot store, which would have collapsed the
tip materialisation and its 2,000,000-row ceiling into a plain query.

**Measured on 2026-08-04: it cannot.** `duckdb-rs` exposes no `sqlite` feature, and against an empty
extension directory both `ATTACH ... (TYPE SQLITE)` and `LOAD sqlite` fail under
`install_mode = REPOSITORY`. The only route is a runtime download, which non-negotiables 1 and 3
forbid.

With the prize unavailable, SQLite is tidiness only - one mental model and nicer debugging, paid for
with a rewrite of the hot path - and it loses to the work actually queued. **redb stays.** The decision
is recorded rather than silently dropped, because the scope was agreed while the question was open.

The probe also found an unrelated live bug: DuckDB was downloading the `parquet` and `json` extensions
at runtime, a phone-home on the sealed-segment read path and a hard failure when air-gapped, invisible
to CI. Fixed and guarded in PR #318, shipped in 1.x rather than held for 2.0.

## 6. What explicitly does not break

Stated because a major invites the assumption that everything is fair game:

- **The nest bundle format.** `blob.rs`, the manifest, the `registry_hash` check - untouched, and made
  *more* load-bearing by RFC-0032. A nest packed by 1.x mounts on 2.0.
- **Segment format.** Sealed Parquet is immutable and stays readable. If 2.0 required rewriting sealed
  segments, the design would be wrong.
- **The five non-negotiables.** Single static binary, ≤2 GB per cursor, no phone-home, determinism in
  the core, `MIT OR Apache-2.0`.
- **One cursor per chain.** RFC-0021 stands entirely.
- **The single-nest developer experience.** `nuthatch init` → `nuthatch dev` → `nuthatch sql` behaves
  as it does today, on the same URLs.

## 7. The stability commitment

Issue #312 recorded that `docs/operators.md` claimed a `0.x` stability contract while the project was
at 1.0.1, and that the semver commitment had never actually been published. 2.0 is the moment to
publish it, because the first major bump is when a commitment is either demonstrated or discovered to
be absent.

What it should say is a separate piece of writing, but it must be published **before** 2.0 ships, not
alongside it - a stability promise that first appears in the release that breaks things reads as an
apology.

## 8. Release plan

| Step | Gate |
|---|---|
| Publish the stability commitment (§7) | Before any 2.0 branch |
| RFC-0032 slices 1-5 | Migration parity tests green; the one-backfill-for-two-mounts test passing |
| Config cleanup (§2) | Deletions only; no config added |
| HTTP review (§3) | Provenance answer decided; refusals consistent |
| Remove `nest diff`/`upgrade` (§4) | Mount-time detection landed first |
| `nuthatch migrate` | Dry-run verified against a real multi-nest deployment, not a fixture |
| 2.0.0 | Upgrade notes written from the migration, after running it |

The Lodestar production box is the migration's real test. A 2.0 that has not been migrated on a live
deployment with existing indexed history is not ready, whatever CI says - which is the lesson from
issue #250, where every control-plane check passed and no check asserted a row appeared.

## 9. Status

Draft. It coordinates rather than designs; the substance lives in RFC-0032, 0033 and 0034. It should be
the last of the four to be accepted, since its scope is the union of theirs.
