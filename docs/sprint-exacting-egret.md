# Sprint: exacting-egret

## Definition of done

Every issue labelled `exacting-egret` closed, and no open PR for one of them.

## The theme

**Find out what DuckDB actually costs us, before anyone argues about replacing it.**

RFC-0042 is the second and last carve-out from the 2026 freeze, and it was explicitly **sequenced
behind RFC-0041** because §9 hands DuckDB four roles inside the entity work and moving the engine while
those were still being assigned would make both unattributable. RFC-0041 shipped on 2026-08-28. That
condition is now met, and this sprint takes the carve-out in sequence rather than by drift.

**This sprint does not replace anything.** Slices 0 and 1 are measurement and boundary work; the
product is byte-identical at the end of both. Slices 2 to 4 - the DataFusion spike, the Turso spike,
the composed path - are deliberately **out of scope**, because committing to them now would be deciding
the answer before measuring, which is what §0 of the RFC forbids in as many words:

> There is no preferred answer. If evidence says DuckDB remains best, it stays.

A legitimate outcome of this sprint is "DuckDB stays, and here is what it costs and why it earns it".

## The pieces

### 1. #849 - RFC-0042 has no tracking issue and no freeze position

`question rfc p2`, currently `blocked`. Unblocked by RFC-0041 shipping. Closing it is recording the
position: carve-out taken, slices 0 and 1 only, slices 2+ need their own decision.

### 2. #891 - feed RFC-0043's tables into slice zero

`rfc`, currently `frozen` by design. RFC-0043 §4's difference table and §5's role table are slice
zero's input, so the inventory does not rediscover which DuckDB roles a DataFusion port would and would
not address. §5's answer is **one of four roles, partially**, and that is the honest size of what a
competitor's architecture tells us.

### 3. #889 - segment layout, and it comes FIRST

`performance p2`. Sequenced ahead of slice zero rather than beside it, because it is a **confound**:
small Parquet files degrade planning (RFC-0043 §7.1), so a DuckDB-versus-candidate measurement over
many-small-segments is partly measuring file layout and would name the wrong cause. Either fix it or
make file count, size distribution and row-group size a recorded covariate of every measurement.

### 4. RFC-0042 slice 0 - native BOM and role inventory

The bill of materials from build logs and link maps, and the complete list of DuckDB roles. Per the
2026-08-29 amendment: start from the **call sites** (`analytics.rs`, `entities.rs`, `entity_lower.rs`,
`graft.rs`, `seal.rs`, `authored_entity_spike.rs`), not from §9's four, because two roles are
product-visible and appear nowhere in §9 - `graft.rs` writes the engine string into grafting identity,
and `entities.rs` derives the admissible function vocabulary from `duckdb_functions()`, which means
**the SQL a nest may declare is DuckDB's function list**.

Baseline at or after #896, and say so. Before that fix `SELECT 1` cost 2,465 ms on a 38,428-segment
nest because of our own view management, and a baseline taken earlier charges the engine 2.4 seconds
it never spent.

Ends with: what ships, why, what it costs, and the benchmark noise floor.

### 5. RFC-0042 slice 1 - engine boundary and parity corpus, DuckDB unchanged

An internal analytical boundary that can execute, register hot/cold tables and views, explain and
cancel, with no DuckDB-specific types escaping it. Plus the parity corpus.

Per the amendment, the corpus **must cross every engine's internal batch boundary** - DuckDB's vector
is 2,048, DataFusion's default batch 8,192, dbsp's step 10,000 - because #894 is what happens when it
does not: all 857 tests sat under dbsp's step size and the whole suite was blind to a relation silently
keeping `groups mod 10,000`.

Ends with: byte-identical results on tests and a real workload, no change beyond noise.

### 6. #918 - `nuthatch_sealed_through` reads 0 after every restart

`bug`. Not an alpha regression - a 2.7.1 unit restarted at the same moment behaves identically - but it
matters more now that 3.0.0 ships six `nuthatch_entity_*` series inviting people to alert on that
surface. An alert that cries wolf after every restart gets muted, and a muted alert is how the real one
is missed.

### 7. #890 - the canonical-chain provenance limit as a stated non-claim

`documentation verification`. From RFC-0043 §7.3: hash-linking proves a set of headers forms an
internally valid chain, not that it is the chain consensus agreed on. It applies to us exactly as it
applies to them, and it belongs in `verification.md`'s claims rather than being quietly assumed away.

## Explicitly not in this sprint

- **RFC-0042 slices 2, 3 and 4.** The DataFusion spike, the Turso spike, the composed Rust path. They
  follow from slice 0's evidence. §10's own words: "Slice 4 follows evidence, not an open-ended plan to
  write a database."
- **Anything `parked` or `frozen`** beyond #891, which is slice zero's input. The freeze holds; this is
  a recorded carve-out, not a reopening.
- **#296** (compact binary row format). Real, and adjacent enough to slice zero's findings that doing
  it first would pre-empt them.
- **#750** (`p1`, `board-only`), #698, #814, #638, #815, #790. Real, none urgent, and adding them
  dilutes a sprint whose whole value is a clean measurement.
- **The 3.0.0 soak.** It runs on wall-clock, not sprint scope. Findings are filed unlabelled.

## How this sprint runs

Standing from nocturnal-nightjar, unchanged:

1. **Scope is the board's.** Discovered work is filed unlabelled.
2. **`Reviewed-by:` names the party who read the diff.**
3. **Acceptance is above.**

Also standing: never `git add -A`; `Closes` is one keyword per issue; and for this sprint especially -
**a ratio without a cause is not an architectural conclusion** (§5.1). Every measured gap gets profiled
to a cause before it appears in a table.
