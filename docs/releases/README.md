# Release notes

`release.yml`'s `release-notes` job reads `docs/releases/<tag>.md` (e.g. `docs/releases/v2.6.0.md`)
and uses it as the GitHub release body. Write that file on the release branch, alongside the
`Cargo.toml` version bump, before tagging.

If the file is missing when the tag is pushed, the `release-notes` job fails and nothing further
runs - no build, no empty release to edit afterward. There is no fallback to an empty body; a
missing notes file is a build failure, not a warning.

No em dashes - spaced hyphen, matching the rest of the project's public-facing prose.

## Pre-releases

A tag carrying a semver pre-release identifier - the `-` in `v3.0.0-alpha.1` - is published as a
GitHub pre-release. The workflow decides this from the tag itself, so nothing is ticked by hand
and nothing is remembered. A pre-release never takes the repository's "Latest release" badge,
which is the whole point: `curl | sh` and anyone arriving at the repo front page continue to be
offered the last stable version.

`action-gh-release`'s own `prerelease` input defaults to **false**, so this had to be stated
rather than left to the default; the `publish` job restates it on the undraft and prints
`prerelease=` alongside `draft=` so the tag run says out loud what it did.
