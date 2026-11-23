# Notes

- Schema version drift: `lu_store::SCHEMA_VERSION` is `6`, but several docs still describe version
  `4` as the current baseline (for example the note about `parent_job_id`/`root_job_id`/`depth` in
  [docs/api.md](docs/api.md)). The code creates new databases at version `6` and migrates versions
  `1` through `5` in place by adding columns. `docs/monitoring.md` and its `lu status` example were
  updated to `6`; the remaining prose is left for a separate docs cleanup.
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
# Verification notes

The referenced report was readable through the worklog vault. Claims relevant to this work were
checked against the tree before editing; the results follow.

## Confirmed in code

- `check_repo` only checked length, control characters and the URL/path shape; it accepted
  userinfo in the authority. `parse_http_url` in `lu-config` already rejects `@`, `?` and `#`.
- `job_view_with` copied `JobRecord.spec` into every `JobView`, so a stored repo was shown verbatim.
- No response set `Cache-Control` or `X-Content-Type-Options`. Artifact responses had no
  `Content-Disposition`; log responses had no `Content-Disposition` either.
- `limit` on jobs and drafts, and `offset` and `wait` on logs, silently defaulted when unparseable.
- Both operator and agent auth counted a missing token in the same `(peer IP, scope)` bucket as a
  wrong token, and the block check ran before the token was read.
- `docs/api.md` omitted retry, staged-commit and merge-base, and described `repo` as an https URL
  or configured local path. It did not document strict query parsing.
- Retry takes a full `JobSpec` body and returns `job`, `retried` pairs and `waiting_dependents`.
- Staged-commit and merge-base are agent routes fenced by the lease epoch.

## Where the code disagreed with the report

- The report said the security doc's fixed throttle numbers might be config-driven and unverified.
  They are hardcoded in `AppState::from_config` (`max_failures: 10`, `window: 60s`) and matched the
  doc. The new tokenless bucket is also hardcoded (100 in the same window) and is documented.
- The report said the code accepts any absolute path for `repo`. It also accepts any string with
  `://`, not just https URLs; the docs now say "a git URL with a scheme, or an absolute local path".
- The report described config mirror validation as rejecting userinfo only. It also rejects query
  and fragment. `source.repo` now rejects userinfo to align with that rule, while query and
  fragment are allowed on submit and stripped from stored values on read as requested.

## Scope decisions

- Redaction is applied where `JobView` values are built, so lists, single reads, retry replies and
  agent JobView replies all use the cleaned repo. The agent assignment event and scope job context
  keep the stored repo because an agent needs it for its fetch; new submissions cannot contain
  userinfo, and those paths are not job views.
- Audit records did not contain a repo URL before this change. `Data::text` now strips URL
  userinfo, query and fragment centrally, so a future field cannot leak one.
- Security headers are set both in the router middleware and `with_version`; the latter keeps
  direct handler calls and their tests consistent with served responses.
- The status route collapses every auth failure to 401. The integration check observes the
  wrong-token block through `POST /v1/jobs`, which surfaces the throttle error as 429.
# Second-round security review notes

All ten findings were verified against the current source and docs. None were rejected.

| # | Verdict | Disposition |
|---|---------|-------------|
| 1 | Confirmed | With default `scrub_patch = false`, the scope read path returned the raw patch while claiming `scrubbed: true`, despite the documented read-time re-scrub guarantee. Kept raw storage under the default option, but scrub `diff.patch` explicitly on scope read so the flag stays truthful. Added a regression test. |
| 2 | Confirmed | Pre-lease scope audit calls trusted the caller-supplied job id. They now use the `unknown` placeholder until lease verification succeeds. Added a regression test. |
| 3 | Confirmed | Context values are byte-capped and truncation stops on a character boundary. Added unit tests. |
| 4 | Confirmed | Derived idempotency keys reserve prefix bytes and truncate on a character boundary. Added a unit test. |
| 5 | Confirmed | Rollback now releases the exact admitted slot via an opaque ticket instead of popping the newest entry. Added a concurrency regression test. |
| 6 | Confirmed | The intake router body limit is set to the largest configured per-source cap; the handler still enforces each source's own cap. Updated the body-limit test. |
| 7 | Confirmed | Dedupe is checked before a rate slot is consumed, so retransmissions no longer burn rate capacity. Added a regression test. |
| 8 | Confirmed | The rate cutoff uses a guarded subtraction that cannot underflow. Added a unit test. |
| 9 | Confirmed | Presented bearer values are capped at 8 KiB; docs now reconcile that with the larger agent token-file limit. Added a unit test and updated the docs. |
| 10 | Confirmed | Mirror and sync failures return the generic internal error to clients while detailed errors are logged server-side. Added an API regression test. |

## Rejects

None.

## Validation

- `cargo fmt --all`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`

`.git` is root-owned `0700` in this checkout, so changes could not be committed; they remain local and uncommitted.

## Phase 1 scheduling implementation notes

These are the places where the shipped code differs from the design brief. The code was checked
before each choice; where the design and the repository disagreed, the repository won.

- `lu_store::SCHEMA_VERSION` is **5**, not the 4 in `docs/monitoring.md`. `eligible_since` therefore
  bumps it to **6**. The migration folds the new column into the baseline and into the additive
  path for versions 1–5, keeping the existing "refuse higher versions" policy.
- Cordons are finite: `cordons.until_mono_ms INTEGER NOT NULL`. `recover_on_boot` deletes every
  cordon, so a restart forgets provider cooldowns. Schedule waits report the stored upper bound,
  not "unknown".
- Dispatch order is creation-time `dispatch_seq`, not `eligible_since`. Ordering is now
  `(starvation class, effective priority, eligible_since, dispatch_seq)`. `eligible_since` is
  restamped at draft approval and dependency unblock.
- `submit_child` deliberately keeps `spec.group = None` for cap accounting. Calendar gates for a
  child are evaluated with the parent's group; `AdmissionRequest.group` carries that value.
- The task asks for top-level `[[calendar]]` and `[[attach]]`; the design showed nested
  `[[schedule.calendars]]` and `[schedule.scopes]`. Top-level tables are the wire form; the
  nested aliases are accepted where practical.
- `requires_calendar_state(t)` is `open(t)` for `state = "available"` and `!open(t)` for
  `state = "unavailable"`. The usable interval is `[begin + lead, end - tail)`. The design's
  "timeout minus tail" example was ambiguous: fit is applied on top of that interval, so a state
  gate needs `max(tail, timeout_secs + staging + finish)` of real state remaining.
- The fit rule requires `timeout_secs + staging_secs + finish_secs`; the allowance is configured in
  `[schedule]` and defaults to 300 s staging plus 60 s finish. `fit = "none"` disables the check.
- `days` names the local **start day** of a window. `to < from` (including `24:00`) crosses
  midnight; `from == to` is rejected at validation.
- Phase 1 has no timezone database, cron rules, DST, cost weights or burst, per v2 item 15.
  `[[calendar]] offset` accepts `Z`, `UTC`, `+HH:MM`, `-HH:MM`, `+HHMM` or integer minutes.
- The schedule change permission is `schedule = true` on `[[controller.operators]]` and defaults
  to `false`. Scoped MCP children cannot set schedule fields at all.
- Full calendar evaluation happens outside SQLite's `IMMEDIATE` transaction. The store trait gains
  `next_assignment_scheduled`, which defaults to the old `next_assignment` for test stores;
  SQLite first collects a read-only snapshot, applies cheap checks, asks the controller's schedule
  gate, then re-checks and leases in a short write transaction. The per-poll step budget is the
  configured `step_budget`.
- The wake timer always recomputes `next_change` after firing and re-arms; a fire before the
  boundary is a no-op. The last wall tick is stored in memory for the clock-fault check.
- `on_deadline = "run"` ignores only the job's soft gates (`window`, `requires_calendar_state`
  and fit). Global, group, node, provider and model attachments and `not_before` stay hard.
- The deadline ends a job as `deadline_missed`, a new terminal state, rather than reusing
  `expired`. `expired` remains draft-only.
- A per-provider start budget is configurable but defaults to zero (unlimited); phase 1's herd
  bound is one lease per node, with the provider rate-limit consequence documented in
  `docs/scheduling.md`.
