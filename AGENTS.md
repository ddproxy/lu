# Agent guide

Lu is a Rust workspace: `lu` is the controller and operator CLI; `ting` is the node agent.
Read the code before changing docs or behavior. Every flag, key, default and error message in
this repository is verified against the source.

## Commands

Run from the workspace root.

| Command | Purpose |
| --- | --- |
| `make help` | List the make targets. |
| `make build` | Release binaries for `lu` (with the `tui` feature) and `ting`. |
| `make test` | Workspace tests, with and without the `tui` feature. |
| `make lint` | `cargo fmt --all --check` and clippy with warnings denied, both feature sets. |
| `make check` | `lint` then `test`. |
| `make docs-check` | README sections, doc links, CLI names and config examples. |
| `make install` | Install both binaries into `PREFIX/bin`; `DESTDIR` is honoured. |

For a quick smoke test before a wider run:

```sh
cargo test --locked -p lu --test check
cargo test --locked -p lu --test docs
```

## Repository map

| Path | Role |
| --- | --- |
| `app/src/main.rs`, `app/src/operator.rs`, `app/src/audit.rs`, `app/src/admin.rs` | `lu` CLI, flags and printed output |
| `app/src/bin/ting.rs` | `ting` CLI |
| `crates/lu-config` | Config tables, validation and defaults |
| `crates/lu-proto` | Wire types, job spec, states and numeric limits |
| `crates/lu-store` | SQLite schema, leases, epochs and idempotency |
| `crates/lu-controller` | HTTP controller and dispatch |
| `crates/lu-serve`, `crates/lu-auth` | Listener, TLS and bearer-token checks |
| `crates/lu-supervise`, `crates/lu-staging`, `crates/lu-client` | Harness supervision, workspaces and client calls |
| `crates/tuishou-ting` | Node loop, scoped harness socket and MCP |
| `crates/lu-audit`, `crates/lu-tui`, `crates/lu-schedule` | Audit chain, terminal view and scheduling |
| `docs/`, `README.md`, `CHANGELOG.md`, `SECURITY.md` | Operator-facing documentation |
| `scripts/` | CI helpers, including `check-docs.sh` and `check-monitoring.sh` |

## Conventions

- Unsafe code is forbidden and clippy warnings are denied for the workspace. Keep both feature
  sets (`cargo test --workspace` and `cargo test --workspace --features tui`) green.
- A docs change must match the code. Add or update the docs check when a flag, key, default or
  command is added, and run `make docs-check`.
- Keep docs examples runnable. Minimal and full config examples must pass `lu check --config`.
- Describe credentials and secret files in prose only. Do not add examples that contain key
  blocks, token values, bearer strings or similar material.
- The store schema version is code-owned. Change docs when it changes; never change the printed
  version alone. Migrations are forward-only.
- Do not edit generated monitoring assets by hand; use the generator described in
  `docs/monitoring.md`.
- Commit messages are terse, imperative and name the change, not a person or a job identifier.

## Security notes

Never paste a token, key or other credential into a task, a config example or a test fixture.
Token files must stay outside version control with private file modes. See `SECURITY.md` and
`docs/limits.md` for the boundaries Lu does and does not provide.
