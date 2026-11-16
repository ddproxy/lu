# lu

A job queue and dispatcher for dev peers: submit a job once, and an agent on a peer with room runs it with a coding harness and reports back. One binary (`lu`), SQLite, plain HTTPS, a CLI, and later a TUI. Early: the workspace was started from the Rust reference workspace (config, auth, listener, terminal view), and the Lu crates are being filled in.

The design is in the project vault: controller and agents, lease fencing, retries, process-group supervision, artifacts and auth. Read it before changing the protocol.

## Crates

| Crate | Purpose |
| --- | --- |
| `lu-config` | validated TOML config |
| `lu-auth` | hashed bearer tokens, constant-time match, per-address failure throttle |
| `lu-serve` | HTTP(S) listener with connection caps, request ids and clean shutdown |
| `lu-tui` | terminal view (feature `tui`, off by default) |
| `lu-proto` | wire types: job spec, states, events, limits, output sanitising |
| `lu-store` | the controller's SQLite store: jobs, leases and fencing epochs, retries, idempotency, cordons |
| `lu-supervise` | harness supervision in a process group, with timeouts and capped output |
| `lu-staging` | per-job checkout, diff export, artifact caps, safe cleanup |
| `app` | the `lu` binary |

## Commands

```sh
cargo test --workspace
cargo run -p lu -- check --config lu.toml
make install      # release build into ~/.local/bin, or /usr/local/bin as root
```

MIT. See `LICENSE`.
