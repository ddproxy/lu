# Security scanning

The workflows in `.github/workflows/` scan the code and its dependencies. Findings appear in the
GitHub Security tab, on pull requests, or in the Actions tab as noted below. `deny.toml` is the
licence and source policy; the dependency-review allow list mirrors it.

## What runs where

| Workflow | Checks | Triggers | Findings |
| --- | --- | --- | --- |
| `ci.yml` | Formatting, clippy, workspace tests and a static musl build | Push, pull request | Pull request checks and the Actions tab |
| `codeql.yml` | CodeQL static analysis of Rust with build mode `none` | Push to `main`, pull request, weekly | Security tab → Code scanning (SARIF, category `/language:rust`) |
| `scorecard.yml` | OpenSSF Scorecard checks, published publicly | Push to `main`, weekly | Security tab → Code scanning (SARIF), the README Scorecard badge and the `scorecard-results` artifact (5 days) |
| `dependency-review.yml` | Dependency changes with high-severity advisories or licences outside the `deny.toml` allow list | Pull request | Pull request checks |
| `audit.yml` | `cargo deny check` of advisories, bans, licences and sources | Push, pull request, daily | Pull request checks and the Actions tab |
| `fuzz.yml` | libFuzzer runs of the untrusted-input parsers, 120 seconds per target | Weekly, manual dispatch | Actions tab and the `fuzz-crashes-<target>` artifacts (7 days) |
| `release.yml` | CycloneDX SBOM and build provenance attestation for release archives | Tag `v*` | Release assets and `gh attestation verify` |

Scorecard runs only on github.com. For the threat model and hardening notes, see
[Security](security.md); reporting is covered by
[SECURITY.md](https://github.com/ddproxy/lu/blob/main/SECURITY.md).

## Fuzz locally

The `fuzz/` crate is a separate workspace of cargo-fuzz targets for parsers that handle untrusted
input: `patch_inflate`, `patch_apply`, `staging_path`, `artifact_path`, `job_spec`, `config_toml`,
`ids`, `wire_request` and `secret_scrub`. It uses hand-written seeds in `fuzz/corpus/<target>/`.

```sh
cargo install cargo-fuzz --locked --version 0.13.2
cargo +nightly fuzz run job_spec -- -max_total_time=30
```

Replace `job_spec` with any target above. Crashing inputs are written to
`fuzz/artifacts/<target>/`; keep them out of `fuzz/corpus/` until the fault is fixed. The weekly
workflow runs every target for 120 seconds and uploads any crashes as artifacts.

## Verify a release

Each release has one archive per target, its `.sha256` file, a CycloneDX JSON SBOM named
`lu-<tag>-sbom.json`, and a build provenance attestation for each archive.

```sh
tag=v0.4.0
target=x86_64-unknown-linux-musl
asset="lu-${tag}-${target}.tar.gz"

sha256sum -c "${asset}.sha256"
gh attestation verify "$asset" --repo ddproxy/lu
```

Replace the tag and target with the ones you downloaded. The checksum confirms the bytes match
the checksum published with the release; `gh attestation verify` confirms the archive was built by
this repository's release workflow. Inspect the SBOM with a CycloneDX tool to see the components
in the `lu` binary build with the `tui` feature.

## Repository settings (owner steps)

These settings are in GitHub, not in files in this repository. A repository owner must apply them.

- [ ] Enable **secret scanning** and **push protection** under Settings → Code security and analysis.
- [ ] Enable **Dependabot alerts** under Settings → Code security and analysis.
- [ ] Enable **private vulnerability reporting** under Settings → Code security and analysis. This
      is the channel named in [SECURITY.md](https://github.com/ddproxy/lu/blob/main/SECURITY.md).
- [ ] Protect `main` under Settings → Branches and require these status checks: `test`, `musl`,
      `dependency-review`, `cargo-deny`, `analyze`. Require branches to be up to date before merging.
- [ ] Leave code scanning **default setup disabled** under Settings → Code security and analysis:
      `codeql.yml` and `scorecard.yml` upload the SARIF used here.
- [ ] Register the project for the [OpenSSF Best Practices badge](https://www.bestpractices.dev/);
      add that badge to the README only after registration.
- [ ] Do not add a crates.io badge: the `lu` crate is not published.
