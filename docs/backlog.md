# The backlog

**GitHub Issues are the source of truth for what is left to do.** This file is orientation: what the
tracks mean, which decisions are settled, and how to query the queue.

It deliberately does **not** list individual items any more. It used to, and by 2026-08-06 the list
had drifted badly enough to be actively misleading: it still called RFC-0023 tier 1 "building" when
tiers 1-2 had shipped, still said "roost" a fortnight after the concept was retired, and had no row at
all for RFCs 0032-0035 - the entire 2.0 arc. Every one of its items already existed as an issue, each
one better written than the entry it came from. A second list is a list that drifts.

| Question | Where it is answered |
|---|---|
| What is each RFC, and is it built? | [RFC index](rfcs/README.md) |
| **What is left to do?** | **[Open issues](https://github.com/nightswatchhq/nuthatch/issues)** |
| What must be true before this runs unattended? | [prod-readiness.md](prod-readiness.md) |
| How do I prove a claim on my own hardware? | [verification.md](verification.md) |

## Reading the queue

Labels carry the meaning that this file's "tracks" used to:

| Label | What it means |
|---|---|
| `parked` | **Deferred by decision, not by oversight. Do not treat as a blocker.** |
| `rfc` | Tracks a named RFC slice; the RFC holds the design, the issue holds the state |
| `verification` | An unproven claim that needs a real run - not code, evidence |
| `performance` | Throughput, latency or footprint |
| `security` | Hardening or audit follow-up |
| `tech-debt` | Deferred cleanup with a recorded reason |

The one query that matters most:

```sh
gh issue list --limit 100 | grep -v parked     # everything actually actionable
gh issue list --label parked                   # everything deliberately not
```

## Standing decisions - do not re-raise these as blockers

These are settled. They are recorded here rather than in an issue because the answer is "no, and here
is why", which is not a task.

**A colocated reth node is deferred** (2026-07-29). It is the substrate RFC-0003 reads from and
RFC-0014 extracts from, and it stays the single unlock for that branch. The cost is provisioning plus
**days** of sync (full) or **TB and longer** (archive) - a hardware and ops job, not a coding session.
Deferring it is a decision. What it gates stays `parked`: ExEx wiring, trace/state extraction, an
honest tip-lag number, and RFC-0023 tier 3's pinned-block verification.

**DataFusion did not meet its gate** (2026-08-02). Measured at **1.6-2.7x DuckDB's latency** on
`net_balances` over sealed segments, widening with segment size, at exact result parity. DuckDB stays
in both modes. RFC-0013 §2's destination is *unmet, not repudiated* - re-run the gate before planning
around it, and do not re-argue it from first principles in the meantime.

**Turso is double-gated**, not rejected: a production-ready release, and a measured win over redb
that federation does not already provide. Until both, no.

The third gate - *a permissive, non-BSL licence* - is **dead, and was wrong when written**
(2026-07-17; corrected 2026-08-10). `tursodatabase/turso` and `tursodatabase/libsql` are both MIT,
checked against the GitHub API on 2026-08-10, so the licence never barred anything. Of the two that
remain, production-readiness is arguable rather than settled (production use is claimed at several
organisations, but it is pre-1.0 and some features are marked experimental), and the measured win has
**never been attempted**. #366 carries the measurement.

**Scaled mode is no longer infra-blocked.** RFC-0022 turned it into ordinary work - the `HotStore`
trait, a Postgres backend with a redb-parity suite, the query-FE role, and ownership fencing. The old
framing that "nothing in this track is verifiable on the dev laptop" was true when written and is now
only half true: scaled mode moved out of it, the node did not.

## The failure mode this file exists to prevent

Twice, entries here **outlived their fix** - proxy/EIP-1967 introspection and SSE push were both
listed as open long after they shipped, and the proxy entry was additionally misdiagnosed, describing
a gap that was already closed while the real one (a *bespoke* proxy that silently indexes zero rows)
went unrecorded until it cost a day on the Livepeer nest.

That is the argument for one list. An issue gets closed when the work lands, because closing it is how
you finish; a bullet in a document gets closed when someone remembers.

## History

Dated records, not live plans. Read them for *why*, never for *what is left*:

- [progress-log.md](progress-log.md) - what happened, when
- [sprint-amiable-axolotl.md](sprint-amiable-axolotl.md), [sprint-boisterous-badger.md](sprint-boisterous-badger.md) - sprint scopes as they stood
- [high-level-roadmap-aug-2026.md](high-level-roadmap-aug-2026.md) - the architecture session that produced RFCs 0032-0035
