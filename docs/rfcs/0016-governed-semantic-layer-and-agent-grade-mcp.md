# RFC-0016: The governed semantic layer and the agent-grade MCP experience

- Status: **Implemented** (S1 eval harness → S2 semantic layer → S3 errors-as-prompts + explain → S4 result shaping → S5 resources/prompts; Tier-B baseline pending a keyed run)
- Author: Pete (cargopete)
- Date: 2026-07-19
- Depends on: RFC-0001 (Implemented - the decode registry and schema manifest this layer describes),
  RFC-0012 (Implemented - nest blobs, which must carry semantics as authored inputs), RFC-0015
  (Draft - this RFC *is* item 5 of the delightful core, grown to full size)
- Blocks: the "AI-native" claim being literally true. Today `nuthatch mcp` works; this RFC is the
  difference between "an agent can query nuthatch" and "an agent queries nuthatch *correctly, first
  try, and can prove it*."
- Nature: engineering RFC. One deliberate scope rule up front: **nothing in this RFC touches the
  data path.** Every change is to what agents *read* (schemas, errors, results, docs) and how we
  *measure* them - the deterministic core is untouched, per non-negotiable 4.

## Abstract

The strategic call is already made and stays made: **SQL is the IR.** Natural language is becoming
the query surface, agents are the compiler, and nuthatch's job is not to parse English - it is to be
the best possible *compilation target*. That reframes the MCP server: it is not an API problem, it
is a **context-engineering problem**. An agent's SQL is exactly as good as what our tools teach it.

Five workstreams, sequenced by the RFC-0004 discipline (measure first, optimise second):

1. **The eval harness first** - a fixture nest plus a pinned set of natural-language questions with
   known-correct answers, scored by comparing *query results*, not judging prose. The score is a
   published artifact (bench-report house rule) and the regression gate for everything below.
2. **The governed semantic layer** - `semantic.toml`: authored, per-table/per-column meaning,
   generated at `init`, hand-editable, hash-pinned into the nest blob. The `schema` tool stops being
   a static string and becomes the composition of registry + semantics + live coverage + sample rows.
3. **Errors as prompts, plus `explain`** - every failed agent query is a teaching opportunity;
   enrich errors so the agent self-corrects in one round-trip, and let it validate before executing.
4. **Results shaped for context windows** - MCP-specific row caps, compact tabular output,
   truncation *guidance*, and a provenance stamp ("as of block N, from sealed segments") so an
   agent's answer is citable back to content-addressed data.
5. **The rest of the MCP spec** - resources and prompts, which we currently leave on the table.
   Notifications are named and explicitly deferred (they belong to a future standing-queries RFC).

## Motivation

### Where the MCP surface stands (0.4, honest)

`nuthatch mcp` is a thin, fully-offline stdio bridge to a running `dev`'s HTTP API - the right
architecture (never contends with the single writer, reflects live IVM views, nothing phones home).
It exposes eleven tools: `status`, `schema`, `tables`, `table`, `sql`, `entity`, `balance`,
`top_balances`, `flags`, `exposure`, `screen_status`. `init` scaffolds `llms.txt` and a
`.claude/skills/nuthatch/` skill. That was slice 5's brief, delivered.

But watch a real agent use it and the gaps are specific and repeatable:

- **`schema` is a static string.** The same hardcoded `schema_doc()` for every nest - it describes
  the *shape* of the data model, not *this nest's* data. It cannot say what `staking__stake_delegated`
  means, which blocks are covered, or what a row actually looks like.
- **The footguns are undocumented, and agents step on every one.** `usdc__transfer` has a column
  literally named `from` - a reserved word in every SQL dialect on earth. Every agent writes
  `SELECT from FROM` on its first attempt. `uint256` values are exact text with a `{col}_dec`
  companion; an agent that hasn't seen a sample row writes `SUM(value)` on the text column and gets
  a type error (or worse, a silent cast). The hot/cold split (`sql` sees sealed only; the tip is
  served by point-reads and IVM views) is stated once in prose and routinely missed.
- **Errors are relayed raw.** A DuckDB `Binder Error: Table "transfers" does not exist` costs the
  agent a full extra round-trip to rediscover what `tables` already told it. Multiply by every
  mistake class.
- **Result shaping is tuned for curl, not context windows.** The `/sql` guards (50,000 rows, 30 s,
  2 concurrent, 16 KiB query) are node self-protection - correct for HTTP, hostile to a 200K-token
  context. 50K rows of verbose JSON doesn't protect an agent; it lobotomises it.
- **We use a third of the protocol.** Capabilities advertise `tools` only. Resources (the natural
  home for `llms.txt`, the schema doc, per-table samples) and prompts (canned analysis flows) are
  unimplemented.
- **And we have no number.** Every claim above is anecdote. The project's rule since RFC-0004 is
  that anecdotes don't gate work - measurements do.

### Why this is the right work now

RFC-0015 named the wedge: time-to-first-query and the joy of querying. The MCP path is that same
wedge for the fastest-growing class of user - the developer whose first move is "point Claude at
it." For that user, *the agent's first-try success rate is the time-to-first-query.* And it
compounds: every improvement to the semantic layer also improves the human docs, the admin UI's
self-description, and the scaffolded skill, because they all read the same source of truth.

The competitive angle is real too. Hosted platforms will ship MCP servers; what they cannot ship is
a semantic layer that is **authored in the nest, versioned with the nest, content-addressed with the
nest, and verified like the nest** - because they don't have nests. Semantics-as-code, riding the
RFC-0012 blob machinery, is a differentiator that costs us a file format and them a product rethink.

## Goals

- An agent with *only* MCP access answers a pinned question set correctly, first-SQL-try in the
  common cases, and the rate is a published, reproducible number.
- A nest author can state what their tables *mean* in one obvious place, and every surface (MCP,
  admin UI, scaffolded skill, docs) reads it.
- Semantics travel with the deploy unit: `nest pack` pins them, `nest mount` verifies them.
- A failed agent query self-corrects in one round-trip for the known mistake classes.
- Results carry provenance an agent can cite: block height, sealed watermark, data source.

## Non-goals

- **NL-to-SQL in the binary.** No LLM in core, no LLM in the data path, no bundled model. The agent
  is the client's; we are the target. (A BYO-key/Ollama *authoring* assist for `semantic.toml`
  descriptions is a future nicety, not this RFC.)
- **Streaming/subscription tools.** MCP notifications are the natural transport for DBSP standing
  queries ("watch this predicate, get deltas, reorgs arrive as retractions") - that is a
  killer demo and **a separate future RFC**, because it needs the `watch` machinery itself. Here we
  only avoid painting over the door: the capabilities structure and tool naming leave room.
- **Auth/tenancy on the MCP surface.** Same dividing line as ever (RFC-0005 §6): the MCP server is
  local, single-user, bridging to localhost. Fronting it is the operator's layer.
- **Prompt-injection-proof output.** Sample rows and error echoes contain chain data, which is
  attacker-writable (token names, revert strings). We label untrusted data clearly (§3.4) but do not
  pretend to sanitise the chain.

## Design

### §1 - The eval harness: `nuthatch eval` (measure first)

The Matchstick move, applied to the AI surface. Two tiers, because determinism and LLMs don't mix
and we refuse to pretend otherwise:

**Tier A - deterministic, CI-gated (no LLM).** Golden tests over the *context surfaces themselves*:
the composed `schema` response for the fixture nest matches a committed fixture byte-for-byte; each
known error class maps to its enriched message; truncation produces the guidance text; the blob
round-trip preserves the semantics hash. This tier runs in CI on every commit, no network, no key -
exactly like the existing `check` framework (committed fixtures, hermetic by design).

**Tier B - the agent eval (BYO key or local Ollama, out of the default CI path).** A pinned
question set (`eval/questions.toml`: natural-language question + the SQL whose result is the
expected answer, recorded as a fixture) is run against a real agent that has *only* the MCP tools.
Scoring is mechanical: the agent's final query result must equal the expected result (order-
normalised, numeric-tolerant where declared) - we compare *data*, never judge prose. Reported per
question: pass/fail, number of SQL attempts, tool calls used. Headline: **first-try pass rate** and
**overall pass rate**, median of 3 runs, pinned model and temperature recorded in the report.

House rule applies: every published number traces to an `eval-report.json` (date, model, commit,
question-set hash). No hand-typed scores, including flattering ones. The baseline is run against
0.4's surface *before* any §2-§5 work lands, and each subsequent slice republishes the number.
That's the whole point of doing this section first.

The fixture nest reuses the existing tape/replay infrastructure (`tests/common/tape.rs`): a small,
committed, deterministic nest (USDC-class table + a factory template + one IVM view + one
compliance annotation) so every question class has a target and Tier B needs no live RPC.

### §2 - `semantic.toml`: semantics as authored nest input

One file, beside `nuthatch.toml`, human-first:

```toml
# semantic.toml - what this nest's data *means*. Read by the MCP `schema` tool,
# the admin UI, and the scaffolded skill. Edit freely; `nest pack` pins it.
schema_version = 1

[nest]
description = "Graph Protocol Horizon staking activity on Arbitrum One."

[table.staking__stake_delegated]
description = "A delegator adds stake to an indexer's delegation pool."
grain = "one row per StakeDelegated event"

[table.staking__stake_delegated.columns]
service_provider = "The indexer receiving the delegation (address)."
tokens = { description = "GRT delegated, base units (18 decimals).", unit = "GRT-wei", use = "tokens_dec for arithmetic" }

[table.usdc__transfer.footguns]
reserved_words = ["from", "to"]      # always double-quote in SQL
big_ints = ["value"]                 # text; use value_dec for SUM/AVG/comparisons
```

Rules of the layer:

- **Generated at `init`, never trusted blindly.** `init` seeds descriptions from the ABI - NatSpec
  (`devdoc`/`userdoc`) when the Sourcify metadata carries it, event/param names as honest fallback
  ("(from the ABI; edit semantic.toml to improve this)"). Footguns are *derived, not authored*:
  reserved-word columns and big-int columns are computed from the registry, so they are always
  present and always correct even if the author never opens the file.
- **Governed = versioned + content-addressed + verified.** `semantic.toml` is an authored input in
  the RFC-0012 sense: `nest pack` includes it in the manifest with its hash; `nest mount` verifies
  it; the `schema` tool response carries the hash. A drift guard mirrors the registry check: if the
  file references a table or column the registry doesn't have, `dev` warns loudly (stale semantics
  are worse than none) - and Tier A has a test for the warning.
- **One source of truth, many readers.** The MCP `schema` tool, the admin UI's table inspector, and
  the scaffolded `.claude/skills/nuthatch/` skill all render from the same struct. No second copy
  anywhere, ever.

### §3 - The enriched `schema` tool (and friends)

`schema` becomes the composition of four layers, assembled per call from a running nest:

1. **Structure** (registry): tables, columns, Solidity types, topic0s - what `tables` returns today.
2. **Meaning** (`semantic.toml`): descriptions, grain, units, the derived footgun annotations.
3. **Coverage** (live): per table - row count, min/max `block_number`, and the hot/cold seam stated
   *as data*: `sealed_through`, tip height, and the sentence "sql sees ≤ sealed_through; the tip is
   served by entity/balance/table". The seam stops being prose an agent skims and becomes numbers
   it can reason about.
4. **Evidence** (deterministic sample rows): up to 3 rows per table, selected as the *first* rows by
   `(block_number, log_index)` - deterministic, so Tier A can golden-test the whole response. Sample
   rows are disproportionately effective context: an agent that has *seen* `value` as text with
   `value_dec` beside it writes correct aggregation first try. Samples are wrapped in a clearly
   labelled untrusted-data block (chain data is attacker-writable; the label is the mitigation we
   can honestly offer).

**Size budget:** the composed response must fit a working context comfortably - target ≤ 8 KiB for
a five-table nest; large roost-mounted nests summarise per table with a `schema {table}` drill-down
argument. The budget is a Tier A assertion, not an aspiration (same discipline as the RAM budget).

Supporting tool changes:

- **`explain` (new):** validate + plan without executing (DuckDB `EXPLAIN` under the same guards,
  fast timeout). An agent checks a query's shape before spending a concurrency slot on it, and the
  enriched error path (§4) applies to `explain` too - so validation errors teach.
- **`sql` description** is regenerated per-nest from the same composition (real table names in the
  examples, not `usdc__transfer` hardcoded).
- **Tool descriptions audit:** every description states its finality domain (sealed vs tip) in the
  first sentence - the single most-missed fact in observed sessions.

### §4 - Errors as prompts

A small, closed error-classification layer between DuckDB/HTTP and the MCP response. Raw message
always preserved (never lie about what the engine said); enrichment appended. The classes, each with
a Tier A golden test:

| Class | Detection | Enrichment |
|---|---|---|
| Unknown table | binder error + name | Fuzzy match (edit distance over registry tables): "no table `transfers`; closest: `usdc__transfer`. Call `schema` for the full list." |
| Unknown column | binder error + name | Same fuzzy match over that table's columns; if the miss is a footgun name, say so. |
| Reserved word | parse error + column ∈ derived reserved set | "`from` is a reserved word and a column of `usdc__transfer` - double-quote it: `SELECT \"from\" …`" |
| Big-int arithmetic | type/conversion error on a text uint column | "`value` is exact text (uint256); use `value_dec` for SUM/AVG/comparisons." |
| Tip-blindness | valid query, 0 rows, requested range > `sealed_through` | Not an error - a result *note*: "range extends past sealed_through=N; recent rows are served by `table`/`entity`." |
| Guard rejections | 400/503 from the guards | Restate the specific guard and the remedy ("30 s timeout - add filters or LIMIT; try `explain` first"). |

The measure of success is mechanical and already instrumented by §1: **mean SQL attempts per
question drops** in the Tier B report. This is the cheapest section of the RFC and compounds on
every query forever.

### §5 - Results shaped for context windows

MCP responses diverge from HTTP responses deliberately (the bridge already mediates; this is where
it earns its keep):

- **Caps:** default 200 rows per `sql` result over MCP (arg-overridable up to the HTTP cap). The
  HTTP surface keeps 50K; curl and agents are different consumers.
- **Format:** compact aligned text / CSV-style for tabular results - measured target ≥ 3× fewer
  tokens than the current verbose JSON for the same rows (a Tier A fixture asserts the format).
- **Truncation is guidance, not silence:** "truncated at 200 of ~184,203 rows - aggregate
  (GROUP BY), tighten the WHERE, or raise `limit`." An agent told *why* adapts; an agent silently
  truncated reports wrong totals.
- **Provenance stamp on every result:** `as_of` block, `sealed_through`, source (`sealed` /
  `hot+sealed` / `view`), and the registry + semantics hashes. An agent can now *cite* its answer
  against content-addressed data - verifiability extended to the last hop, which is the founding
  thesis wearing an MCP hat.

### §6 - The rest of the protocol

- **Resources:** expose `llms.txt`, the composed schema document, `semantic.toml`, and per-table
  sample sets as MCP resources with stable URIs (`nuthatch://schema`, `nuthatch://table/{name}/samples`).
  Clients that preload resources get the context without burning a tool call.
- **Prompts:** three shipped, argument-taking prompts - `profile-contract` (activity overview),
  `investigate-address` (balances, exposure, flags, screen status for `{address}`),
  `verify-a-number` (re-derive a figure with provenance). Prompts are rendered from the semantic
  layer, so they name real tables.
- **Capabilities honesty:** advertise `tools`, `resources`, `prompts`; nothing else. When a future
  standing-queries RFC lands, `notifications` slots in without breaking a client.

## Implementation (slices; each ends runnable, RFC-0004 order)

1. **S1 - harness + baseline.** Fixture nest on the tape infra; `eval/questions.toml` (20 questions
   across tables/views/factories/compliance); Tier A golden framework; Tier B runner +
   `eval-report.json`; **publish the 0.4 baseline score.** Nothing else may merge first.
2. **S2 - `semantic.toml` + enriched `schema`.** File format, `init` generation (NatSpec →
   fallback), derived footguns, drift guard, composition (structure+meaning+coverage+samples), size
   budget, blob pinning + mount verification. Re-run Tier B; publish.
3. **S3 - errors as prompts + `explain`.** The classification table, fuzzy matching, per-class
   goldens. Re-run; publish (watch mean-attempts).
4. **S4 - result shaping + provenance.** MCP caps, compact format, truncation guidance, stamps.
5. **S5 - resources + prompts + descriptions audit.** Final re-run; the README's AI section gets
   the number.

Do not start slice N+1 while slice N has failing tests or an unmet budget. (Standing rule; restated
because S1 *is* the budget for this RFC.)

## Testing

- Tier A goldens: composed `schema` (fixture nest, byte-exact), every §4 error class, truncation
  text, compact-format fixture, size-budget assertion, semantics drift warning, blob round-trip
  (`pack` → `mount` → identical semantics hash).
- Property test: fuzzy-match suggestions always come from the registry (never hallucinate a name).
- Tier B: 20-question run, median of 3, pinned model; `eval-report.json` schema-validated in CI even
  when the run itself is skipped (no key present → skip loudly, never fake).

## Risks

- **Semantics drift** (ABI evolves, file doesn't): mitigated by derived-not-authored footguns, the
  drift guard, and mount-time verification. Stale *descriptions* remain possible - that is the
  author's file, and the generated-fallback text marks unedited entries honestly.
- **Eval flakiness / model dependence:** median-of-3, pinned model+temp in the report, and the
  score is always published *with* its model string - it is a number about a pairing, not a
  universal constant. Tier A is the CI gate precisely because Tier B can't be.
- **Context bloat:** the size budget is a test, and roost-scale nests get the drill-down form.
- **Prompt injection via chain data:** labelled untrusted blocks; no tool ever treats result
  content as instructions (the bridge has no such path today; keep it that way by construction).
- **Scope creep toward NL-to-SQL:** the Non-goals section is the fence; anything that puts a model
  behind a nuthatch tool call is a different RFC and probably a different project.

## Alternatives considered

- **A separate "semantic catalog" service** - violates single-binary; the nest *is* the catalog.
- **SQL comments / view DDL as the semantics store** - not diffable/authorable enough, invisible to
  `pack`, and DuckDB comment support doesn't survive our attach-per-query model.
- **LLM-judged evals** - non-deterministic scoring of non-deterministic output; comparing query
  *results* against fixtures is strictly more honest and infinitely cheaper.
- **GraphQL as the agent surface** - re-introduces the schema-authoring tax nuthatch exists to
  delete; agents demonstrably write better SQL than GraphQL against novel schemas, *when taught*.

## Open questions

1. NatSpec coverage in the wild: what fraction of Sourcify-verified contracts carry usable
   `devdoc` for events? (Determines how good generated descriptions are on day one - worth a quick
   measured survey during S2, reported like everything else.)
2. Question-set governance: does `eval/questions.toml` live in-repo only, or do nests ship their
   own question sets (`nest pack` including evals - "tested semantics" as a publishable property)?
3. `explain` guard budget: share the 2-slot semaphore or a dedicated cheap lane with a 2 s timeout?
4. Tier B local runner: is an Ollama-class open model good enough to make the eval runnable with
   zero keys (the sovereignty-consistent default), with the frontier-model score as the published
   headline?
5. Sample-row selection for factory-shared tables: first-3 globally, or first-per-child capped -
   which teaches the `address`-distinguishes-children pattern faster?
