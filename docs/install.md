# Installing nuthatch

The short version is in the [README](../README.md#install). This page is the detail behind it.

## The installer

```sh
curl -fsSL https://nuthatch-indexer.com/install.sh | sh
```

It downloads the prebuilt binary for your platform from the latest release, verifies its SHA-256, and
installs it to `~/.local/bin` (override with `NUTHATCH_INSTALL_DIR`). No compiler is involved, so
whichever rustc you happen to have is irrelevant.

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

**glibc 2.34 or newer.** This is the measured ABI floor. The release is *built* on glibc 2.35
(`ubuntu-22.04` in `.github/workflows/release.yml`), but building on 2.35 does not make 2.35 a runtime
requirement: the binary references no symbol newer than `GLIBC_2.34`, and that reference set is what
the loader checks. The two numbers answer different questions - 2.34 is what you need to run it, 2.35
is what we compile it on - and stating the build baseline as the requirement once excluded a platform
we support ([#978](https://github.com/nightswatchhq/nuthatch/issues/978): RHEL 9 ships glibc 2.34).

**No C++ runtime.** The binary links `libc`, `libm` and `libgcc`. Releases before 4.1 embedded DuckDB,
which is C++, and also needed libstdc++ from GCC 11 (`GLIBCXX_3.4.29`).

Debian 12, Ubuntu 22.04, RHEL 9 and Amazon Linux 2023 clear it.

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
