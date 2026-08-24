# Sprint: yawning-yellowthroat

Filed while watchful-wren (#816) and xenial-xenops (#817) are still open. Independent of both.
**One issue.** A sprint is a labelled set. It has no calendar. A p0 is allowed to be a set of one.

## Definition of done

Every issue carrying the **`yawning-yellowthroat`** label is closed, and no open PR is for one of
them. That is one issue: #819. Work discovered in flight is filed **unlabelled**.

## The theme

**The README describes a product we have not shipped.**

Declarative entities as incremental DBSP views is the thesis. The runtime has three built-in
circuits: balances, exposure, velocity. Nest-authored `views/*.sql` are names, evaluated in
DuckDB at query time. That distinction is load-bearing for anyone who writes a view expecting
it to be maintained. The copy did not make it.

Freeze-legal: documentation. Not RFC-0041 implementation.

## The one

### #819 - README promises general incremental views

**Acceptance** is the issue's. Restated so this file can close without the GitHub tab:

1. README distinguishes decoded facts, the three built-in IVM relations, and authored
   `views/*.sql` evaluated at query time.
2. `CLAUDE.md` describes authored incremental entities as RFC-0041, frozen for 2026, not
   shipped general behaviour.
3. Operator and builder-skill docs are searched for the same broad claim and corrected
   where they imply arbitrary authored views are already incremental.
4. Shipped balance / exposure / velocity claims stay intact and specific.
5. RFC-0041 is linked as the proposed route, explicitly frozen for 2026.
6. A test fails if the misleading broad wording returns in those surfaces.

## Explicitly not in this sprint

- **RFC-0041 implementation.** Frozen. Slices 0-2 are #818, #820, #821.
- **watchful-wren / xenial-xenops.** Independent. Do not restack.
- **#296, #814, #790.** Not this p0.

## How this sprint runs

Standing from nocturnal-nightjar, unchanged:

1. **Scope is the board's.** Discovered work is filed unlabelled.
2. **`Reviewed-by:` names the party who read the diff.**
3. **Acceptance is above.**

Also standing: never `git add -A`; one merge per CI cycle. `Closes` is one keyword per issue.
