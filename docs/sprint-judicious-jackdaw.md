# Sprint: judicious-jackdaw

Filed after industrious-ibis closed all six of its issues and v2.6.1 shipped. **Four issues.**

## Definition of done

Every issue carrying the **`judicious-jackdaw`** label is closed, and no open PR is for one of them.
Work discovered during the sprint is filed as an issue for the board rather than picked up, and
pulling anything into scope needs board approval.

## The theme

**The front door.**

The board set the direction on 2026-08-20 and it is written down in `docs/roadmap-2027.md`: no new
capability for the rest of the year. Bug fixes, security, performance, maintenance, marketing, and
one goal above the others - **make the delightful core best in class**.

This sprint is the first instalment, and its four issues are all the same thing seen from four
angles: *what a stranger meets first*. The CLI's help output. `init`'s first ten lines. A chain we
advertise that cannot actually backfill. Documentation that describes a product two releases old.

The reason to start here rather than with the deeper bug is what the week established. RFC-0015's
acceptance bar - *a stranger goes from an address to querying, delighted, in under two minutes* -
was written in July, has six shipped slices behind it, and had **never been measured**. The first
time anyone ran it was 2026-08-20, and it failed. Six releases went out over a front door nobody
had walked through.

So the discipline for this sprint, on every item: **run it the way a stranger would, before and
after.** Not the test, the thing.

## The four

### 1. #674 - 24 subcommands, and `worker` outranks `sql`

RFC-0015's non-goals are explicit that the enterprise breadth must be "discoverable in the docs, not
in the first ten seconds". For a CLI, `--help` *is* the first ten seconds, and ours currently answers
"what is this tool?" with a list in which `worker` and `control` - a writer pool and a control-plane
API - rank **fifth and sixth, above `sql`**. The core path is `init` → `dev` → `sql`.

Bounded and reversible: clap help headings, plus ordering by the happy path. Three options are costed
on the issue; nothing is removed and no capability changes. Probably the highest ratio of
first-impression to effort on the whole board.

### 2. #675 - a raw log line through the middle of `init`

`init` is measured at five seconds, no flags, chain probed and ABI resolved, and it is genuinely the
best-feeling thing in the product. One `tracing::info!` crashes through the middle of its `→`/`✓`
output carrying an ISO timestamp and ANSI codes, saying something the lines either side already say.

Small. Worth doing precisely because the surface around it is good: this is the only blemish on the
part a stranger sees first. Sweep the other `init` paths while in there - the Etherscan fallback, a
cached ABI, `--abi` supplied, and `add` - since only the Sourcify path is confirmed.

### 3. #679 - Polygon cannot backfill, and shipped two days ago

The only item here that is a live defect rather than polish. Polygon shipped in v2.6.0 on the 19th.
On the 20th, `nuthatch doctor` against the endpoint we list **first** reports no archive depth and a
failing `getLogs` probe, three times over several minutes. Its `log_window` of 2,000 is twenty-five
times wider than its only working endpoint serves.

The instructive part is not the endpoint. It is that `chains.rs` records *"Measured 2026-08-19:
polygon-bor-rpc.publicnode.com gives a 5,120-block window"* - true when written, false one day later.
**A recorded measurement is a snapshot presented as a property**, and this is the third time that
class has bitten: two mainnet endpoints were removed in July for the same reason.

So the issue asks for two things and the second matters more: fix Polygon, and propose how the
endpoint bar becomes a **recurring** check rather than a one-time gate. #633 is adjacent and should
be read alongside.

### 4. #681 - the remaining stale surfaces

The board took the high-traffic half on 2026-08-20 (PR #682): the README, two book chapters, the
roadmap page and `CLAUDE.md`'s build order, all of which described the pre-parity product. The
website's `build/contract-calls.md` was written because **2.6.0's two headline features had shipped
with no documentation page at all**.

What is left is the rest of the website doc sections and a sweep for smaller stale claims. Measured
counts are on the issue.

The part worth thinking about rather than editing: which of these can be checked **mechanically**.
`tests/skill_refs.rs` gates the skill reference against the CLI, and that is precisely why the CLI
reference did not drift while everything around it did. A proposal is worth more than a clean sweep,
because the sweep will be needed again in two releases.

## Explicitly not in this sprint

- **#672** - the flagship first run. The biggest item on the board and deliberately excluded. The
  board has made five attempts at it and been wrong four times; the reproduction in #677 is merged
  and `#[ignore]`d, and the remaining piece is that the pipeline sizes several windows concurrently
  so a cap discovered mid-flight arrives too late. It wants somebody who has watched the trace. Do
  not pick it up; if you have an idea, put it on the issue.
- **#649** - the Lodestar parity gaps. Board work, in flight.
- **The parked capability issues** - revm, traces, ExEx, DataFusion, Turso, tier-4 cache, wildcard
  decode, OBIB, whole-derivation reuse. Parked for 2026 by the freeze, not cancelled. They still want
  a `parked` label; that is on the board.

## Why four, and the standing trap

Four rather than six because two of these have an open question inside them rather than a known edit:
#679's recurring-probe proposal and #681's mechanical-check proposal. Those are the valuable halves
and they are not typing.

The trap to name, because it cost the board a day this week: **do not conclude anything from a single
run against a public endpoint.** Four identical 90-second runs of the same demo indexed 2, 15, 28 and
198 events. A number quoted from one sample is not a measurement, and this sprint is full of
temptations to take one.

## Outstanding on the board's side

Recorded here rather than in an issue, because they are the board's and the firm should not wait.

- **Industrious-ibis's audit has not happened.** The cycle says audit, then file. This sprint was
  filed first, deliberately: the board will audit ibis immediately after, and the two sets share no
  files - ibis touched `doctor.rs`, `semantic.rs`, `docs/rfcs/README.md` and one e2e test; this one
  touches `cli.rs`, `init`, `chains.rs` and the website.
- **Hardy-heron's audit is still owed** from the sprint before.
- **The `parked` label** on the frozen capability issues.
- **A note of thanks.** #659 - what `curatorCount` actually counts - was the sharpest issue on the
  ibis board, and the board was working the same question in parallel without realising it. That is
  the board's failure to check for duplicate work, not the firm's, and the firm's answer landed.
