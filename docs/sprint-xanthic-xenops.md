# Sprint: xanthic-xenops

**Scoped 2026-10-03, not started. Starts when every `wakeful-wagtail` issue is closed and its release
is cut. Chief settled the open questions the same day; they are recorded at the foot.**

**No release reaches production without meeting production's queries first, and production says
within minutes when something breaks.**

On 2026-10-03 at 07:07 UTC, 4.1.1 was rolled onto the Lodestar box. Within minutes its allocations
nest refused the dashboard's join-heavy views with an out-of-memory error, five API routes went 502
and 27 of 58 Lodestar checks failed. Every CI gate had passed, because none of them runs an authored
view. The roll script reported every unit ok, because ready means indexing, not serving. No alert
fired, because nothing scrapes the nests. The dashboard's own monitor found it, and a rollback by
hand fixed it about an hour later.

Each of those three gaps is a sprint item. Burrmill gets the systematic version of the fault behind
it, so the third production failure of this kind does not come from an operator nobody tested.

## Definition of done

Every issue carrying the **`xanthic-xenops`** label, in `nightswatchhq/nuthatch` and in
`nightswatchhq/burrmill`, is closed, and no open PR is for one of them. A release candidate that
breaks one of Lodestar's or kittiwake's queries is reported red before it is rolled, and a deliberately broken unit on the
box pages a person within five minutes.

## Order

1. **A release candidate meets production's queries: #1749.** A copy of the allocations nest's
   sealed segments and the query set Lodestar and kittiwake really send, run on the ThinkPad under the
   production budget for every candidate and every burrmill rev bump, reporting back as a check that
   does not block the tag. A red result stops the roll, not the release. It would have caught 4.1.1
   on `lodestar_indexer_daily`. First, because it is the gate that was
   missing.
2. **The roll smoke-tests and reverts: #1750.** `deploy-nest.sh roll` runs a committed query file
   against each unit after its version check and reverts that unit on any refusal, one unit at a time.
3. **Production pages: #1714, #1720.** Prometheus and Grafana on the ThinkPad as already settled,
   with an alert on `/sql` refusals and out-of-memory errors alongside the nine documented rules; this
   morning's failure would have fired one at 07:08. And the `nest_busy` reds on the Lodestar monitor,
   held over from `vigilant-vole`.
4. **Burrmill holds no operator's input blind: burrmill #40.** Every operator that holds input
   without spilling, tested over string-view columns under a budget against the same input as text.
   Whatever fails gets the compaction rule or its own fix.
5. **The two engine divergences the dialect work found.** `BIGINT % -1` at the minimum returns 0
   where DuckDB raises an overflow, and HUGEINT refuses 10^38 where DuckDB holds it. File both from
   the `wakeful-wagtail` dialect PR's notes, then fix or refuse by name.
6. **A scheduled live-chain smoke: #1719.** Weekly `init`, `dev` and first query against a real
   chain from the published binary, timed against the two-minute promise. It is the same idea as item
   1 at the other end of the product.
7. **Release**, through the gate from item 1, rolled with the script from item 2, and watched by the
   alerts from item 3.

## Rules

- **A gate is done when it has failed.** Item 1 closes on a recorded failure of 4.1.1 against the
  query set and a pass of the release that fixes it. Item 2 closes on a recorded revert. Item 3 closes
  on a recorded page.
- **Operations work is committed**: scrape config, alert rules, the query sets and the ThinkPad job.
  Nothing lives only on a box.
- **Every fix lands with the test that was missing**, red first, mutation-checked.
- **The ThinkPad is now production-adjacent.** It runs the QoS nest, and after this sprint the
  release gate and the monitoring too. Anything heavy there is scheduled away from the QoS nest's
  budget, and its own health is on the dashboard it serves.

## Not in this sprint

- **The parity timer, #1713.** Chief: not yet.
- **The rest of the test-strategy list**: #1716, #1718, #1722, #1723, #1724, #1741. #1723's
  throughput floor is partly overtaken by item 1, which measures what consumers actually do.
- **Burrmill build time and footprint**, #6 and #7.
- **The undecided pair**, #1670 and #1671.
- The parked programmes and `docs/frozen-for-2027.md`.

## Settled by Chief, 2026-10-03

- **The release gate reports; it does not block the tag.** A red result stops the roll to production.
- **The query set is Lodestar's and kittiwake's first.** Others may join later.
