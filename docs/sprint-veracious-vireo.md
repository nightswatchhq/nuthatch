# Sprint: veracious-vireo

Filed after unhurried-urial (#811), sprints-are-groups (#810) and tenacious-thrush (#809) landed.
**Five issues.** A sprint is a labelled set. It has no calendar.

## Definition of done

Every issue carrying the **`veracious-vireo`** label is closed, and no open PR is for one of
them. That is five issues: #661, #687, #776, #789, #807. Work discovered in flight is filed
**unlabelled**. Pulling it into scope needs a board reply.

## The theme

**A healthy process that looks stuck, and a current binary described as an old one.**

`/ready` reporting WAITING through a seal-direct pass that is fetching, decoding and sealing is the
same class as launch copy that still says there is no `eth_call` executor. Both surfaces tell a
stranger the product is not doing the thing it is doing.

Freeze-legal throughout: bug, documentation, observability of a phase that already exists. Not
RFC-0040. #807 was frozen; the board unfroze it into this sprint.

## The five

### 1. #661 - launch copy still describes 2.5.0

The RFC-index half is done: 0030 and 0031 rows already say Implemented. The launch half is not.
`docs/launch/show-hn.md` still says a pinned `eth_call` has no executor, IPFS-derived entities are
not indexed, and signs off as v2.5.0. `port-queue-nest.md` still says RFC-0037 cannot resolve.
`community.md` still says events-only is reasoned from first principles. RFC-0037 and RFC-0023
tier 3 shipped in 2.6.0.

**Acceptance**

1. Those three launch files do not claim that `eth_call`, IPFS, or the chain set is unbuilt.
2. `show-hn.md` does not sign off as v2.5.0.
3. A test, or a grep the RFC-index gate already runs, fails if the three stale phrases return.

### 2. #687 - RFC-0023's own header still says tiers 1-2 building

The index row is current. The document is not. Status still reads **tiers 1-2 building** and
pending tier 3. Tier 3 shipped in v2.6.0. 0030 and 0031 headers look current; confirm rather than
assume. Two `config.rs` comments in the issue, same vintage.

**Acceptance**

1. RFC-0023's status line names tier 3 as shipped, not pending.
2. A test fails if that status line contains `building` or `Pending: tier 3`.
3. The two `config.rs` comments named in the issue no longer describe the pre-2.6.0 refusal.

### 3. #776 - the obib-case6 command still omits `--window-adaptive`

Leftover p1 from starling. The artefact is `--seal-direct --window-adaptive`. The README in
`nightswatchhq/obib-case6` still publishes `--seal-direct` alone, which is the fixed-window arm
after #758. That PR is open. Closing this is finishing starling, not starting capability.

**Acceptance**

1. The obib-case6 README names `--seal-direct --window-adaptive` and a nuthatch version ≥ 2.7.1.
2. This issue closes when that README change is merged, not when a comment is left.

### 4. #789 - a seal-direct `[[calls]]` bench that failed once in CI and nowhere else

`bench::tests::seal_direct_bench_resolves_declared_calls` failed on the `test (exex)` leg of
PR #785 with `calls_resolved = 0`. The sibling `test` leg of the same run passed. It will not
reproduce locally. A gate that is green because we could not catch it is raven's failure mode
with a flake instead of a missing check.

**Acceptance**

1. The test cannot share a stub HTTP server, a work dir, or a port with a sibling running in the
   same process. Isolation is the mechanism, not a retry.
2. Deleting the isolation (or the `calls_resolved > 0` assertion) fails the test.
3. We do not close this by re-running until green.

### 5. #807 - seal-direct backfill looks like WAITING

`nuthatch dev --seal-direct` streams history to Parquet before the hot cursor exists. During that
pass `/ready` and the cursor metrics stay at zero, so a TUI shows WAITING for a healthy
multi-minute backfill. The systemd log has the progress; scraping logs is not an API contract.

Unfrozen for this sprint: the phase already exists. This exposes it. It is not a new extraction
mode, not RFC-0040, and not a second cursor.

**Acceptance**

1. A fresh `--seal-direct` run reports monotonically advancing progress through HTTP (Prometheus
   gauges and the corresponding `/ready` fields: active, origin, highest completed, final target)
   before the tip cursor begins.
2. After handoff to tip-following it reports inactive and preserves the completed target.
3. A test fails if those fields stay at zero through a seal-direct pass that actually sealed
   rows. Deleting the gauges fails it.

## Explicitly not in this sprint

- **RFC-0040** and anything still `frozen`. #812 (process/storage/RPC metrics for the TUI) is
  new capability; freeze it, do not pick it up.
- **#698**, the live site. No deploy automation, needs a Vercel token. Board-only.
- **#790**, the tyre-kicking pass. Someone without today's scars runs it. Filing its findings in
  advance would be inventing them.
- **#649 / #638 / #305**, Lodestar.
- **#750**, the VPS.
- **#286**, the 2 GB budget.

## How this sprint runs

Standing from nocturnal-nightjar, unchanged:

1. **Scope is the board's.** Discovered work is filed unlabelled.
2. **`Reviewed-by:` names the party who read the diff.**
3. **Acceptance is above.**

Also standing: never `git add -A`; do not `@`-mention Rowan in GitHub markdown; one merge per
CI cycle. `Closes` is one keyword per issue, not a comma list - squash only honours the first.
