# Changelog

All notable changes are recorded here. The format follows Keep a Changelog, and the project
uses semantic versioning once it reaches 1.0.

## [0.1.0] - 2026-11-16

First packaging release.

### Added

- One `lu` binary with controller, operator CLI and an optional terminal dashboard (`--features
  tui`).
- Controller HTTP(S) API over SQLite: job submission with idempotency keys, listing, state and
  log reads, artifact download, cancellation, node listing, drains and provider cordons.
- Bearer-token authentication with separate operator and agent scopes, SHA-256 token hashes,
  per-address failure throttling, connection caps and request/body limits.
- Lease-based agent assignment with epoch fencing, boot recovery, retry classes for node and
  provider failures, and `max_retries`.
- `lu check`, `lu serve`, `lu submit`, `lu ls`, `lu show`, `lu logs`, `lu fetch`, `lu cancel`,
  `lu nodes`, `lu drain` and `lu undrain`, with documented exit codes.
- Validated TOML configuration, private-file checks for tokens and the TLS key, loopback-only
  plain HTTP, and TLS for non-loopback listeners.
- `lu-supervise` for harness process groups, timeouts and capped output, and `lu-staging` for
  per-job checkouts, diffs, artifact limits and cleanup.
- systemd units, sysusers/tmpfiles files and sample controller/agent configs under `contrib/`.
- CI for formatting, clippy (default and `tui`), workspace tests and `cargo deny`, plus tagged
  GNU release builds for x86_64 and aarch64 with SHA-256 checksums.
- Configuration, operations and security documentation.

### Known gaps in 0.1.0

- The agent runs the harness as its own user, so a harness can read the agent's token file.
  Run the agent as a dedicated account that holds nothing else. Privilege separation is planned.
- Leaf agents use their own credentials to check out private repositories; controller-side
  fetch is planned.
- The agent reports no provider health, so the controller does not yet steer around a cooling
  provider.
- `[service].shutdown_timeout_secs` is validated but the in-flight drain is fixed at 10 seconds.
- `[[controller.agents]].cordoned` is parsed but does not seed the node's drain state.
- Provider credentials stay on the agent/leaf host; there is no controller-side credential
  store.
