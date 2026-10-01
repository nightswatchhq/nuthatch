# Upgrade fixtures

Data and config written by released nuthatch, which `tests/upgrade_golden.rs` opens with the current
build. They are how the 4.x promise in `docs/operators.md#stability-contract` is enforced.

**Never regenerate or edit anything here. Only add.** A fixture that is rewritten to match a new
build tests nothing. When a release line is cut (4.1, 4.2, ...), add its own `v<version>/runtime`
beside the existing ones and a test that opens it; every older directory stays, and every one must
keep opening.

## `v3.13.2/runtime`

A runtime directory written by the v3.13.2 source, whose code is identical to 4.0.0's.

- Produced on 2026-09-30 from a checkout of tag `v3.13.2` (commit `59cc459`), with the only
  additions being `tests/upgrade_golden.rs` and its registration in `tests/it.rs`, on toolchain
  1.95.0 (aarch64-apple-darwin):

  ```sh
  NUTHATCH_UPGRADE_FIXTURE_OUT=$PWD/tests/fixtures/upgrade/v3.13.2/runtime \
    cargo test --locked --test it upgrade_golden::write_upgrade_fixture -- --ignored
  ```

- Offline and deterministic: the chain is the tests' scripted `TapeSource`, six blocks on
  `arbitrum-one`, each with one USDC `Transfer` of `100 * block` from `0x..01` to `0x..02`. Two runs
  gave the same NID and the same segment hash.
- The nest (`usdc`, one contract, and a `received` entity in `entities.toml`) was mounted as
  `primary` by `nuthatch migrate`, then given a second mount, `acme/mirror`, of the same dataset.
- The runtime indexed to block 6 and stopped. Blocks 1-4 were then sealed by `seal::seal_range`,
  the function the seal loop calls (`force_seal_through` in `tests/common/tape.rs`), because the
  seal loop itself only cuts a segment at 20,000 rows. Blocks 5-6 stay in the hot store.

What it holds:

| Path | What |
|---|---|
| `mounts.toml` | `[runtime]`, one `[[chains]]`, two `[[mounts]]` sharing NID `2bca0926...1576` |
| `data/<nid>/nuthatch.toml`, `entities.toml`, `entities/`, `abis/` | the nest |
| `data/<nid>/nuthatch.redb` | hot store: last block 6, sealed through 4, blocks 5-6 |
| `data/<nid>/segments/manifest.json` | one segment, blocks 1-4, 4 rows |
| `segments/48ef579c...6816.parquet` | that segment, in the shared content-addressed store |

`nuthatch.redb` is 3.6 MB on disk because redb allocates its first region up front; it compresses to
about 7 KB, which is what git stores. It is kept exactly as nuthatch wrote it rather than compacted,
because nuthatch never compacts a store and the test should open what an operator has.

## `config-4.0`

`nuthatch.toml`, `entities.toml` and `mounts.toml` as a 4.0 user writes them, frozen. The test
checks what each key means, not only that the file parses. The `mounts.toml` names the NID of the
v3.13.2 fixture but is not a runtime: it has no `data/`.
