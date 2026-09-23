# Sprint: temperate-tern

**Put checkpointed folds into the runtime and find out whether they hold their targets there. Alongside
that, give the hosted nest service the nuthatch release it depends on.**

## Definition of done

Every issue carrying the **`temperate-tern`** label is closed, and no open PR is for one of them. The
label, not this document, is the record of scope.

## Order

1. **#1473, record the hosted decision before building for it.** CLAUDE.md's out-of-scope list still
   forbids a hosted service. Chief decided on 2026-09-23 to run one from the private `nuthatch-hosted`
   repo, bounded by RFC-0046 §1's deletion test. That override lands first, so the sprint does not
   build what the standing brief forbids.
2. **#1475, then #1480, the release hosted nests needs.** A live mount of a second name onto an
   already-mounted NID fails on the redb lock, because `RuntimeHandles::mount` opens the dataset again
   where boot would share it. This is a multi-mount bug, and hosted nests is only where it showed up.
   Then cut a release carrying it and #1474 (`nuthatch nest nid`). v3.8.5 has neither.
3. **#1440, settle the packaging before S1 adds to it.** Decide whether the off-by-default `graph`
   feature takes the RFC-0053 surface that is already on main, before `folds` becomes a second
   feature that depends on the answer.
4. **#1481, RFC-0060 S1's other half.** Map the 21 client operations onto the fixtures that compile
   them on the RFC-0053 surface, and name any that do not. It is kill-or-continue for RFC-0060 as S0
   was for RFC-0059, so it goes before the runtime work it could stop.
5. **#1479, RFC-0059 S1: folds in the runtime.** Load `folds/`, bind carries and window-scoped facts,
   refuse volatile folds and schema mismatches at load, and read at `n` from a checkpoint. The
   delegation ledger passes S0's differential on nuthatch's own connection.

## Rules

- **The head targets are a gate, not an aspiration.** Chief kept 500 ms p99 and 256 MiB peak on
  2026-09-23, measured inside the running process. S0's cold process missed them by a fixed start-up
  cost. If S1 misses them in-process, the programme stops, and that result is reported rather than
  argued around.
- **The deletion test binds every change behind `folds` and `graph`.** A default build's CLI help, the
  config `init` writes, its on-disk layout and its behaviour are what they would be had neither RFC
  been written.
- **Hosted-specific code stays in `nuthatch-hosted`.** The binary gains general fixes, such as #1475,
  and nothing that only a hosted service would use. Delete the hosted repo and a self-hoster loses
  nothing.
- A release is cut through `release.yml` with all its gates. No gate is skipped because the fix inside
  is small.

## Not in this sprint

- RFC-0059 S2 to S4 (checkpoints from the seal loop, head snapshots, serving) and RFC-0060 S2 onwards.
  They are filed once S1 reports, because S1 can stop them.
- Deploying hosted nests to the box. It needs Chief: a GitHub OAuth app, DNS, and the release from #1480.
- RFC-0057 (#1381) stays `p2`. RFC-0045's stage 3 stays unfiled, per its §8.
- The parked items: the Dune sidecar and recorded run (#1362, #1360), warehouse runs (#1263),
  cross-nest SQL (#1324), the RFC gaps (#1313, #1288, #1280), and seal-direct recovery (#1446).
- Crates.io (#1299) is board-only.
- `docs/frozen-for-2027.md` and its reopening rule.
