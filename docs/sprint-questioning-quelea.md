# Sprint: questioning-quelea

**Seven issues** - every issue open on 2026-09-02. A sprint is a labelled set, not a calendar.
Filed after [patient-plover](sprint-patient-plover.md) closed fifteen of eighteen. The three that
did not land, plus four found while doing that work, are this sprint.

## Definition of done

Every issue carrying the **`questioning-quelea`** label is closed, and no open PR is for one of
them. That is #1067, #1076, #1078, #1092, #1093, #1095 and #1097. Work discovered in flight is
filed **unlabelled**; pulling it into scope needs a board reply.

## The theme

**Does the completion claim survive?**

Plover made the Lodestar migration measurable: there is a denominator, the nest is deployed, the
decisions are written down. It did not switch anything, and it did not run the comparison. Four
things found in that work say why "Lodestar is on nuthatch" would currently be a lie:

- **Twenty-two of twenty-seven subgraph-calling API routes hit the gateway at request time**, not
  via Postgres (#1092). Completing the ingest layer and removing `GRAPH_API_KEY` 503s those pages
  immediately. #1086 named eight of them; the count is 22.
- **`api/subgraph-names` returns 200 with an empty body** when the key is absent (#1097). Twenty
  sibling routes 503. This one renders every subgraph nameless, which is a real ordinary state, so
  the page looks correct. Absence reading as success, in production this time.
- **Parity has never compared.** The refusal-half script is on main. The Graph Network half has
  not run, because `GRAPH_API_KEY` is a Vercel Secret and `vercel env pull` cannot read it (#1076).
- **Nothing has been switched** (#1078). The nest is live. The dashboard is not on it.

The other three are the same fault in the test suite and in CI, and the tip-path seal work already
in flight.

## The spine, in the order it has to run

The migration is sized by #1092, so that is first even though the nest is already up.

1. **#1092 - the 22 request-time routes.** Correct `docs/audits/2026-09-plan.md` §6 (the UI does
   *not* mostly read Postgres). Fold the 22 into #1074's denominator and #1078's completion
   condition. **Decide**, in this sprint, whether those routes migrate to a nest directly or move
   behind the Postgres cache first. They are two different jobs, and only one of them is what
   "migrate the ingestion layer" meant. Leaving the decision unmade makes #1078's done-state a
   number nobody can defend.
2. **#1097 - `subgraph-names` 200 {}.** Return 503 like the other twenty, or return the hashes
   unresolved with an explicit flag. `200 {}` is not defensible: it is indistinguishable from
   "these subgraphs genuinely have no names". Then sweep the same guard shape outside the 21
   `subgraphQuery` routes the filing already read. Lives in the Lodestar repo.
3. **#1076 - run the comparison.** `GRAPH_API_KEY` has to be readable (Development Config, or a
   mode-600 file; not chat). Then `GRAPH_API_KEY=… NEST_URL=http://127.0.0.1:8105 bash
   scripts/lodestar-parity.sh`. An absent comparison must not read as agreement; that half of the
   script already fails closed. The subgraph half has never executed.
4. **#1078 - switch group C**, once #1076 is a match or a recorded DIFF, and once #1092 has folded
   the 22 into the completion condition. The defensible claim remains #638's: the gateway key is no
   longer load-bearing for Lodestar's own dashboard. Not zero `GRAPH_API_KEY` in the repository.

## What runs in parallel, and does not wait for the spine

5. **#1067 - batch the tip path's seals.** PR 1098 is already in flight (auto-merge armed, Jules
   green). Land it. Do not stall the spine on it, and do not reopen the cursor-hold question Jules
   already rejected: segment identity must not depend on co-tenants. The multinest RAM job is the
   cap (372 MB vs 2048).
6. **#1093 - the #1015 regression test drives a copy of the production loop.** Revert the three
   production `while let` to `if let` and the suite stays green, because the test reimplements the
   loop inside the test module and never calls the shipped path. Extract the production loop into
   a helper the three sites and the test all use, or assert through the integration path that
   already seals. Then mutation-check it: `if let` must go red, and the failure gets quoted in the
   PR. Coupled in spirit with #1067 (same seal loop) and not blocked on it.
7. **#1095 - `PROTECTION_READ_TOKEN` was never set.** Board-only. The check that watches required
   contexts has been red on `main` for a week because the secret does not exist, and it is not
   itself a required context, so nobody saw. Create the secret, decide whether the check should be
   required, and do not leave a permanently red non-required job as scenery. Agents must not
   attempt this.

## The call

**All seven stay.** They are every issue currently open. #1067 stays even though it is p2 and
already in a PR: pulling it out of plover and not putting it here is how a coupling gets
forgotten, and #1093 is the next layer of the same seal-loop guard.

**#1092's decision is taken in this sprint, not deferred.** Nest-direct versus Postgres-cache-first
changes what #1078 has to do, and a completion claim that does not include the 22 will not survive
removing the key.

## Explicitly not in this sprint

- Every `frozen` issue. The 2026 feature freeze remains intact and **both carve-outs are spent**.
  RFC-0042 is parked to 2027-09-01 with four reopen triggers; a proposal to resume it is a proposal
  for a third carve-out.
- New engine, chain, extraction or AI capability.
- Anything that makes nuthatch care about *who* a tenant is. Multi-nest tenancy is an opaque label
  and a refcount; per-tenant authz, quotas and billing stay the gateway's job.
- New findings discovered while doing these seven, unless the board adds them explicitly.

## How this sprint runs

**A test that passes proves nothing until it has been made to fail.** Mutate the gate, watch it go
red for the right reason, and quote the failure in the PR. #1093 exists because that rule, applied
to #1068's replacement, found a test that asserts a hand-written copy of the code.

**Anything worth remembering has an open issue.** A closing sentence is not a queue.

**An absence is not an answer.** #1097's 200 {}, #1076's subgraph half that has never run, #1095's
check that cannot read protection: the same fault, in a route, a script, and CI.

**The migration's completion claim is a sentence somebody has to be able to defend out loud.** #638
already wrote the defensible version. #1092 is what makes that sentence true or false.
