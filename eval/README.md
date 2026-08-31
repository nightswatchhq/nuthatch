# The agent-grade MCP eval (RFC-0016 §1)

nuthatch's bet is that **SQL is the IR**: natural language is the query surface, the agent is the
compiler, and nuthatch's job is to be the best possible *compilation target*. That makes the MCP
server a context-engineering problem, and context-engineering - like everything since RFC-0004 - is
gated by measurement, not anecdote. This directory is the measurement.

Two tiers, because determinism and LLMs don't mix and we refuse to pretend otherwise.

## Tier A - deterministic, CI-gated (no LLM)

`tests/eval_harness.rs` builds the fixture nest on the tape infra (the same scripted-chain double the
e2e tests use), seals a deterministic range, and runs every question in [`questions.toml`](questions.toml)
through the *same* hot∪cold SQL surface an agent's `sql` tool hits - asserting each known-correct
query returns its declared `expect` (order-normalised, numeric-tolerant).

This **proves the oracle**: the SQL and expected answers are correct against the fixture before any
agent is ever scored against them. It runs on every commit, no network, no key. If it's green, the
question set is a valid scoreboard; if the surface regresses, this goes red before an agent eval ever
runs. Run it with:

```sh
cargo test --test eval_harness
```

### The fixture

A `usdc__transfer` table over 10 blocks - `a1→a2` (blocks 1-5) then `a2→a3` (blocks 6-10), value
`100·b` - with blocks 1-7 sealed and 8-10 hot. Small, hand-computable, and deterministic, so every
answer in `questions.toml` is checkable by eye. The 15 questions span the classes an agent trips on:
aggregation, big-int arithmetic (the `value` / `value_dec` footgun), reserved-word columns
(`"from"`/`"to"`), coverage/range, filters, and group-by.

## Tier B - the agent eval (BYO key / local Ollama)

The part that needs a real model, and therefore lives **out of the default CI path**. A pinned agent
(model + temperature recorded) is given *only* the MCP tools and each question's natural-language
`question` string. Scoring is mechanical: the agent's final query result must equal the same `expect`
this repo already proved correct in Tier A - we compare **data, never prose**. Reported per question:
pass/fail, number of SQL attempts, tool calls used. Headline: **first-try pass rate** and **overall
pass rate**, median of 3 runs.

Every published number traces to an `eval-report.json` conforming to
[`eval-report.schema.json`](eval-report.schema.json) - date, model, commit, question-set hash. No
hand-typed scores, including flattering ones (the house rule since RFC-0004).

### Baseline status

The **0.4 Tier B baseline** is [0/15 first-try and 0/15 overall](eval-report.json) - **verified, not
merely reported**: all 15 known-correct oracles score as *passes* through the runner's own
`sql_rows` and `results_equal`, against the same `question_set_hash` the report carries, so the zero
belongs to the agent and not the harness. Median of three
runs on `claude-sonnet-5` at the provider-default temperature 1.0. Mean SQL attempts were 1.267.
The exact commit, question-set hash, per-question outcomes, and class breakdown are in the report.

The runner invokes each subject as a separate restricted process with only the nuthatch MCP bridge;
the runner alone reads the SQL and expected answers. This honesty is the point: the eval is only
worth anything if its numbers are real.

**A rejected query is a verdict; an unreachable scorer is not (#1051).** The two look alike and are
opposites. An invented table name comes back as a well-formed `{"error": ...}` from a healthy nest -
the scorer looked, and what it saw was bad SQL - so it scores a *diagnosed failure* and records why.
An unreachable nest is the scorer failing to look at all, and that is fatal. Collapsing them in
either direction loses the same information, and this file has done both.

**A scoring failure is fatal to the run, not a zero (#1051).** An unreachable nest, a timeout, an
HTTP error or a response of an unexpected shape used to leave the row set empty and score the
question *failed* - so a scorer that could not reach the nest published a schema-valid 0/15 with
nothing to say why. A scorer that cannot obtain a verdict has not discovered the agent is wrong; it
has discovered it cannot tell, and those must not share a result. `python3 eval/run-tier-b.py
--self-test` proves it without a key, a nest or a model, and `cargo test --test
eval_runner_self_test` runs that inside CI's required check.

**Every failing result records its query (#1051).** `final_query`, plus `final_rows` on a failure.
Without them a zero cannot say whether the agent invented a table name, tripped the
`value`/`value_dec` big-int footgun this fixture exists to probe, or fell over the `"from"`/`"to"`
reserved words - and RFC-0016 §1's whole premise is that the MCP surface is a context-engineering
problem *to be fixed*. The fields are optional in the schema and **mandatory in the runner**: the
0.4 baseline below predates them and genuinely lacks them, and backfilling `null` would assert the
subject issued no SQL, which is a different claim from "nobody recorded it".

## The authoring eval (RFC-0017, #1050)

The two evals answer different questions and neither substitutes for the other. RFC-0016 §1 above
measures **runtime** knowledge - an agent with the MCP tools querying a nest that already exists.
This measures **authoring** knowledge: the builder skill plus a shell, a contract address, and
nothing else. An agent with only MCP cannot scaffold a nest; an agent with only the skill cannot say
what a table means as of block N.

RFC-0017 fixes the three criteria, and `eval/authoring.toml` does not get to reinvent them: `init`
succeeds, `dev` reaches the pinned tip, one canned question answers correctly - *"scored mechanically
(exit codes + result comparison)"*. **Mechanically** is load-bearing: nothing scores prose, effort,
or how well the agent explained itself. Three facts about the filesystem and one result set.

Fully offline and deterministic. `scripts/fixture_rpc.py` serves the chain over loopback and the ABI
is handed over as a local file, so there is no Sourcify, no Etherscan and no network - a score that
moved because a third party was slow would be a number about the internet.

### The board is proven before anyone plays on it

`tests/authoring_eval_board.rs` walks the scenario with a **scripted reference solution** and must
satisfy every criterion, in CI, with no key. If it is red, an agent scoring 0/3 tells you nothing
about the agent. It also pins the criteria to nuthatch's own surface in the direction that actually
rots: a criterion is a claim that `init` writes a `schema.json`, that `sealed_through` appears in
`/sql` provenance, that `value_dec` exists - and any of those could change without an eval file
being touched, after which the next keyed run scores zero and it reads as a model that got worse.

Two things the board caught while being built, both by mutating it rather than by reading it:

- **The `reaches-pinned-tip` criterion was decorative.** The scenario declared `value = 8` and the
  test read the chain's `finalized` pin instead, so editing the criterion changed nothing. A
  criterion the scorer does not consult is not a criterion.
- **The runner's numeric-tolerance check proved nothing.** `"3600"` against `3600` passes even with
  numeric comparison removed entirely, because `json.dumps(3600)` is `"3600"`. It takes a float to
  discriminate.

### Isolation: enforced by construction, or labelled as asserted

The subject must not be able to read this repository. `eval/authoring.toml` carries the expected
result, and `tests/authoring_eval_board.rs` carries the same fixture values, the expected total and
the canned query verbatim - an agent that finds either scores 3/3 without building anything, and the
failure is silent and flattering.

Exactly one of two modes is required; there is no default.

**`--docker-image IMG` - enforced.** The runner builds the whole `docker run` itself, mounting only
the workdir (at its own path, so the ABI and nest paths stay valid) with `--network host` for the
fixture chain. The repository is **not mounted**, so it is unreachable *by construction*.

**`--sandbox TEMPLATE` - asserted.** Your own confinement, with `{workdir}` and `{rpc_port}`
substituted. Sanity-checked for usability, and **not enforcement**.

The distinction is a limit rather than laziness: **probing cannot prove the absence of a
capability.** An earlier design tested an operator template against a fixed set of commands and
paths, and four rounds of review kept finding the same defeat - a wrapper that rejects exactly those
probes and permits everything else passes every one, after which the subject reads the repository by
relative path, by building the path at runtime, or through an API the probes never considered. Each
new probe is one more special case for a wrapper to know about. So the runner stopped claiming what
it cannot check, and every report records which mode produced it:

```json
"isolation": "enforced-by-runner" | "operator-asserted"
```

Only `enforced-by-runner` may be published as an isolated score.

### What the probes still do

Both modes are checked for **usability**, because a confinement the subject cannot work in yields a
false zero - a number that looks like an agent failing and is actually a broken environment. The
sandbox must read a file in the workdir at its host path, and must reach the fixture RPC on
loopback. Reachability is proved, never assumed: curl, wget and python3 are each tried and *none of
them existing is a refusal rather than a pass*.

### Scoring does not depend on what the agent names things

`init` derives the table name from the contract alias, which defaults to `c0` but is the agent's to
choose. The runner resolves `{table}` from `/tables` rather than hardcoding `c0__transfer`, which
would fail a perfectly good nest for picking a nicer name - measuring obedience rather than
authoring.

### Running it

```sh
python3 eval/run-authoring.py --self-test                       # no key, no model, no network
python3 eval/run-authoring.py --nuthatch target/release/nuthatch --runs 3 \
  --sandbox docker run --rm -v "$WORKDIR:/w" -w /w <image>
```

**No baseline is published yet.** The board, the runner and both self-tests are in place and CI-gated;
the score lands the first time the keyed runner is executed, and is board-only because a keyed run is
credentials. The same refusal as RFC-0016's: a number here is real or it is absent.
