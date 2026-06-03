# holys3

ripgrep over a private S3 bucket, accelerated by a trigram index (Stage 1).
See `docs/superpowers/specs/2026-06-01-holys3-design.md` for the full design.

## Try it (local corpus)

    cargo run -p holys3 -- index  --local-dir <dir> --out idx
    cargo run -p holys3 -- search "<regex>" --local-dir <dir> --index idx
    cargo run -p holys3 -- stats  --index idx

## Releases

holys3 is released with [release-plz](https://release-plz.dev/). It is the
primary tool because it understands a multi-crate workspace: it computes the
**topological publish order** automatically (libs before the `holys3` binary),
bumps versions per [SemVer](https://semver.org/) from
[Conventional Commits](https://www.conventionalcommits.org), generates the
`CHANGELOG.md` (via [git-cliff](https://git-cliff.org/)), tags, and publishes to
crates.io.

How a release works:

1. You merge PRs to `main` using Conventional Commit messages (`feat:`, `fix:`,
   `feat!:`/`BREAKING CHANGE:` for majors).
2. The **release-plz PR** job keeps an open "Release" PR up to date — bumped
   `Cargo.toml` versions and a regenerated changelog. Review and merge it when
   you want to cut a release.
3. Merging it triggers the **release-plz release** job: it tags the release and
   runs `cargo publish` for each crate in dependency order to crates.io (via
   crates.io [trusted publishing](https://crates.io/docs/trusted-publishing),
   so no long-lived token is stored).
4. The tag fires the **binary build** workflow: a cross/OS build matrix compiles
   the `holys3` CLI for each target and attaches the archives to the GitHub
   Release.

Alternatives, for context:

- **[cargo-release](https://github.com/crate-ci/cargo-release)** — a local,
  scripted release command (`cargo release`). Excellent and battle-tested, but
  you drive it manually and wire your own changelog/CI; release-plz automates
  the same steps via PRs.
- **[cargo-dist](https://github.com/axodotdev/cargo-dist)** (the `dist` tool) —
  focused on building and packaging binaries/installers, not crates.io
  publishing. After Axo wound down it is community-maintained (still active —
  see the r/rust "cargo-dist is not dead" PSA). You can swap our matrix
  build-and-attach job for `dist` if you want richer installers; the crates.io
  side stays on release-plz either way.

The MSRV is declared once in `Cargo.toml` (`[workspace.package] rust-version`)
and enforced by the `msrv` CI job. Bumping it is a minor-version change.
