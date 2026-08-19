# Release notes

`release.yml`'s `release-notes` job reads `docs/releases/<tag>.md` (e.g. `docs/releases/v2.6.0.md`)
and uses it as the GitHub release body. Write that file on the release branch, alongside the
`Cargo.toml` version bump, before tagging.

If the file is missing when the tag is pushed, the `release-notes` job fails and nothing further
runs - no build, no empty release to edit afterward. There is no fallback to an empty body; a
missing notes file is a build failure, not a warning.

No em dashes - spaced hyphen, matching the rest of the project's public-facing prose.
