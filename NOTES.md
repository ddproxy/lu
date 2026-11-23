# Notes

- Schema version drift: `lu_store::SCHEMA_VERSION` is `5`, but several docs still describe version
  `4` as the current baseline (for example the note about `parent_job_id`/`root_job_id`/`depth` in
  [docs/api.md](docs/api.md)). The code creates new databases at version `5` and migrates versions
  `1` through `4` in place by adding columns. `docs/monitoring.md` and its `lu status` example were
  updated to `5`; the remaining prose is left for a separate docs cleanup.
- Job retention: Lu has no `DELETE FROM jobs` path. Node contribution counters are recomputed from
  the persistent `jobs` table, so they stay monotonic across controller restarts. A `since` window
  narrows the report by design and is not a monotonic counter source. If pruning is added later,
  these counters need a persistent rollup or an explicit reset semantic.
- There is no OpenAPI/Swagger document in this repository. [docs/api.md](docs/api.md) is the
  authoritative HTTP API reference; the stats route is documented there in the operator table and
  under `StatsReport`.
# Notes: agent self-drain

Verified differences between the requested assumptions and the code, plus the decisions taken.
No database schema change was needed.

## Verified against the code

- **Node “role”.** There is no node role field anywhere in the data model or wire types. The
  controller is a service, not a node, and its config holds agent/operator token hashes but no
  agent token for itself. Accordingly the three new routes accept only `authenticate_agent`, so an
  operator token or an anonymous caller gets the ordinary agent-route `401 unauthorized` with
  `WWW-Authenticate: Bearer`; a token is bound to exactly one node name and no caller-supplied node
  is read. “Refuse the controller role” is therefore satisfied by the existing scope check and
  node binding. No role column was added.
- **Drain state.** `nodes.draining` already existed as an integer used as a flag (`0`/`1`). It is
  now a tri-state: `0` not drained, `1` operator drain, `2` agent drain. `lu-store::DrainOrigin`
  decodes/encodes those values; `NodeRecord.drain_origin` carries it internally and
  `NodeView.drain_origin` exposes it additively as `"operator"` or `"agent"` to operator clients.
  Dispatch, metrics and job counts still treat any non-zero value as drained (`draining <> 0`), so
  an agent drain has the same effect as `lu drain`: no new assignment, running job continues.
- **Ownership.** The operator `set_draining(node, bool)` API still writes `0`/`1`. `set_agent_drain`
  is idempotent and deliberately does not replace an operator drain. `clear_agent_drain` is atomic,
  idempotent when not drained, and returns `StoreError::Conflict` for an operator drain, which the
  route maps to `409 conflict`. Both use an immediate transaction and refuse nodes that have never
  heartbeated (`404`).
- **Schema version.** `crates/lu-store/src/lib.rs` defines `SCHEMA_VERSION = 5`; no migration,
  column or baseline change was introduced and the version was left at 5. Several existing docs
  still describe a single schema-4 baseline and schema-4 backup/restore rules (for example
  `docs/operations.md` and `docs/api.md`); that pre-existing drift is recorded here rather than
  silently rewritten, because this change does not touch backup or migration policy.
- **Rate limiting and error shape.** The new routes go through the same `authenticate_agent` +
  shared auth throttle as the other agent routes; there is no new per-route quota. Rejections use
  the shared `ApiError` envelope `{"error":"...","message":"..."}` with the same codes and
  `with_version` header handling. The expensive mirror/base-pack per-principal budgets are
  unchanged and do not apply to these small routes.
- **Input handling.** `GET /v1/agent/self` rejects any query string. `POST /v1/agent/drain` and
  `/v1/agent/undrain` reject any query string and accept an empty body or `{}` only; unknown body
  fields are `400 bad_request`. The node name can only come from the bearer token.
- **Self view.** `lu_proto::AgentSelfView` has exactly `name`, `version`, `draining`,
  `current_job`, `last_heartbeat_age_secs`, `online`; the fixed key set is asserted in the API test.
- **Audit.** Self reads write `Event::AgentSelf` (`"agent_self"`). Drain/undrain write
  `Event::Drain` (`"drain"`) with `source = "agent"` and `state = true|false`, actor = agent node
  name; a refused undrain of an operator drain also records `result = "conflict"`. Self writes
  `source = "agent"`.
- **Client and CLI.** `lu-client` gained `agent_self`, `agent_drain` and `agent_undrain`; the two
  POSTs send `{}` with no retry. `ting` reads the node config and its `[agent].token_file` via the
  existing private-file reader and never prints the token. `ting status --json` serialises the
  self view directly, so its field order is stable.
- **Default config path.** The `ting --config` default was a relative `ting.toml`; it is now
  `/etc/ting/ting.toml`, matching `docs/install.md` and the contributed systemd unit. An explicit
  `--config` still overrides it. Missing/unreadable config or token exits `2` with a message naming
  the path; controller/HTTP errors exit `1`; `wait-idle` timeout exits `3`; clap usage errors exit
  `2`. `--timeout` is required and accepts seconds or an `s`/`m`/`h` suffix. Polling is one second.
- **Test harness caveat.** The new binary tests use `#[tokio::test(flavor = "multi_thread",
  worker_threads = 2)]`: `run_bin` blocks the test thread, so the fake-server task needs another
  worker or the client sees a request timeout. The CLI test file does not contain any token-shaped
  literal; tokens are generated at runtime from `lu_proto::JobId::new()`.

## Verification

- `cargo test -p lu-controller --test api agent_` — 7 passed.
- `cargo test -p lu --test ting` — 7 passed.
- `cargo check --workspace --all-targets` — passed after the implementation.
- `cargo fmt --all -- --check` — clean after formatting.
- `cargo clippy --workspace --all-targets -- -D warnings` — passed; the only warning is Cargo's
  pre-existing note that the unrelated `fw-tui` patch is not used in the crate graph.
- `cargo test --workspace` — passed (all crates; 46 test binaries/doc-test groups reported `ok`,
  including 59 controller API tests, 7 new API self/drain tests and 7 new `ting` binary tests).

## Outstanding environment limitation

The checkout's `.git` directory is not writable, so no commit could be created. Commit messages
were kept terse as requested; report this limitation rather than claiming a commit exists.
