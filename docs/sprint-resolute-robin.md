# Sprint: resolute-robin

**A port is real only when real queries survive it.**

The third sprint after the freeze. `inessential-ibis` proved three capabilities the product works
without; this one asks the opposite question about one capability it now claims to have. RFC-0044
has shipped a classifier, an emitter and a report across S1, S2 and S5, and none of that has yet met
a subgraph somebody else chose.

## Definition of done

Every issue carrying the **`resolute-robin`** label is closed, and no open PR is for one of them. At
filing: **#1212, #1264**. The label, not this list, is the record of scope. Whoever files a new issue
into this sprint labels it.

**This sprint does not start until `inessential-ibis` closes.** #1214, #1215 and #1219 are all open,
and #1214 has PR #1254 behind it. The rule `halcyon-hoopoe` set and `inessential-ibis` kept is that a
sprint is scoped from a clear board.

## The theme

Both items are the same instrument pointed at two things.

**#1212** needs RFC-0044 §10's second gate: diff a ported nest against a reference Graph endpoint,
field by field, and fail on a single divergence the report did not predict in advance.

**#1264** needs RFC-0053 S0: compare real queries against a reference Graph endpoint and Nuthatch,
including response shape and field-level values.

That is one harness. Build it once, in the open, and point it at the acceptance port and at the
compatibility surface in turn. Building it twice is the failure mode this sprint should be most
alert to, because two harnesses that disagree are worse than one that is wrong.

## The shared way of being wrong

**A validator that reaches agreement by narrowing the question.** #1264 already carries the clause
in its body: *a clean result must not be obtainable by dropping unsupported selections.* The same
trap sits under #1212 from the other side, where a diff that skips the fields the report called
unreachable is a diff that agrees with itself.

This repository has paid for that shape repeatedly. Four tests and three RFC criteria once passed
with the mechanism removed. Two CI gates once passed by matching the explanatory comment above the
thing they guarded. A proptest ran its whole reorg suite with `sealed_through` at zero, so the
finality guard never fired.

So the harness gets the treatment those lessons earned, on day one rather than at review: it is
mutated, a red run is quoted in its PR, and it must report a **coverage denominator** - how many
fields and selections it compared, not only how many matched. A validator that cannot say what it
declined to look at is not a validator.

## The spine

### 1. #1212 - RFC-0044 S3, the acceptance port

**Subject settled 2026-09-09: Uniswap V4 on Ethereum mainnet**, deployment
`Qmda2K4NcKWXB2AqyGUZEU35DgxSqFFRhkCmrJ8oC9po7i`. The reasoning, the deployment choice, the measured
shape and the cost are recorded on the issue and are not repeated here.

The part that belongs in a sprint document is why this satisfies §10's hardest clause, because it is
the clause that kept the slice parked. §10 wants a subgraph *neither of us picked for convenience*.
A user picked this one, in the Night's Watch Discord on 8 September, because the canonical
deployment stopped returning data (nightswatchhq/graph-support#32). Demand chose the subject. That
is a better answer than a partner nominating one on our behalf, and a much better one than us
choosing from our own catalogue.

Ninety `BigDecimal` fields across nineteen entities is the difficulty, and it is deliberate. They
are the ordered, stateful, mapping-derived family RFC-0038 §6a settled is unavailable
declaratively. **The slice passes only if every one of them was classified in advance.** An
unpredicted divergence fails it, which is the whole point: the report exists so that nobody
discovers a divergence after adopting the nest.

### 2. #1264 - RFC-0053 S0, the migration validator

First of RFC-0053's slices by design, and correctly so: the validator is the only slice that can
falsify the others. RFC-0053 was accepted on 2026-09-09, and the acceptance is deliberately scoped
to this one slice: **S0 produces the evidence the rest of RFC-0053 should be judged on**, rather
than being the first step of an approved build-out. S1 to S4 wait on what it measures.

Its acceptance is its own sentence, quoted above. Add the coverage denominator to it.

## Explicitly not in this sprint

- **#1265, #1266, #1267 - RFC-0053 S1 to S3.** The schema introspection, the query-dialect
  compiler and the entity history are the compatibility mode proper. They wait on what S0 measures.
  Building the compiler before the validator exists is the order RFC-0044 §7 already warned about in
  a smaller way, and RFC-0053 repeats the shape at a larger one.
- **#1268 - RFC-0053 S4, the mapping-derived value contract.** It is a decision, and #1212's report
  on ninety `BigDecimal` fields is the evidence that decision wants. Taking it before the port runs
  would be taking it blind.
- **#1213 - RFC-0044 S4, the query table.** Still gated on §12.2, and RFC-0053 may retire the
  question rather than answer it. Not this sprint either way.
- **RFC-0052's slices, #1258 to #1263.** Approved as a design on 2026-09-09, deliberately unsprinted.
  #1252 is a live p1 against §3.3 and wants fixing before any slice runs, but it is not this sprint's
  subject.
- **#1222, the segment-format version.** Third sprint running. The reasoning has not moved and
  neither has the issue; it wants a decision of its own rather than a slot.
- **The frozen register.** `docs/frozen-for-2027.md` is unchanged and reopens one item at a time
  under its own rule.

## How this sprint runs

**A test that passes proves nothing until it has been made to fail.** Carried forward unchanged
from the last two sprints, and it binds harder here than in either, because this sprint's single
deliverable is a comparison tool. A comparison tool that cannot fail is not evidence, it is
decoration.

**A finding is the work.** Where the port surfaces behaviour that is true by accident, that gets an
issue rather than a closing sentence.

**Say what did not reproduce.** #1212's output is three things and the third is the one that matters:
the nest, the report, and an honest list of what did not come across. A sprint that produces a
working nest and a quiet report has failed at the thing RFC-0044 exists to do.
