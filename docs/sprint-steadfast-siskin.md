# Sprint: steadfast-siskin

**The bugs first. Then RFC-0052 in full, and the Dune-facing view emitter it enables.**

## Definition of done

Every issue carrying the **`steadfast-siskin`** label is closed, and no open PR is for one of them. The
label, not this list, is the record of scope. RFC-0055's build slices join the label when the RFC is
accepted.

## Order

1. #1341 and #1342, and any bug filed during the sprint that is true for everyone.
2. #1343. RFC-0052 S1 against a real S3-compatible bucket in CI. S1 closed without it, and every later
   slice publishes to a bucket.
3. RFC-0052 S2 #1260, S3 #1261, S4 #1262.
4. #1344, the RFC-0055 draft, alongside 3 once research briefs 7 and 8 come back. Chief accepts it before
   any code; its build slices are filed from its own slice table.
5. RFC-0052 S5 #1263, then RFC-0055's build.
6. #1345, trackers and RFC status lines.

## Rules

- **Public sources only for anything Dune.** Chief, 2026-09-13: his inside knowledge does not shape this.
  An ingestion path, type mapping or convention that is not publicly documented is listed as
  unsupported, not guessed.
- **A recipe is only supported if it was run**, and the run is recorded (RFC-0052 §7, S5).
- **The mirror stays opt-in.** With no target configured the binary makes no network call it does not
  make today (RFC-0052 §2).

## Not in this sprint

#1216 and #1222, the 2026-09-11 enhancements (#1318 to #1324), RFC-0053's parked items (#1313, #1288,
#1280), #1299 (board), RFC-0054, and `docs/frozen-for-2027.md`.
