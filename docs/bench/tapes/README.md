# Recorded tapes (RFC-0039)

Each directory is one recorded workload: `manifest.json` (chain, provider host, block range, recorded
date, content address) plus `entries.jsonl` (one line per unique `Source` call key, sorted by key).

Recorded once from a real provider, then replayed so a benchmark is a function of the code alone:

```sh
nuthatch bench backfill --dir <nest> --from <a> --to <b> --replay docs/bench/tapes/<name> --runs 5
```

A tape is content-addressed by the sha256 of `entries.jsonl`, and a replay refuses a tape whose bytes
have drifted from the address its manifest claims. `BenchReport.fixture_content_address` carries it,
so a published figure names the exact bytes it came from - `289 events/sec` outlived the harness that
produced it by five weeks because nothing tied the number to a run.

**`usdc-120-fixed`** - USDC (`0xa0b8…eB48`) on mainnet, blocks 25,809,368-25,809,487, `Transfer` only,
fixed 20-block window. 12 keys: six `logs` and six `block_timestamps`.

Five of its six `block_timestamps` keys carry a **recorded 429** from the public endpoint, preserved
deliberately. That is not a spoiled recording - it is what these endpoints actually do, it is how
#784 was found, and it is what makes that bug reproducible without a network. A tape that recorded
only the successes would have hidden it.

**`usdc-120-fixed-clean`** - the same nest, range and window, recorded against an endpoint whose
timestamp batches all succeed. Both storage-path arms replay this one. `tests/tape_clean.rs` fails
if a recorded error appears in it. Do not "fix" `usdc-120-fixed` by copying this over it.
