# RFC-0017: The builder skill - teaching coding agents to drive nuthatch

- Status: **Implemented** (generated CLI ref + drift-checked authored skill; authoring eval pending a keyed run)
- Author: Pete (cargopete)
- Date: 2026-07-19
- Depends on: RFC-0016 (Draft - the semantic layer this skill co-reads, and the eval harness it
  extends), RFC-0015 (Draft - this is an adoption artifact in the delightful-core sense)
- Blocks: the agent-first onboarding path. A model asked to "set up nuthatch for this contract"
  today hallucinates flags - nuthatch is days old and in nobody's training data.
- Nature: short engineering RFC. Deliberately small: this is a week of disciplined work, not a
  design problem; it gets an RFC so the drift rules and the eval are on record, not relitigated.

## Abstract

Ship a standalone **builder skill** - a `.claude/skills/`-style package in the nuthatch repo,
installable into Claude Code (or any skill-reading agent) *before* the user has a nest - that
teaches an agent to drive nuthatch itself: init, config, factories, compliance, roost, pack/mount,
troubleshooting. It complements, not duplicates, the MCP work: **RFC-0016 is runtime knowledge**
(what a running nest's data means and how to query it); **this is authoring knowledge** (how to
build and operate a nest at all). An agent with only MCP cannot scaffold a nest; an agent with only
the skill cannot say what a table means as of block N. Both, one source of truth each.

Two rules carry the RFC: **generate what can be generated** (CLI and config references from the
code, not by hand), and **CI-check the rest for drift** - a skill that lies about flag names is
worse than no skill, for the same reason stale semantics are worse than none (RFC-0016 §2).

## The two skills (distinction on record)

| | Per-nest skill | Builder skill (this RFC) |
|---|---|---|
| Ships via | `nuthatch init` scaffold, travels with the project | nuthatch repo, installed by the user once |
| Teaches | *this* nest: its tables, semantics, query surface | nuthatch itself: CLI, config, workflows, ops |
| Source of truth | the semantic layer (RFC-0016 renders it) | clap + serde + docs/, generated + drift-checked |
| Exists when | after `init` | before `init` - it's how the agent *runs* `init` |

## Design

**Layout.** Thin `SKILL.md` (the trigger description does the heavy lifting: building, configuring,
running, or debugging a nuthatch nest/roost), progressive-disclosure references behind it:

```
skills/nuthatch-builder/
  SKILL.md                # triggers, the 90-second happy path, when to read what
  cli-reference.md        # GENERATED from clap (--help walk of every subcommand)
  config-reference.md     # GENERATED from the serde structs (nuthatch.toml, semantic.toml, roost.toml)
  workflows.md            # authored: init→dev→sql, add-a-contract, factories, publish (pack/mount), roost
  compliance.md           # authored: labels/lists/screen/flags/audit, when each applies
  troubleshooting.md      # authored: tip lag, RPC failover, reorg symptoms, guard rejections, RAM budget
```

**Generated, not authored, wherever possible.** `nuthatch skill-refs` (hidden dev subcommand or
`scripts/`) emits `cli-reference.md` from clap's own metadata and `config-reference.md` from the
config structs. Authored files may only *reference* flags and keys that appear in the generated
files - a CI grep-check fails the build on any mention of a nonexistent flag/key (the `check`
discipline applied to prose).

**Content stance.** Workflows are copy-paste-runnable and honest about the non-negotiables an agent
must not fight (single writer, one cursor per chain, sealed segments immutable, guards are node
self-protection). Troubleshooting maps symptom → metric → remedy using real `/metrics` series
names. Nothing in the skill duplicates the per-nest skill's job: for "what does this table mean,"
it says *call the MCP `schema` tool*.

**The authoring eval (closing the loop with RFC-0016 S1).** Extend the Tier B harness with one
scenario class: an agent with only the builder skill + a shell takes a contract address and must
produce a working nest against the fixture tape - `init` succeeds, `dev` reaches the pinned tip,
one canned question answers correctly via `nuthatch sql`. Scored mechanically (exit codes + result
comparison), reported in the same `eval-report.json`. "An agent can build a nest end-to-end,
measured" joins the README claims, sourced like every other number.

## Implementation (slices)

1. **S1 - generators + CI drift check.** `cli-reference.md` + `config-reference.md` emitted from
   code; the prose-mentions-real-flags check wired into CI.
2. **S2 - the authored files.** SKILL.md + workflows + compliance + troubleshooting, written against
   the generated references.
3. **S3 - the authoring eval.** The end-to-end scenario in the RFC-0016 harness; baseline published.
4. **S4 - distribution.** Install one-liner in the README (copy into `~/.claude/skills/` or repo
   `.claude/skills/`), mentioned by `init`'s output and the website's AI section.

## Acceptance

- CI fails on reference drift (proven by a deliberate bad-flag commit in review).
- The authoring eval passes on the fixture tape with a published score.
- The subjective 0015-style bar: a stranger in Claude Code, skill installed, says "index USDC on
  mainnet" - and is querying, delighted, without opening the docs.

## Open questions

1. Does the per-nest scaffolded skill *reference* the builder skill (one pointer, no content copy)?
2. Skill format portability: keep it Claude-skills-native, or also emit a generic `AGENTS.md`
   rendering from the same sources for non-Claude agents?
3. Where does `skill-refs` live - hidden subcommand (always in sync with the binary) or build
   script (keeps the CLI surface clean)? Leaning subcommand: the binary describing itself is the
   most drift-proof option we have.
