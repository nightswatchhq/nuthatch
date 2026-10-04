# Installing nuthatch

The short version is in the [README](../README.md#install). This page is the detail behind it.

## The installer

```sh
curl -fsSL https://nuthatch-indexer.com/install.sh | sh
```

It downloads the prebuilt binary for your platform from the latest release, verifies its SHA-256, and
installs it to `~/.local/bin` (override with `NUTHATCH_INSTALL_DIR`). No compiler is involved, so
whichever rustc you happen to have is irrelevant.

A stock macOS shell does not have `~/.local/bin` on its `PATH`, so right after installing,
`nuthatch` is "command not found" until you run `export PATH="$HOME/.local/bin:$PATH"`. Put that line
in `~/.zshrc` (or `~/.bashrc`) to keep it for new terminals. Many Linux distributions already add the
directory when it exists; if yours does not, the same line applies. The installer itself is served from
the website and lives in its own repository, so this is documented here rather than changed there.

Prebuilt binaries are published for two targets, attached to every release with their checksums:

| Platform | Target | Prebuilt |
|---|---|---|
| macOS, Apple Silicon | `aarch64-apple-darwin` | yes |
| Linux x86_64 | `x86_64-unknown-linux-gnu` | yes |
| macOS, Intel | `x86_64-apple-darwin` | **no** - build from source |
| Linux aarch64, anything else | - | **no** - build from source |

On a platform without a prebuilt binary the installer stops and prints the source-build command
below rather than installing something that will not run.

## Linux requirements

The Linux binary is dynamically linked and needs one thing, measured off the published artifact
with `objdump -T` rather than inferred.

**glibc 2.35 or newer.** This is the measured ABI floor, read with `objdump -T` from the published
binary: 4.1.0 references `hypot` and `hypotf` at `GLIBC_2.35`, where libm re-versioned them. It is
also the glibc the release is *built* on (`ubuntu-22.04` in `.github/workflows/release.yml`), which is
a coincidence and not the reason: up to 4.0.2 the binary referenced nothing newer than `GLIBC_2.34`
and ran on RHEL 9, and stating the build baseline as the requirement once wrongly excluded it
([#978](https://github.com/nightswatchhq/nuthatch/issues/978)). The floor is what the loader checks.

**No C++ runtime.** The binary links `libc`, `libm` and `libgcc`. Releases before 4.1 embedded DuckDB,
which is C++, and also needed libstdc++ from GCC 11 (`GLIBCXX_3.4.29`).

Debian 12 and Ubuntu 22.04 clear it. RHEL 9 and Amazon Linux 2023 ship glibc 2.34: they ran 4.0.x and
need the source build for 4.1.0.

## Verifying a download

The SHA-256 sidecar beside each tarball tells you the file did not corrupt in transit, and nothing
more: whoever could replace the tarball could replace the sidecar in the same breath. Every release
binary therefore carries a **build provenance attestation**, signed by GitHub's identity for the
workflow run that produced it and recorded in a public transparency log. It needs no key from us:

```sh
gh attestation verify nuthatch-x86_64-unknown-linux-gnu.tar.gz --repo nightswatchhq/nuthatch
```

That answers what a checksum cannot: which repository, which commit and which workflow built the file.
`--repo` is the load-bearing part - without it, an attestation from *any* repository is accepted,
which is most of the property you are checking for. The release workflow runs the same verification
against its own assets before it publishes, so a release whose provenance does not verify never
becomes public.

## Building from source

This is the only route on a platform without a prebuilt binary, **Intel Macs included**:

```sh
rustup toolchain install 1.95.0
cargo +1.95.0 install --git https://github.com/nightswatchhq/nuthatch nuthatch
```

The toolchain pin is load-bearing. `rust-toolchain.toml` pins 1.95.0 because `dbsp` hits a
next-trait-solver ICE on 1.97, and **that file does not apply to `cargo install --git`**, which builds
in a temporary directory of its own. Without `+1.95.0`, a 1.97 default toolchain fails after a full
dependency build with `error: could not compile dbsp` and installs nothing
([#534](https://github.com/nightswatchhq/nuthatch/issues/534)).

The Intel Mac build is not in CI and has not been verified on Intel hardware. If it fails, an issue
with the error is welcome.

## Container images

Published per release to `ghcr.io/nightswatchhq/nuthatch`: `:<version>` for embedded mode,
`:<version>-scaled` for the scaled build. The image ships the same binary attached to the release, so
the two cannot drift. Its base is `debian:bookworm-slim` (glibc 2.36), which must stay at or above the
release builder's glibc.
