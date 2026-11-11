# Contributing

[`AGENTS.md`](AGENTS.md) holds the ground rules, the build gate every commit passes, and the guardrails a change must
keep. Planned work lives in [`docs/plans/fork-roadmap.md`](docs/plans/fork-roadmap.md), and the libvirt test bed in
[`tests/testbed/`](tests/testbed/README.md).

## Versions and releases

Versions follow [Semantic Versioning](https://semver.org/). The version speaks for this public interface:

- the MCP tools: their names, arguments, and what their results and refusals mean;
- the configuration file's keys and their defaults;
- the `virtmanager-mcp` command line, including the `sweep` command and its exit statuses; and
- the arguments of `scripts/make-template.sh` and `scripts/fetch-ocr-models.sh`.

While the version is below 1.0, a release that breaks any of these raises the minor number (0.2.3 to 0.3.0). A
release of fixes alone raises the patch number (0.3.0 to 0.3.1). Additions that break nothing, such as a new tool or
a new configuration key with a safe default, also raise only the patch number below 1.0, so a minor bump always
means something broke. From 1.0 on, a break raises the major number and an addition the minor number.

The version lives in `Cargo.toml`, and the server reports it to MCP clients. Each release has a Git tag `v` plus that
version, such as `v0.2.0`, and a GitHub release on this repository. The release carries the notes from
[`CHANGELOG.md`](CHANGELOG.md) and a build for x86_64 Linux. That build links dynamically against the libvirt and
glibc of GitHub's `ubuntu-latest` runner. Nothing is published to crates.io.

### Make a release

1. Move the entries under `## Unreleased` in `CHANGELOG.md` into a new `## X.Y.Z` section below it.
2. Set `version = "X.Y.Z"` in `Cargo.toml`, and run `cargo build --locked` so `Cargo.lock` follows.
3. Commit both files as `chore(release): X.Y.Z`, and push `main`.
4. Once CI passes on `main`, tag the commit with `git tag -a vX.Y.Z -m vX.Y.Z`.
5. Push the tag with `git push origin vX.Y.Z`.

The tag runs CI again. The `release` job runs only when every other job passes. It fails when the tag does not match
the version in `Cargo.toml`, or when `CHANGELOG.md` has no section for the version. A tag with a pre-release suffix,
such as `v0.3.0-rc.1`, becomes a GitHub pre-release with generated notes and needs no changelog section.

If the `release` job fails, delete the tag locally and on GitHub, fix the cause on `main`, and tag again.
