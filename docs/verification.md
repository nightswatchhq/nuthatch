# Verifying a nuthatch deployment

A falsifiable acceptance runbook. Every step has a **command**, an **expected result** you can compare
against, what it **proves**, and what a failure means.

This is deliberately not [`operators.md`](operators.md), which tells you *how to run* nuthatch. This
tells you *how to prove it works* — for your own sign-off, or so a second operator can independently
confirm the claims this project makes rather than taking them on trust.

## Run it, don't read it

Most of this document is executable:

```sh
./scripts/verify.sh 0 1 2      # artifact, single nest, correctness
./scripts/verify.sh 5          # scaled mode (needs a fleet up)
./scripts/verify.sh all --strict
```

Each check maps to a numbered step below, asserts a concrete result, and prints what failed with its
output. **A skip is not a pass** - steps whose prerequisites are absent are counted separately, because
a green run that silently skipped the interesting half is worse than a red one. `--strict` turns any
skip into a failure, which is what CI uses.

The steps that genuinely need a human are marked as skips with the procedure attached rather than
faked: a restart drill, a deliberate reorg, breaking a nest to watch its co-tenants survive, and
comparing a row count against an independent source. That last one is the step separating *it ran* from
*it is correct*, and no script can do it for you.

CI runs the fleet half on every push - `the compose fleet comes up` stands the whole stack up, walks
level 5, and asserts exactly one worker takes the lease.

## How to use it

Levels are independent and cumulative. **Run the levels that match what you deploy** — level 5 is
irrelevant if you run a single nest, and levels 1–4 still matter if you run a fleet.

Every step is written so a *failure is unambiguous*. If a step says "expect `ok`" and you get something
else, that is a finding worth reporting, not something to interpret. Wherever an expected value depends
on your chain or contract, the step says so.

**Please report results, including passes.** A level someone actually ran is worth more than a level we
assert. Open an issue with the level, the step, what you expected and what you got.

### What we have verified ourselves

Stated plainly so you know which steps are re-confirmation and which are genuinely new evidence:

| Level | Verified by us | On what |
|---|---|---|
| 0 Artifact | yes | every release |
| 1 Single nest | yes | CI + the Lodestar production box |
| 2 Correctness | yes | CI (deterministic fixtures, property tests) |
| 3 Roost | yes | live two-chain run, 8-nest density run |
| 4 Guards | yes | CI + a live `/sql` adversary check. **4.4 is CI-only so far** - the flip refusal and the schema-version stamp are covered by tests; no one has yet run a timestamp-free nest over a long backfill and timed it, so we publish no speed figure for it |
| 5 Scaled mode | **mostly** | 41 tests against a live Postgres, **plus a full level-5 pass on a clean Hetzner box** (2026-07-30, Ubuntu 24.04, published v0.8.1 artifacts, 2 writers + 2 FE nodes): **10/10, zero skipped**, via `scripts/verify.sh 5`. **Nothing has run across real machines** - the partition and clock-skew cases are still open. |

Level 5 is where independent verification is worth the most, for exactly that reason.

---

## Level 0 — the artifact is what it claims

**0.1 Version matches the tag you downloaded**

```sh
nuthatch --version
```

Expect the version you fetched. *Proves* you are testing what you think you are. A mismatch usually
means a stale binary earlier in `PATH` — check `command -v nuthatch`.

**0.2 Checksum**

```sh
shasum -a 256 -c nuthatch-x86_64-unknown-linux-gnu.tar.gz.sha256
```

Expect `OK`. *Proves* transport integrity.

**0.3 It runs on your libc**

```sh
nuthatch --version && echo started-ok
```

Expect no `GLIBC_… not found`. The Linux build targets **glibc 2.34**, so RHEL 9, Debian 12,
Ubuntu 22.04 and newer are fine. *If this fails*, your distro is older than the floor; build from
source or use the container image.

**0.4 Embedded mode carries no database driver**

```sh
nuthatch worker --control-db x --hot-store y --chains z
```

Expect a refusal naming `--features postgres-store`. *Proves* the default artifact is the embedded one
(non-negotiable 1). The subcommand is *listed* in `--help` on purpose — a command that vanishes
depending on build flags is harder to diagnose than one that explains itself.

---

## Level 1 — a single nest, end to end

The under-two-minutes claim. Use any contract on any supported chain; a busy ERC-20 is easiest.

**1.1 Scaffold**

```sh
nuthatch init 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 --chain mainnet --dir /tmp/v-usdc
```

Expect a scaffolded nest, and a printed table count > 0. *Proves* ABI resolution and code generation.

**If it warns that no logs match the resolved ABI**, that is the check working: you have hit a proxy
whose implementation ABI the public resolvers did not return. Re-run with
`--abi path/to/implementation.json`. A nest that indexes nothing is the failure this warning exists to
prevent.

**1.2 Index and serve**

```sh
nuthatch dev --dir /tmp/v-usdc --backfill 2000
```

Expect progress output, then a bound listener. *Proves* ingestion, decode, storage and serving in one
process.

**1.3 The API answers**

```sh
curl -s localhost:8288/health          # expect: ok
curl -s localhost:8288/tables | head   # expect: a non-zero count and your tables
curl -s --get localhost:8288/sql --data-urlencode 'q=select count(*) from "usdc__transfer"'
```

Expect a non-zero count. *Proves* the decoded data is queryable. **Compare it against an independent
source** — a block explorer, or `cast logs` over the same range. Matching counts is the whole point;
this is the step that distinguishes "it ran" from "it is correct".

**1.4 Provenance is attached**

Expect every `/sql` response to carry a `provenance` block with `as_of`, `registry_hash` and
`sealed_through`. *Proves* answers are attributable to a specific decode version and a specific chain
position. `as_of` ahead of `sealed_through` means it is tip-following rather than serving only sealed
history.

---

## Level 2 — correctness under adverse conditions

The levels above prove it works. These prove it stays right when things go wrong.

**2.1 Restart with no gaps or duplicates**

```sh
curl -s --get localhost:8288/sql --data-urlencode 'q=select count(*) c from "usdc__transfer"'
# SIGTERM the process, wait for it to exit, start it again with the same command
# re-run the same count once it has caught up to the same block
```

Expect the count for a **fixed block range** to be identical. *Proves* checkpointed resume. A higher
count means duplicates; a lower one means a gap. Either is a serious finding.

**2.2 Invariants and parity**

```sh
nuthatch check --dir /tmp/v-usdc
```

Expect a pass. *Proves* the nest's committed fixtures still decode to the same entities — the
regression net for decode changes.

**2.3 A reorg converges** *(needs a chain that reorgs, or trust CI)*

Our property tests drive random reorg depths against the hot store and assert convergence to canonical
state. Reproducing that live requires catching a real reorg; if you want to verify it yourself, watch
for `reorg` in the logs and confirm the affected block range's row count matches the canonical chain
afterwards. *Proves* reorgs only ever touch the mutable hot store.

---

## Level 3 — a roost (many nests, one runtime)

**3.1 Co-tenancy**

```sh
nuthatch roost dev --dir /tmp/v-roost
curl -s localhost:8288/nests
```

Expect every mounted nest listed with its chain, registry hash and footprint, and each serving its full
API under `/<name>/`. *Proves* co-tenancy with per-nest routing.

**3.2 Nests on one chain share a cursor**

Expect one `getLogs` per window regardless of how many nests share a chain — visible in RPC-provider
metering, or in the logs. *Proves* the cost claim: N nests on a chain for roughly one nest's RPC spend.

**3.3 Multichain isolation**

With nests on two chains, expect two cursors, each with its own tip and finality. *Proves* a cursor is
never multiplexed across chains. Stalling one chain's RPC must not stall the other's ingestion.

**3.4 Mount and unmount without a restart**

```sh
curl -XPOST   localhost:8288/_admin/nests -d '{"name":"another"}'
curl -XDELETE localhost:8288/_admin/nests/another
```

Expect both to succeed **without co-tenants being interrupted** — check the other nests' `/ready` and
row counts across the operation. *Proves* the live roost: a configuration change no longer has a wider
blast radius than a fault.

Expect a `507` if the mount would breach the cursor's RAM budget, carrying the projected and ceiling
figures. That refusal is the feature; a budget that can be quietly exceeded is not a budget.

**3.5 Per-nest blast radius**

Break one nest deliberately — an invalid authored view is easiest. Expect that nest to be quarantined
and reported, and **every other nest to keep serving**. *Proves* isolation. A roost-wide failure here
is the most serious finding in this document.

---

## Level 4 — the guards

Each of these is a deliberate refusal. **A missing refusal is the finding**, not a convenience.

**4.1 `/sql` is read-only**

```sh
curl -s --get localhost:8288/sql --data-urlencode 'q=CREATE TABLE x(a int)'
curl -s --get localhost:8288/sql --data-urlencode "q=SELECT 1; COPY (SELECT 1) TO '/tmp/x'"
```

Expect both refused — the second because `;`-stacked statements are rejected. *Proves* the surface is
structurally read-only. A stacked `COPY TO` succeeding is an arbitrary file write.

**4.2 Filesystem access is refused**

```sh
curl -s --get localhost:8288/sql --data-urlencode "q=SELECT * FROM read_csv('/etc/passwd')"
```

Expect a refusal. *Proves* the file-access denylist.

**4.3 The hot-scan cap**

Past 2,000,000 unsealed rows expect `503` naming `sealed_through`, rather than the box running out of
memory. *Proves* the largest RAM risk is bounded. It **refuses rather than truncating**, because a
partial tip would silently change the answer to an aggregate.

**4.4 The timestamp declaration cannot be flipped**

```sh
nuthatch init 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 --dir ts-nest
nuthatch dev --dir ts-nest --backfill 500       # let it index, then stop
sed -i 's/block_timestamps = true/block_timestamps = false/' ts-nest/nuthatch.toml
nuthatch dev --dir ts-nest
```

Expect the last command to **refuse to start**, naming it a breaking schema change. *Proves* a nest
cannot end up holding two schemas — rows and segments written before the edit carrying
`block_timestamp` and everything after not, with nothing to say so until a query hits the wrong half.

Then the other direction, which is the one an operator actually wants:

```sh
nuthatch init 0xA0b8…eB48 --dir fast-nest --no-timestamps
grep -E 'schema_version|block_timestamps' fast-nest/nuthatch.toml
nuthatch dev --dir fast-nest --backfill 5000 --seal-direct
nuthatch sql --dir fast-nest 'SELECT * FROM usdc__transfer LIMIT 1'
```

Expect `schema_version = 2`, no `block_timestamp` column in the result, and a **visibly faster**
backfill than the same range with timestamps on. The v2 stamp is what makes a 0.8.x binary refuse this
nest rather than index timestamps into it — check that too if you have an old binary to hand.

**4.5 Admin exposure**

Bind off-localhost without `NUTHATCH_ADMIN_TOKEN` and expect the admin routes to be unavailable; with a
token, expect requests lacking it to be refused. *Proves* the admin surface is not reachable
unauthenticated off-localhost.

---

## Level 5 — scaled mode (a fleet)

**This is the level we most want independently verified.**

> **2026-07-31 - a genuinely distributed fleet ran, and the clock-skew invariant PASSED.**
>
> Control plane + store + FE tier on one box (`10.44.1.1`), **two workers on their own machines**
> reaching the store over a private network. Both registered -
> `{"count":2,"workers":[{"id":"59e57b46be66"},{"id":"928d24d4d604"}]}` - and the scheduler assigned
> the `arbitrum-one` cursor to a **named remote worker**, which took the lease with a fence.
>
> **skew: PASS.** The holding worker's clock was pushed **10 minutes** forward on its own machine. It
> neither lost the lease nor extended it by ten minutes: `expires_at` advanced **66 s**, the renewal
> cadence on the *database's* clock. RFC-0022's claim - expiry is evaluated by the store, so a wrong
> clock on a worker can neither win nor lose it a cursor - is now demonstrated across machines rather
> than argued.
>
> **partition: BLOCKED, and the blocker is itself a finding.** It needs `last_block` to advance, and
> the assigned worker never indexed: it logs `acquired cursors chains=["arbitrum-one"]` and then
> `runtime secrets injected for held cursors nests=0`. **A declared nest plus an assigned cursor did
> not produce indexing in the distributed topology.** The lease machinery demonstrably works across
> machines; the *ingestion* half remains unproven, and that gap should be chased before anyone claims
> the writer pool works end to end.
>
> Getting this far took six tooling fixes, every one found by running it rather than reading it: a 422
> network payload (so `multi` had never created a box), a missing capacity fallback, a 409 on a
> leftover network, error handling that hid all three behind `curl: (56)`, inconsistent SSH host-key
> policy against recycled IPs, and a `psql` helper that never `cd`'d to its compose file.

> **2026-07-30 - the two cross-machine cases were repaired before ever being run.** `partition` and
> `skew` printed their expectations for a human to read and asserted nothing, which hid two defects in
> the tests themselves: `partition` blocked the whole control-plane *host*, and since Postgres runs on
> that box in the `multi` shape it cut the writer off from its **hot store** too - making its own
> stated expectation ("the cursor STILL INDEXING") impossible to satisfy. Both now assert against the
> shared Postgres and exit non-zero on failure. **Neither has been run yet**; this table moves only
> when they have. Our 41 automated tests run against a live
Postgres and cover every invariant below, but **the compose stack has never been brought up end to
end**, and nothing has run across real machines. If you verify one level from this document, this is
the one worth your time.

Needs the **scaled** artifact — `…:<version>-scaled` or `nuthatch-scaled-…tar.gz`. The default build
refuses these commands by name.

**5.0 Prerequisites, both of which have bitten us**

```sh
nuthatch init 0xYourContract --chain arbitrum-one --dir nest
sudo chown -R 10001:10001 nest
```

The fleet mounts `./nest`. Without it, FE nodes exit and **writers keep running** - they take work from
the control plane, not from disk - so the failure looks like an FE bug rather than a missing directory.
Without the `chown`, FE nodes exit with `Permission denied`: the image runs unprivileged as uid 10001,
and a root-owned bind mount is unwritable to it.

That second one **passes on Docker Desktop and fails on Linux**, because Desktop fakes mount
permissions. If you are testing on a Mac and it works, that is not evidence it works.

**5.1 The stack comes up**

```sh
docker compose -f docker-compose.scaled.yml --profile fleet up \
  --scale writer=2 --scale fe=3
curl -s localhost:8290/health     # expect: ok
curl -s localhost:8290/workers    # expect: 2 workers with their budgets
```

*Proves* the topology: control plane reachable, workers registering, FE nodes up. We have run this on a
single host; **on separate machines it is still unverified**, and that is where a report helps most.

**5.2 Declaring a nest starts it, without a restart**

```sh
curl -XPOST localhost:8290/nests \
  -d '{"name":"usdc","chain":"arbitrum-one","estimated_rss_mb":120}'
curl -s localhost:8290/plan
```

Expect the declare to return `200` with a note that it is *desired state*, and the plan to assign the
chain to one worker. Within a tick (5s) expect that worker's logs to show `acquired cursors`.

*Proves* dynamic lifecycle. Note `200` means **told**, not **running** — the fleet converges on its own
tick. Conflating those is the most likely source of confusion at this level.

**5.3 Exactly one owner**

With two workers both able to host the chain, expect **exactly one** to log `acquired cursors` for it.
*Proves* the single-owner invariant. Two workers claiming one cursor is the most serious possible
finding in this document — it is the failure the lease and fence exist to prevent.

**5.4 Lease handover on writer loss**

```sh
docker compose -f docker-compose.scaled.yml kill <the owning writer>
```

Expect the other worker to acquire the cursor within a lease TTL (30s by default), and indexing to
continue. *Proves* failover. Then bring the killed worker back and expect it **not** to resume writing
the cursor it lost — its fence is stale, and the store refuses it.

**5.5 A stale writer is refused, not trusted**

Pause the owning writer long enough for its lease to expire (`docker pause`, then wait past the TTL),
let the other take over, then unpause it. Expect the resumed worker to log a **lost-ownership** error
and stop writing that cursor, rather than writing alongside the new owner.

*Proves* the fence. This is the invariant everything else rests on, and the one a comment in a config
file cannot deliver.

**5.6 Serving scales independently**

Expect every FE node to answer identically for the same query, and adding or removing FE nodes to
change no cursor's ownership and no ingestion progress. *Proves* the plane split.

**5.7 Versions resolve identically fleet-wide**

```sh
curl -XPUT localhost:8290/nests/usdc/pin -d '{"version":"1.0.0","bundle_hash":"0x…"}'
# then, against each FE node:
curl -s localhost:8290/nests/usdc/resolve
```

Expect the same version from every node. *Proves* the fix for the bug that only exists across machines:
if each node resolved `latest` itself, one would serve the new schema while another served the old, and
the same endpoint would answer differently depending on where the load balancer sent the request.

An unpinned endpoint must report `servable: false` — an FE refusing is correct, guessing is not.

**5.8 Secrets stay out of bundles**

```sh
curl -XPUT localhost:8290/nests/usdc/secrets -d '{"key":"rpc_url","value":"<canary>"}'
curl -s localhost:8290/nests/usdc/secrets     # expect: key names only, never values
```

Then `grep` your canary through the nest directory, any generated bundle, and the sealed segments.
Expect **no match anywhere**. *Proves* secret isolation. Also confirm rotating the secret does not
change the nest's bundle hash — if it does, every rotation invalidates segment reuse.

**5.9 A control-plane outage does not stop ingestion**

```sh
docker compose -f docker-compose.scaled.yml stop postgres   # or just the control plane
```

Expect workers to log failed reconcile ticks and **keep indexing** cursors whose leases have not
expired. *Proves* the deliberate independence: a control-plane outage must stop *rescheduling*, not
*ingestion*. Workers exiting here would turn a database blip into a fleet-wide outage.

**5.10 Under-scheduling is loud**

Declare a nest whose cursor exceeds every worker's budget. Expect `GET /plan` to report it
`unplaceable` with reason `toolargeforanyworker`, and the workers to warn every tick. *Proves* a fleet
that cannot run what it was asked never looks healthy. Note the distinction from `noroomrightnow`:
adding a worker fixes the latter and never the former.

---

## Reporting

Please report **passes as well as failures** — a level someone ran is worth more than a level we
assert, and the table at the top should get shorter over time.

Useful to include: the level and step, expected versus actual, `nuthatch --version`, chain and
contract, and whether embedded or scaled. For level 5, how many writers and FE nodes.

Known-unverified items are tracked in [`docs/prod-readiness.md`](prod-readiness.md) §11; anything you
confirm there can move from 🟡 to ✅ with your evidence attached.
