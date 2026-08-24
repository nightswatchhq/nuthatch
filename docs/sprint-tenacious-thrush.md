# Sprint: tenacious-thrush

Filed while quizzical-quail (#805) and steady-starling (#808) were still landing.
**Four issues.**

## Definition of done

Every issue carrying the **`tenacious-thrush`** label is closed, and no open PR is for one of
them. That is four issues: #745, #713, #751, #764. Work discovered in flight is filed
**unlabelled**. Pulling it into scope needs a board reply.

## The theme

**A suite that stays green when you delete the mechanism is not a suite.**

Raven made the checks that cannot fail stop pretending. Starling made the storage-path number
come off a tape. What still sits in p2 is the same generator one file over: a production path
with no test, a hand list the next config file will escape, a report that omits the axis that
cost a day, and a seeded description that names an event that does not exist.

Freeze-legal throughout: bug, verification, a gate, documentation. Not RFC-0040.

## The four

### 1. #745 - `backfill_direct_factory` call resolution can be deleted and 659 tests pass

**The path.** #742 threaded `calls` / `state_rpc` / `chain_id` into all three seal-direct
paths. Mutating the `if let Some(rpc) = state_rpc` branch dead, one path at a time:

| seal-direct path | suite | caught by |
|---|---|---|
| `backfill_direct` | RED | `bench::tests::seal_direct_bench_resolves_declared_calls` |
| `backfill_direct_pipelined` | RED | `indexer::tests::seal_direct_with_declared_calls_resolves_and_seals_them` |
| **`backfill_direct_factory`** | **green, 659 passed** | **nothing** |

This is the production seal-direct path for a nest with `[[templates]]` / `[[factories]]`. A
factory nest declaring `[[calls]]` is the shape most likely to reach it. Missing call rows
look like a successful run. #725's failure mode, one path over.

**Acceptance**

1. A test exercises `backfill_direct_factory` with a declared `[[calls]]` and asserts at least
   one `eth_call` reached the state RPC, same shape as
   `seal_direct_with_declared_calls_resolves_and_seals_them`.
2. Making the factory path's `state_rpc` branch dead fails that test. The other two paths
   staying red is not enough.

### 2. #713 - `CONFIG_SOURCES` is a hand list with no completeness check

**The list.** `config_reference_names_every_real_config_key` scans four files named in
`tests/skill_refs.rs`. `allowlist.rs` was missing until #706 added it by hand. Nothing asserts
the list contains every file that holds a `Deserialize` config struct. The next one escapes
the same way.

**Acceptance**

1. A test fails if a `src/*.rs` file defines a `Deserialize` config struct and is not named in
   `CONFIG_SOURCES` (or an explicit, reasoned opt-out).
2. Deleting the `allowlist.rs` row fails that test. Adding a new `Deserialize` struct in a
   fifth file, without listing it, fails it too.

### 3. #751 - a bench event count is not comparable without the declared event set

**The axis.** 11,758 and 12,933 were both correct. One nest declared `Transfer` only; the
other declared every event in the ABI. `BenchReport` carries provider, hardware, commit, and
now the tape address. It does not carry which events the nest asked for. The house rule
gained a fourth axis and then did not record it.

Starling closed the 11,758 question. It did not make the next disagreement cheap.

**Acceptance**

1. `BenchReport` carries the nest's declared event set (names, not a count). An undeclared
   "all events in the ABI" nest is distinguishable from a `Transfer`-only nest in the artefact.
2. Reintroducing two reports over the same range, same commit, different event sets, with no
   field that separates them, fails a test.
3. Existing committed `docs/bench/*.json` artefacts either gain the field or are documented as
   predating it. The gate does not ship green over a file it cannot defend.

### 4. #764 - `semantic.toml` seeds a call table as an empty event

**The sentence.** `nuthatch schema` writes, for a `[[calls]]` result table:

```toml
description = "`` events emitted by the `token0_symbol` contract."
grain = "one row per  event"
```

A call table has no event. The name interpolates empty. The "contract" is a declaration name.
The grain has a double space where the event should be. `llms.txt` and the scaffolded skill
inherit it.

**Acceptance**

1. Seeding a `[[calls]]` table does not use event-table copy. The description names a call
   result, not an event, and not a contract that does not exist.
2. A fixture with one `[[calls]]` entry, run through the real seeder, fails if the seeded
   `description` or `grain` contains a doubled space or an empty event name.
3. Event tables keep their current seed. This does not rewrite operator-edited prose.

## Explicitly not in this sprint

- **RFC-0040**, the freshness dial. Design, freeze.
- **#807**, seal-direct progress gauges and `/ready` fields. Unlabelled, and it is new
  surface. Say so rather than quietly build it.
- **#750**, the Lodestar VPS still on 2.5.0. Ops.
- **#649 / #638 / #305**, Lodestar product.
- **#286**, the 2 GB budget under a hostile ABI. A live run, not four tickets.
- **#789**, the one-off exex flake. Quail's work-dir isolation may already have been it.
- **quizzical-quail's four** and **steady-starling's three.** They close on their own PRs.
  Do not restack this on those branches. `#751` touches `BenchReport`; rebase onto 808 when
  it lands rather than stacking now.
- **Anything labelled `parked`.**

## How this sprint runs

Standing from nocturnal-nightjar, unchanged:

1. **Scope is the board's.** A label is not approval to grow the set. Discovered work is filed
   unlabelled.
2. **`Reviewed-by:` names the party who read the diff.** No proxy signatures.
3. **Acceptance is above.** Build against it, do not rediscover it in review.

Also standing: one worktree per run; never `git add -A`; do not `@`-mention Rowan in GitHub
markdown; `CFLAGS=-std=gnu17` on the Linux box; one merge per CI cycle.

## Context at filing

v2.7.1 is what `curl | sh` installs. Raven is on main (`9b3bb6f`). Quail is #805, starling is
#808. The four above were already open; #745 was named when #725 closed two of three paths.
