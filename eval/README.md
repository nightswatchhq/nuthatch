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

The **0.4 Tier B baseline** is [0/15 first-try and 0/15 overall](eval-report.json), median of three
runs on `claude-sonnet-5` at the provider-default temperature 1.0. Mean SQL attempts were 1.267.
The exact commit, question-set hash, per-question outcomes, and class breakdown are in the report.

The runner invokes each subject as a separate restricted process with only the nuthatch MCP bridge;
the runner alone reads the SQL and expected answers. This honesty is the point: the eval is only
worth anything if its numbers are real.
