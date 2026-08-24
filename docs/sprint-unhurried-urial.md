# Sprint: unhurried-urial

Filed while steady-starling (#808) and tenacious-thrush (#809) were still landing.
**Four issues.** A sprint is a labelled set. It has no calendar.

## Definition of done

Every issue carrying the **`unhurried-urial`** label is closed, and no open PR is for one of
them. That is four issues: #774, #671, #766, #676. Work discovered in flight is filed
**unlabelled**. Pulling it into scope needs a board reply.

## The theme

**A nest whose tables are numbered is the 289 ev/s failure mode with an alias.**

The AI surface this project exists for inherits whatever `add` and `init` wrote. `c5__horizon_stake_deposited` is a schema that describes nothing. A Vat nest that reports 0 tables with a checkmark is the same class: it looks finished.

Freeze-legal throughout: bug, documentation. Not RFC-0040. Anonymous-event decode is frozen; we warn, we do not invent a second keying scheme.

## The four

### 1. #774 - `add` (and `init`) default to `c<N>` when the ABI has a name

`--alias` already exists. The default when it is omitted is still `c0`, `c1`, `c2`. The contract
name is in the Sourcify metadata and often in a wrapped ABI's `contractName`. `l2_gns` is a
better default than `c1` in every case. Fall back to `cN` only when the name is missing or
cannot be an alias.

**Acceptance**

1. With no `--alias`, a resolved ABI whose name slugifies to a valid alias uses that alias.
   A collision with an existing alias is uniqued, not overwritten.
2. An ABI with no usable name still gets `cN`. `--alias` still wins.
3. A test feeds `DelegationManager` / `Vat` / a nameless ABI and asserts the three outcomes.
   Deleting the slugify path fails it.

### 2. #671 - a rename is declared, not inferred

`nuthatch schema` cannot re-key `semantic.toml`. `merge` must not drop tables: it cannot tell
a rename from a removal. So a rename has to be a command.

**Acceptance**

1. `nuthatch nest rename-alias <old> <new>` updates `nuthatch.toml` (`alias` and `abi` path),
   moves `abis/<old>.json`, and re-keys `[table.<old>__*]` in `semantic.toml`, preserving
   authored prose.
2. `merge` still does not infer a rename. The existing warning stays for a hand-edit that
   skipped the command.
3. A fixture with authored descriptions under `c0__transfer`, renamed to `gns`, still has
   those descriptions under `gns__transfer`. Deleting the re-key step fails it.

### 3. #766 - an anonymous-only ABI must not look like a successful nest

We do not decode anonymous events. That stays. `init` of MakerDAO Vat currently succeeds,
reports 0 tables, and prints the same checkmark as USDC.

**Acceptance**

1. After scaffolding, if the registry has zero tables and skipped at least one anonymous
   event, the output warns that this ABI cannot be indexed (anonymous events have no topic0)
   and does not present that as an ordinary success.
2. A fixture ABI that is only `LogNote` anonymous fails that assertion if the warning is
   deleted. A Transfer ABI still prints the ordinary success line.

### 4. #676 - RFC-0015's slice list is not the bar

The status line already says Implemented. The acceptance bar does not: a stranger, address to
query, under two minutes. That was measured (#672) and failed on mainnet. A status of
Implemented that does not name the unmet bar will hide the next regression the same way.

**Acceptance**

1. RFC-0015's status line does not say slices are in progress.
2. The Acceptance section records that the two-minute bar was measured (#672) and is unmet.
3. A test fails if the status line contains `in progress`.

## Explicitly not in this sprint

- **RFC-0040** and anything `frozen`. Anonymous-event decode is frozen.
- **starling and thrush.** They close on their own PRs.
- **#750**, the VPS. Ops.
- **#649 / #638 / #305**, Lodestar.
- **#286**, the 2 GB budget.

## How this sprint runs

Standing from nocturnal-nightjar, unchanged:

1. **Scope is the board's.** Discovered work is filed unlabelled.
2. **`Reviewed-by:` names the party who read the diff.**
3. **Acceptance is above.**

Also standing: never `git add -A`; do not `@`-mention Rowan in GitHub markdown; one merge per
CI cycle.
