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
