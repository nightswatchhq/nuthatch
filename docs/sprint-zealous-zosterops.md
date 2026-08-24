# Sprint: zealous-zosterops

Filed after yawning-yellowthroat (#823) landed. **Five issues.** A sprint is a labelled set. It
has no calendar.

## Definition of done

Every issue carrying the **`zealous-zosterops`** label is closed, and no open PR is for one of
them. That is five issues: #824, #825, #826, #827, #828. Work discovered in flight is filed
**unlabelled**. Pulling it into scope needs a board reply.

## The theme

**A runtime must forget what has gone away, and must not report one chain's truth as another's.**

#817 made the analytical connection persistent, which removes expensive rebuild work. Its cache
now has to obey mount lifecycle and source lifecycle, otherwise a deleted view or a departed nest
survives in process memory. #813 made metrics visible. They must be cheap enough to scrape and
must describe the current nest and chain, rather than a historical mount or whichever cursor last
wrote a global gauge.

Freeze-legal throughout: correctness and observability of capabilities already shipped. This does
not add an extraction mode, an RFC-0041 circuit, or a new analytics surface.

## The five

### 1. #824 - bound and evict cached DuckDB connections across runtime mounts

The global cache retains a DuckDB engine for every directory ever queried. Mounting and unmounting
nests must not turn ordinary operational churn into permanently retained connections and files.

### 2. #825 - invalidate cached DuckDB views when nest inputs disappear

A connection is only valid for its current views, labels, children and sealed inputs. Removing one
must remove it from the query surface before restart, not leave a convincing stale answer behind.

### 3. #826 - stop walking every nest directory during each `/metrics` scrape

Prometheus polling must not recursively stat every segment file in every nest. Storage gauges need
an incremental or bounded-refresh implementation with documented freshness semantics.

### 4. #827 - remove departed nests from the process-wide metrics registry

Unmount must remove the corresponding metrics state. A subsequent mount with the same name must
start cleanly, and scrapes must not retain stale series or keep scanning a dead path.

### 5. #828 - do not publish arbitrary cross-chain values as global tip and lag metrics

Per-nest cursors may not overwrite a process-global chain height and lag gauge. The exported model
must either label values by chain/nest or publish an explicit, meaningful aggregate.

## Explicitly not in this sprint

- **#829 and #830.** Mutable CI action references and release provenance form a separate
  release-integrity pair. They need a decision on signing and attestation, not a runtime patch.
- **RFC-0041 and all frozen work.** No general authored incremental entities.
- **#818, #270, #638, #750.** Existing p1 work has a different design or external dependency.

## How this sprint runs

Standing from nocturnal-nightjar, unchanged:

1. **Scope is the board's.** Discovered work is filed unlabelled.
2. **`Reviewed-by:` names the party who read the diff.**
3. **Acceptance is above.**

Also standing: never `git add -A`; one merge per CI cycle. `Closes` is one keyword per issue, not a
comma list - squash only honours the first.
