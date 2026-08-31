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

### The subject must be sandboxed, and the runner refuses without one

`--sandbox` is **required**. It takes a command prefix that confines the subject to its working
directory - a container runner, `sandbox-exec -f`, `bwrap --ro-bind` - and is recorded in the report,
because a score is only as trustworthy as the isolation behind it.

The first version of this runner set the subject's `cwd` to a temporary directory, described that as
isolation, and enforced nothing: the agent could read `eval/authoring.toml`, lift the expected
result, and score 3/3 by **discovering this repository** rather than by knowing how to build a nest.
Changing a working directory is not isolation, and the failure is silent and flattering, which is the
worst combination available. So the refusal is the default and there is no override.

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
