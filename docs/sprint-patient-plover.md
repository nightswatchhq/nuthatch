# Sprint: patient-plover

**Eighteen issues** - every issue open on 2026-09-01. A sprint is a labelled set, not a calendar.
This is a bigger set than `observant-osprey`'s six because the board's standing rule filled the
backlog deliberately: anything worth remembering got an issue rather than a closing sentence, and
this is the bill for that.

## Definition of done

Every issue carrying the **`patient-plover`** label is closed, and no open PR is for one of them.
That is #1067, #1068, #1069, #1070, #1071, #1072, #1074, #1075, #1076, #1077, #1078, #1079, #1080,
#1081, #1082, #1083, #1084 and #1086. Work discovered in flight is filed **unlabelled**; pulling it
into scope needs a board reply.

## The theme

**100% of what?**

The goal is stated plainly and repeatedly: Lodestar's data comes from nuthatch nests on the VPS
rather than from subgraphs through the gateway. Eleven of these eighteen issues are that migration,
and the sprint exists because the goal is currently an **intention rather than a measurable state**.

Four things are true at once, and each one on its own would sink the claim:

- **There is no denominator.** Nobody has written down what Lodestar actually reads, so "how far
  along are we" can only be answered by feel, and this project does not do feel (#1074).
- **The views Lodestar needs live in a nest nobody can query.** `graph-allocations-nest` carries
  `40-lodestar-allocations.sql`, `50-lodestar-epochs.sql` and the rest. Checked on the box on
  2026-09-01: it is **not deployed anywhere**. It exists as a local repository on one laptop, with a
  locally-built `nuthatch.redb` beside it (#1075).
- **Parity was proven once, by hand, at a moment nobody recorded.** A view that matched the subgraph
  in July is not evidence about today - the chain moved, the subgraph was reindexed, and nuthatch has
  had eleven releases since (#1076).
- **There is no answer for a nest being down.** `.env.example` says it out loud:
  `NUTHATCH_DIPS="false"   # no fallback exists`. That is survivable for one surface behind a
  default-off flag. It is not survivable as the pattern for the several surfaces #1078 switches, and
  the box restarted seven services this morning (#1080).

Two of the remaining seven are the same fault pointed at the public record: the site advertises
**v3.0.0** and calls it "the current, executable path" while v3.1.0 - carrying a security fix -
has shipped (#1072), and the only report of a fresh operator working the product blind exists on
**one laptop** because committing it reds CI (#1070).

The rest is the performance trio and the eval re-run. They are honest work and they do not belong to
the theme; see the note at the bottom.

## The spine, in the order it has to run

Everything in the migration is sized by the first item, so the ordering is not a preference.

1. **#1074 - the inventory.** Every data dependency in Lodestar, with source-today, on-chain?,
   nuthatch-status and blocker. That table **is** the migration plan and its bottom line is the
   honest percentage. It needs a stated denominator and an **out of remit** row, because "100% of the
   on-chain data" is achievable and worth chasing, while "100% of everything" is a number we could
   only reach by lying about what the rest is (#638 set that boundary).
2. **#1075 - deploy `graph-allocations-nest`, after deciding the duplication.** It declares eight
   contracts including `staking` and `gns`, and the box already runs `graph-staking-nest` and
   `graph-gns-nest` against those same contracts. Deploying as-is indexes the same events twice:
   two cursors and two hot stores against a 2 GB per-cursor budget already sitting at ~1352 MB, and
   two sets of RPC calls on an endpoint whose cost is documented. Consolidate onto the multi-contract
   nest and retire the singles, or keep them apart for blast radius and accept the duplication
   knowingly. **Both are defensible. Drifting into the second by not deciding is not.**
3. **#1076 - continuous parity, at a pinned block.** Per module, on a timer, recording both results,
   the block, the date and the nuthatch version, and the differing *rows* on disagreement. Two traps
   this project has already fallen into: a live subgraph and a live nest are never at the same tip, so
   an unpinned comparison produces disagreements that are only lag and teaches everyone to ignore the
   alert; and **an absent comparison must not read as agreement** - a check that cannot reach the
   gateway, or finds no rows on either side, fails loudly.
4. **#1078 - migrate the network-state surfaces**, once there is something to point them at.

## What runs in parallel, and does not wait for the spine

5. **#1080 - decide what happens when a nest is down.** A readiness gate off `/ready`
   (`lag_blocks`, `sealed_through`, `cursorless`, all trustworthy since 3.1.0 fixed #1020 and #1025),
   a per-surface decision to fall back or fail visibly, and staleness carried in the response.
   Silently serving stale data is not on the menu, and neither is a blank page with no explanation.
   This is the work that decides what "switched over" operationally means, and #1078 is blocked
   anyway, so it costs nothing to do it first.
6. **#1082 - `delegations` has no view; `escrow` has a view and no cron path.** Four of six cron
   subcommands line up with a nest view exactly. Two do not, and one has the reverse problem. The
   `delegations` events are plainly on-chain and the contracts are already declared, so this is a
   missing view rather than missing data.
7. **#1084 - the Horizon activity cron**, a live gateway-backed ingestion path outside
   `src/lib/ingest/*` that the inventory misses and that runs every two minutes.
8. **#1086 - the direct network-state API routes** #1078's table omits. Eight handlers that call
   `src/lib/subgraph.ts` directly and stay load-bearing on `GRAPH_API_KEY` even after the scoped work
   finishes.
9. **#1079 - write down the gateway surfaces that are *meant* to stay**, so they stop reading as debt
   and start reading as the denominator's out-of-remit row.
10. **#1083 - classify `qos.ts`**, the heaviest gateway consumer and probably out of remit. Deciding
    that on the record is the point; 26 references is too many to leave ambiguous.

## The box

11. **#1077 - four nest directories** on the VPS are inactive, undocumented and pinned to an
    unversioned binary. #1060 versioned every `ExecStart` that is running; these are the ones that
    are not.
12. **#1081 - what has to be true to hand GraphOps a running system** rather than a rebuild. Chris
    hosts it after we have migrated and verified, so the handover conditions want writing down while
    the migration is being done, not after.

## The public record

13. **#1072 - the site is two releases behind.** The hero tag reads `v3.0.0`, and
    `example.astro` calls it "the current, executable path" twice more, in the page description and
    so in the meta tags. v3.1.0 carries a security fix (wasmtime 46.0.3, RUSTSEC-2026-0268/0269) and
    two production defects. This is not merely stale: it points people at a build we would not want
    them on. Note that a site push is **not** a deploy - `nuthatch-frontend` has no git integration.
14. **#1070 - commit the tyre-kicking report.** `doc_command_check` reads `3.0.0-alpha.1` on its
    version line as a subcommand and reds CI, which is precisely why nobody has fixed it: the file is
    invisible to everyone except whoever holds that laptop. Fix the checker or the line, then commit
    the artefact.

## The tail, and it is honestly a tail

15. **#1071 - re-run the Tier-B eval** now the harness records *why* each answer failed rather than
    only that it did.
16. **#1067 - batch the tip path's seals.** 80% of segments are under 20 KB and every query pays for
    them.
17. **#1068 - the seal-boundary test greps a comment**, and #1067 changes the code it guards. **These
    two are coupled**: #1068 must land with or before #1067, or the guard is green while guarding
    nothing - which is this project's most-repeated defect and has its own standing rule below.
18. **#1069 - profile the 1352 MB cursor.** Both previous guesses about where that RAM goes were
    wrong, including one of mine where the synthetic benchmark had the sign inverted. Measure it
    rather than reasoning about it.

## Explicitly not in this sprint

- Every `frozen` issue. The 2026 feature freeze remains intact and **both carve-outs are spent**.
  RFC-0042 is parked to 2027-09-01 with four reopen triggers; a proposal to resume it is a proposal
  for a third carve-out.
- New engine, chain, extraction or AI capability.
- Anything that makes nuthatch care about *who* a tenant is. Multi-nest tenancy is an opaque label
  and a refcount; per-tenant authz, quotas and billing stay the gateway's job.
- New findings discovered while doing these eighteen, unless the board adds them explicitly.

## How this sprint runs

**A test that passes proves nothing until it has been made to fail.** Mutate the gate, watch it go
red for the right reason, and quote the failure in the PR. #1068 is in this sprint because that rule
caught a guard that greps prose.

**Anything worth remembering has an open issue.** A closing sentence is not a queue. This sprint is
eighteen issues rather than six precisely because that rule was applied, and that is the rule
working, not the backlog failing.

**An absence is not an answer.** It appears three times in this scope alone - #1076's parity check
that cannot reach the gateway, #1070's report that exists nowhere, #1074's percentage with no
denominator - and it is the same fault every time: a mechanism that reports success because it never
looked.

**The migration's completion claim is a sentence somebody has to be able to defend out loud.** #638
already wrote the defensible version: the gateway key is no longer load-bearing for Lodestar's own
dashboard. Not every feature reaches zero `GRAPH_API_KEY`, and pretending otherwise would make the
number worthless.
