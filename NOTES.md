# Notes

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

The referenced report was available during review. Claims relevant to this work were checked
against the tree before editing; the results follow.

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

## Web UI write API notes

These are the places where the write-side brief and the existing code disagreed, plus the races the
HTTP guards cannot close. The repository behaviour won except where the brief named an exact status
code.

- **`Idempotency-Key` conflict status.** The existing tree used `409 idempotency_conflict` for a
  reused key. The brief requires `422`, so `ErrorCode::IdempotencyConflict` now maps to HTTP 422.
  The code name is kept because clients can distinguish it from a generic `unprocessable` error.
  This also matches the brief's "same key, different body" wording; a different operation with the
  same key gets the same result.
- **Create with `If-Match: *`.** The store can only test whether the spec's own `idempotency_key`
  is already present. A spec without one has no stored identity, so the header cannot make the
  request conditional; the docs say this rather than implying a scan of every job.
- **ETag source and viewer reads.** `job_if_match_guard` recomputes the same `JobView` body used by
  `GET /v1/jobs/{id}` and hashes it with `read_api::etag_for`, so a GET ETag is accepted. A viewer
  response has task and context removed by `role.rs` after the ETag header was set, so its body no
  longer matches that ETag. Viewers cannot call any mutation, so this cannot be used to bypass a
  guard; it is recorded because the read-side invariant is not literally true for viewers.
- **`updated_at` granularity.** SQLite checks use the `updated_at` seconds observed with the GET
  ETag. Create, retry, cancel and approval without an edit check and write in one immediate
  transaction, but two writes inside the same second can produce an ABA race that the ETag's body
  hash would have seen. `updated_at` is the only stored validator; a full body hash would need a new
  column or table, which the one-table compatibility policy rules out.
- **Multi-step paths.** `publish` runs an external git push before the store update, so the
  `If-Match` check cannot hold across that side effect. `edit`, `reject` and `approve` with an
  `edit` body check the validator, then write in separate transactions because the edit helper owns
  its own transaction. A job can change between those steps. The guard still refuses a request that
  is already stale at check time, and the failure is audited.
- **Request-key lifecycle.** A key is reserved in its own transaction before the mutation and
  completed or released after the response is built. A process crash between reserve and complete
  leaves a pending row that conflicts with the same key until its 24-hour expiry. Failed mutations
  release the key. The stored fingerprint is over the operation name and the raw request bytes, so
  semantically equal JSON with different whitespace is a different body. Replays preserve the
  status and the JSON object; key order may differ because the stored response is a JSON value.
- **Audit fields.** Applied, replayed, precondition-failed and dry-run outcomes carry
  `data.principal`, `data.role`, `data.operation` and `data.outcome`. Existing lifecycle events keep
  the operator in `actor` and the role in `data.source`, which was already their contract.
- **`OperatorRole::Admin`.** The role is parsed and reported in audit data but has no extra
  permission; only `schedule = true` gates schedule reload. This is unchanged.
- **`restrict_fields` on retry.** The original check covered only create. Retry now applies the
  same `restricted_field` check to the parsed or stored spec, so a restricted token cannot widen
  `allow_spawn`, spawn fields, stack paths or source-branch publishing through the retry route.
- **Audit actor for publish.** Before this change the publish helper used the fixed actor
  `"operator"`. The handler now emits a `publish` audit event with the real principal and role in
  addition to the helper's result event.
- **Dry-run ordering.** Create and workspace clean check `If-Match: *` before parsing their body or
  queueing work. Retry and cancel validate the spec/query and check `If-Match` before returning the
  dry-run response. A stale validator produces `412` and a precondition audit, not a dry-run audit.
- **Compatibility probes.** `scripts/compat-test.sh` sends the new headers and `?dry_run=1` to both
  controller builds. The mutating probe uses a harmless in-memory group limit, and the body probe
  uses an invalid job spec so neither build stores anything.

## Scheduling documentation reconciliation

Claims below were checked against the code while updating the phase-1 scheduling docs. Where a
claim disagreed with the tree, the tree won and the docs were corrected; no feature work was done.

- **State count.** The repo has twelve `JobState`s, not eleven. The twelfth is `deadline_missed`,
  terminal, reachable only from `queued` or `blocked`. `expire` and `cancel` both end there with
  summary `deadline missed` and a `deadline_missed` audit event; neither reuses `expired`.
  `StatsReport` and node contribution series omit it, but `lu status --json`, `GET /v1/status` and
  `lu_jobs{state="deadline_missed"}` include it. Older docs claiming four terminal states were
  wrong.
- **Dispatch order.** `SQLiteStore::next_assignment_scheduled` sorts by starvation class desc,
  effective priority desc, `dispatch_seq` asc, then `eligible_since` asc. `queue_key()` returns
  `(starved, effective, eligible_since, dispatch_seq)` but the comparator puts `.3` before `.2`, so
  the tuple order alone is misleading. The earlier note in this file that listed
  `eligible_since` before `dispatch_seq` was also wrong. Effective priority can rise by up to two
  points with age; a starved job outranks every static priority. Docs that said "priority then
  submission order" were corrected.
- **Config reload.** There is no general config, token, TLS or agent reload and `SIGHUP` is not
  handled, but `POST /v1/schedule/reload` exists. It re-reads the controller config path, validates
  the whole file, and atomically swaps only the schedule; a rejected reload leaves the active
  schedule unchanged. It requires `schedule = true` on `[[controller.operators]]` (default deny).
  Read-only schedule routes need an ordinary non-viewer operator token; viewers are not in the
  allow-list.
- **API surface.** `JobSpec.schedule`, `JobEdit.schedule`, `JobView.schedule` and
  `JobView.waiting_detail` exist. `waiting_detail` is filled by the single-job view path, not the
  list path. `GET /v1/schedule`, `GET /v1/schedule/calendars{/,name}`, `POST /v1/schedule/explain`
  and `POST /v1/schedule/reload` are registered. The docs now list them; the CLI has no
  `lu schedule` command and no scheduling flags on submit/edit/retry/collect/batch/MCP paths.
- **Metrics.** No `lu_jobs_waiting_schedule{code}`, schedule-wait counter or reload counter is
  exported, despite comments in `state.rs`. Schedule wait/reload counters exist only in the
  controller process. `JobView.waiting_detail` is the structured local view of the same evaluator.
- **Stale source comments (left unchanged, docs follow the code).** `state.rs` says the schedule is
  swapped by `lu schedule reload` (no such command), `lu-schedule` says `next_change` is shared with
  `lu schedule`, `on_deadline = "cancel"` is described as ending "as cancelled" (the wire state is
  `deadline_missed`), and `Schedule::admissible` is described as used at submit (only
  `POST /v1/schedule/explain` and tests call it). The earlier note in this file claiming a
  configurable per-provider start budget also has no matching config key or code path.
- **Docs test.** `crates/lu-config/tests/docs.rs` extracts `toml` blocks from `docs/scheduling.md`,
  checks their key paths against an allowlist, deserialises them with `lu_config::Config::parse`
  using runtime-generated token hashes, and proves an unknown key is rejected. No token-shaped
  value is stored in source.

Work remains local and uncommitted: the checkout's `.git` directory is root-owned and unwritable
by the current uid, so `git status` fails and no commit could be created. Commit messages were kept
terse as requested.

## `lu top` sortable table verification

### Verified against the code

- The shared widget is `fw_tui::widgets::render_table_sorted`, pinned to main commit
  `8a9fb414c79d7784009e1a3ec3a3688e197931fe`. It owns header rendering, the sort indicator,
  header hit-testing through `sort_column_at`, and key handling through `handle_sort_key`.
- `JobView.spec.priority` already existed and is `0-9`; `JobRow.priority` now carries it into
  the TUI and the `PRI` column is inserted after `STATE`.
- The sort kinds are per table column: numeric for `PRI`, `TRY` and the three `WS` counters;
  duration for `AGE`, `WAIT`, `RUN` and `TOTAL`; text for names, providers, models, repos and
  notes. `STATE` and `WS TOTALS` use custom comparators for semantic state order and
  completed/failed/running totals respectively.
- Column widths come from the TUI's existing `LayoutInfo`. The widget caps a cell to 40
  characters, so `NOTE_MAX` was reduced from 64 to 40 to avoid a second, log-specific cap.
- Selection is restored by `JobRow.id` and `NodeRow.name`, so it follows the same row across
  data refreshes and re-sorts. Row keys are also registered with the widget so its own row
  tracking uses the same identity.

### Where the code disagreed with the request

- The shared widget draws the table header itself, on the inner row directly below the
  dashboard. The TUI no longer stamps a separate header line; `LayoutInfo` now reserves that
  row, and the narrow-column fit logic drops trailing columns rather than clipping identity or
  state cells.
- The widget's minimum row height reserves two inner rows for the header and scroll window.
  The graph and table areas were adjusted to yield those rows; this slightly changes the 80-column
  golden layout.
- The sort indicator is drawn inside the column width. The three-cell `PRI` and `TRY` columns
  cannot show an arrow alongside the header text at 80 columns, although they remain sortable;
  wider columns display the indicator.
- Mouse wheel and double-click still use Lu's own hit-testing because they change selection and
  scroll position rather than sort order. Displayed column indices are translated back to data
  column indices through `LayoutInfo.*_order` so these actions track the sorted and hidden
  columns.
- `ViewModel.lines` remains the source for `--once` and existing text tests. The sorted order is
  applied at render time so the plain-text path and its tests keep their original ordering.

## Sick-node heartbeat gating verification

### Verified against the code

- `Heartbeat` and `RawHeartbeat` now carry `harness_ok: Option<bool>` and
  `disk_free_pct: Option<u8>`. `RawHeartbeat` has no `deny_unknown_fields`, so unknown fields are
  ignored in either direction as `docs/compatibility.md` requires. `disk_free_pct > 100` is
  rejected during heartbeat validation.
- `ting` derives both fields from the same signals it already had: `harness_ok` from its
  harness-launch atomic, and `disk_free_pct` from the free/total work-filesystem bytes in
  `HostStats`. Both heartbeat builders (idle and busy paths) send the values.
- The `nodes` row stores the whole heartbeat as `heartbeat_json`, so the new values persist with
  the node record and no schema migration is needed. `SCHEMA_VERSION` stays `7`.
- The disk gate is enforced inside the SQLite dispatch transaction, so a concurrent poll cannot
  lease to a node whose latest heartbeat is unhealthy. Recovery needs only the next heartbeat.
- Absent fields are treated as healthy for old agents. The config key was added to `[schedule]`
  because no `[limits]` table exists in the code or docs. `0` disables the disk gate.
- Controller tests cover a blocked then recovered node, old agents without the fields, and a
  zero threshold. `lu nodes`, `lu node`, `GET /v1/nodes`, job reasons and `/metrics` expose the
  new state.

### Where the code disagreed with the request

- The task asked for the reason in `blocked_reason`. In the code, `JobView.waiting_reason` is the
  canonical field for why a `queued` job cannot be placed, while `blocked_reason` is persisted for
  jobs in `blocked` state after a failed dependency. Both are populated with the new health reason
  for queued jobs, without changing stored job state, so the CLI and API wording stays consistent
  with the request while the canonical reason remains available.
- `/metrics` already exported `lu_node_disk_free_percent{node}`, derived from heartbeat byte
  totals. The new `lu_node_disk_free_pct{node}` reports the explicit heartbeat percentage; both
  series are emitted, with different names and absent-field behaviour.
- The agent already published harness health as a `harness_ok=false` label and taint. The new
  boolean is an independent signal used by the controller gate; the label and taint are unchanged.
- `docs/monitoring.md` claimed schema version `6` while `SCHEMA_VERSION` and
  `docs/operations.md` say `7`. The monitoring doc was corrected to match the code.
- `docs/api.md` says request bodies reject unknown fields, but heartbeat decoding is deliberately
  lenient for forward compatibility. The heartbeat-specific paragraph now describes the actual
  behaviour.

## Compatibility runner reconciliation

Claims in the request were checked against the existing script and workflows before editing.

- **Existing interface.** `scripts/compat-test.sh` accepted exactly two or three positional
  binary directories and ran one pairing per invocation. The two directions were orchestrated by
  `ci.yml` with two calls, not by the script. The new no-argument mode runs both directions for
  each selected tag while keeping the positional form.
- **"Skipped before releases".** The compat CI job was not skipped: it listed the latest stable
  release with `gh`, downloaded its archive, checked its `.sha256` and ran both pairings. What was
  unavailable was a local run that obtains older binaries itself, because an unreleased build has
  no published binary. The job now delegates downloads to the script and covers two preceding
  stable releases.
- **Assets and target.** `release.yml` publishes `lu-<tag>-<target>.tar.gz` plus a `.sha256` file
  generated by `sha256sum` for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`. CI
  previously hardcoded the x86_64 asset. The host mapping uses those two targets, with `--target`
  as an override.
- **"Current one".** The reference is the `[workspace.package].version` in `Cargo.toml`, not a git
  tag on the checkout. Stable tags are `vX.Y.Z`; dev tags are pre-releases and are excluded both by
  the release listing flags and by the numeric tag filter.
- **Temporary files.** The original script already used `mktemp -d` and `umask 077` for its
  scenario work, so it never used a fixed `/tmp` path. That behaviour is preserved. The new cache
  is `target/compat-cache/<target>/<tag>/`, inside the ignored build tree, not the source tree.
- **Seed binaries.** The original script checked the controller's `lu` and the agent's `lu` and
  `ting`, but never checked the optional seed directory. The refactor checks both seed binaries.
- **Checksum handling.** The old workflow ran `sha256sum -c` inside the extraction directory. The
  script now reads the one checksum entry, requires the exact archive basename and a 64-digit
  digest, and compares it with a fresh `sha256sum` before extraction. No path from the downloaded
  checksum file is used.
- **Skipping.** There was no `--skip-download` flag and no silent skip in the old script or job.
  The new flag is the only path that exits 0 without doing compatibility work; an incomplete
  cache or failed verification is an error.

### Verification

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`
  and the same clippy run with `--features tui` passed.
- `cargo test --workspace --locked` passed, as did `cargo test --workspace --locked --features tui`.
  The first workspace run hit a broken-pipe failure in the unrelated `lu --test scope_mcp`
  credential-visibility test; that test passed alone and the full workspace passed on rerun.
- `scripts/check-docs.sh`, `cargo run --manifest-path scripts/check-workflows/Cargo.toml` and
  `scripts/test-compat.sh` passed. `shellcheck` is not installed in this environment.
- An offline end-to-end run with a locally built archive in `target/compat-cache` verified the
  checksum, extracted it, ran both directions, reused the cache on the second run and rejected a
  tampered archive before any binary ran.
- The checkout's `.git` directory is not writable, so no commit was created. The terse message
  prepared for it is `compat-test: fetch and verify previous releases`.

## Node health and process pane verification

### Verified against the code

- There was no podman-specific error path. Process stats failures reach `lu top` as a generic
  `stats unavailable: {error}` line through `Update::ProcessStats`. The plain headline is therefore
  chosen by matching known runtime/container substrings, while the exact error is kept on a dim
  `detail:` line. Other errors keep the original headline.
- `lu nodes` already had a `HEALTH` column and called `node_health_reason`; the reason logic now
  lives in `lu_proto::NodeView::health()` so the CLI and TUI cannot drift. `lu top`'s node table had
  no health or disk data, so it gained a 15-wide `HEALTH` column after `STATE`.
- `GET /v1/nodes` did not expose the configured threshold. `NodeView.min_disk_free_pct` is
  `Option<u8>` with `#[serde(default, skip_serializing_if = "Option::is_none")]`, so an older
  client reads the new answer and an older controller's answer deserializes with `None`. With no
  threshold, disk stays out of `HEALTH`; with `0`, the disk comparison is disabled.
- `harness_ok: false` wins over disk state. Otherwise a percentage below the threshold is critical,
  a percentage below twice the threshold is a warning, and other percentages are hidden. The CLI
  prints plain text; the warning style exists only in `lu top` (`Tone::Warn`) and critical uses
  `Tone::Bad`.
- `/metrics` exposes raw heartbeat signals and has no threshold series, so the health cell uses
  the API field instead of deriving a value from the metrics.

### Verification

- `cargo fmt --all --check` — clean.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean, with and without `--features tui`.
- `cargo test --workspace` — passed, with and without `--features tui`.
- `scripts/check-docs.sh` — passed.

The checkout has no writable git metadata, so changes remain local and uncommitted. Commit messages
were kept terse as requested.

## Disk housekeeping verification

### Verified against the code

- `Housekeeper::run` skips a pass while a job is running and enforces `MIN_INTERVAL` (10 minutes)
  between passes. It is called at agent start and after a finished job, never on the job path.
- `Housekeeper::pass` canonicalises each configured parent and refuses `/`, `/home`, `/var`,
  `/tmp` and `/srv`. It probes the filesystem holding that parent only when `min_free_percent > 0`,
  acts while free space is below the threshold, stops once free space reaches
  `min_free_percent + 5` (capped at 100), and is bounded by `max_remove_per_pass`.
- Candidates are direct children matching the final component of a `scratch_globs` pattern, or
  `target*` children of an `extra_roots` parent. Only real directories whose canonical path is a
  direct child of the canonical parent are considered. Symlinks are never followed, files are
  never removed, and a tree with a failed read is not removable.
- Age is measured from the newest modification time in the whole tree, not the directory entry;
  candidates are removed oldest first. A tree containing `.git` or `Cargo.toml` is kept unless the
  matching rule names build output (`target`, `target-*` or an `extra_roots` child).
- `Registry::protected_paths()` includes every registry state, `Running` and `Deleting` as well as
  kept trees. The agent passes those paths plus the canonical `work_dir` to the housekeeper, so an
  active or draining workspace is protected.
- `extra_roots` entries are absolute, literal paths with no `..` or wildcards, at most four, and
  none may be one of the refused broad roots. Harness-container scratch is reachable only through
  a configured host path; no container command or overlay storage path is used.
- One `Arc<GcCounters>` feeds both idle and busy heartbeat builders. The client emits
  `gc_removed_total` and `gc_bytes_freed_total` only when non-zero; absent fields decode as zero.
  The controller exposes them in `NodeStats`, `NodeView`, `lu node` and the two
  `lu_node_gc_*_total{node}` metrics. In-memory controller views reset on restart, and agent
  counters reset when the agent restarts.

### Where the report or request disagreed with the code

- An earlier claim said `target-*` directories were removed when older than a day. The code default
  is `min_age_hours = 6`, and the docs follow the code rather than that claim.
- `min_free_percent = 0` disables the disk probe and trigger; it does not make a pass ignore the
  `min_age_hours` grace. The docs state both rules.
- The request named upstream `src/gc.rs` and `docs/workspace-gc.md` as references. Neither excerpt
  was available, so the implementation follows the written requirements and the local code.

### Upstream access failure

The upstream agent repository could not be read. Direct `git` and web fetch were denied in this
environment, and an HTTP probe of the Git smart-HTTP endpoint returned `Repository not found`.
No temporary file was treated as upstream source.

## Check/serve validation and operator settings verification

The stale open item asked `lu check` to run `lu serve` validation without binding and to remove an
`[agent]` fallback for operator settings. Both claims were checked against the tree before the
edits.

- **Operator fallback claim.** False in the current code. `app/src/operator.rs` already resolved
  operator settings only from `LU_URL`, `LU_TOKEN_FILE` and `LU_CA_FILE`, or `url`, `token_file` and
  `ca_file` from the operator config; it never read `[agent]` and the ignored `--config` argument
  never supplied credentials. `ting` continues to use `[agent].token_file`, which is a separate
  node scope. The error text now names the settings and the two operator config files, and says
  explicitly that `--config` never supplies operator credentials. A regression test builds both an
  operator config with only `[agent]` credentials and one with an agent token under
  `[controller]`, then asserts each request carries only the configured operator token. No
  `[agent]` fallback was reintroduced.
- **Check/serve claim.** Partly true. `lu check` already validated values without binding a port,
  but it used a separate validation path, so the two commands could disagree. It also deliberately
  opened no secret files or TLS material and never opened the database for write. `lu check` and
  `lu serve` now share `service::validate`, which parses the config and validates tokens, roles,
  schedules, limits, paths and scrub rules with identical messages. `lu serve` then performs the
  startup-only work `lu check` must not do: `lu_config::check_secrets` opens token and credential
  files, TLS material is loaded, the database is opened for write and the listener is bound.
  `lu check` still reports read-only database health. An agreement test feeds equivalent bad values
  to both commands and compares stderr, while asserting `check` creates no database.

### Verification

- `cargo fmt --all` — clean.
- `cargo clippy --workspace --all-targets -- -D warnings` — passed.
- `cargo clippy --workspace --all-targets --features tui -- -D warnings` — passed.
- `cargo test --workspace` — passed.
- `cargo test --workspace --features tui` — passed.
- `scripts/check-docs.sh` — passed (6 docs tests).

Changes remain local and uncommitted: the checkout's git metadata is not writable by the current
uid, so no commit could be created.

- `cargo clippy --workspace --all-targets --locked -- -D warnings` — passed, with and without
  `--features tui`.
- `cargo test --workspace --locked` — passed, with and without `--features tui`.
- `scripts/check-docs.sh` — passed.

The checkout's `.git` directory is root-owned and unwritable, so no commit was created. The terse
change labels prepared are `check: share serve validation` and `operator: reject agent credentials`.

## Idle timeout CLI and batch verification

Claims in the request were checked against the source before editing; the source won where they
disagreed.

- **The flag already existed.** `SubmitArgs.idle_timeout` was present in `app/src/operator.rs` with
  a whole-seconds parser and was already copied to `JobSpec.idle_timeout_secs`. No second flag was
  added. `lu submit --help` now labels both timeout values `DURATION`.
- **Duration syntax was absent on submit.** `lu submit --timeout` was a plain `u64` of seconds;
  only `ting wait-idle --timeout` accepted an `s`/`m`/`h` suffix. Both submit timeout flags now
  share one parser that accepts plain seconds or one suffix, so the request's "same syntax" holds
  after this change. Plain seconds and all existing bounds are unchanged.
- **Only `mcp.rs` passed the field.** `app/src/batch.rs` hard-coded `idle_timeout_secs: None`. It
  now reads `idle_timeout` from `--defaults` and sidecars through the existing overlay rules, with
  the same `1..=86400` check the API uses.
- **`lu show` had no timeout row at all.** Both `timeout_secs` and `idle_timeout_secs` are now
  adjacent rows in `print_job`; absent idle timeouts print `-`. The longest key was already 18
  characters, so alignment of existing rows is unchanged.
- **Batch resolve errors were swallowed.** `plan` reported only `invalid batch settings` even
  though `resolve` built a field-specific message. The reason is now included so an invalid
  `idle_timeout` (or any other shared field) is refused with a clear CLI error.
- **The controller has no cross-field rule.** `JobSpec::validate` checks each timeout only against
  its own range and never compares them. An idle timeout longer than the wall-clock timeout is
  accepted without a warning, matching the API; the wall-clock timeout still ends the job.
  `docs/api.md` now states this. Zero and out-of-range values are refused by the CLI parser and by
  the API with `422 unprocessable`; negative CLI values are refused by the parser before a request.
- **There is no `docs/cli.md` in this repository.** The CLI reference lives in the `README.md`
  command table, with batch fields in `docs/batch.md`; those files and `docs/api.md` were updated.

### Verification

- `cargo fmt --all --check` — clean.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo test --workspace` — passed on rerun. The first run hit a timing failure in the unrelated
  `tuishou-ting` test `job_override_wins_over_the_agent_default`; it passed alone and in the full rerun.
- `scripts/check-docs.sh` — passed.
- The `tui` feature cannot be compiled in this checkout: the pinned local `fw-tui` patch lacks
  `render_table_sorted`, `SortKind`, `TableColumn`, `sort_column_at`, `with_key`, `sorted_indices`,
  `handle_sort_key` and `cycle_sort`. This is independent of the timeout changes.

The checkout's `.git` directory is root-owned and unreadable by the current uid, so no commit could
be created. The prepared terse message is `cli: add idle timeout to batch and show`.

## Node-class infrastructure requeues

### Verified against the code

- `lu_proto::NodeFailureReason` has five fixed wire codes. `GatewayUnavailable` keeps an explicit
  serde rename to `gateway_unavailable`; round-trip tests in the completion route and the store
  conformance suite assert the spelling on the wire.
- `tuishou-ting` recognizes a harness-server connection failure, Podman start/create failures,
  the full overlay-storage wait timeout and a gateway health `503` whose body contains one of the
  operator-configured `[agent] gateway_fault_markers`. Workspace preparation and staging add
  `workspace_enospc` only when a disk-full marker has a preparation context. Bare socket,
  `ENOSPC` and unmarked `503` text stay unclassified.
- `[controller] node_attempts` defaults to `2` and validates `1..=4`. The store computes
  `min(max_retries + 1, node_attempts)`; `max_retries` remains a hard cap on all retry classes.
  Dispatch in the SQLite transaction skips a job's stored exclusions, and `commit_dispatch`
  rechecks them, so a node requeue cannot return to the same node.
- `Store::complete` and `requeue_recovering` exclude the failed node, persist `failure_reason`,
  and keep the reason on a requeued job. `lu retry` clears both. A stored `diff.patch` or a
  cancellation request blocks automatic requeues of any class, not just node failures.
- Audit `Data` is a strict allowlist. Only fixed reason names and node names are written; raw
  harness text is never copied into `reason` or `excluded_node`. A completion records the node as
  actor; recovery uses actor `maintenance`.
- `SCHEMA_VERSION` is `8`. The forward migration adds `jobs.failure_reason` and
  `jobs.excluded_nodes_json`, and a schema-7 probe confirms old rows keep their history.

### Where the docs or request disagreed with the code

- Earlier docs named schema `7` or `4`, and an older CHANGELOG entry named schema `8` while the
  previous baseline was `7`. The current-schema statements now follow `SCHEMA_VERSION`; historical
  release entries are left as history.
- The canonical queued-job explanation is `JobView.waiting_reason`, not `blocked_reason`; the new
  all-nodes-excluded message uses that field.
- A gateway health `503` is a node fault only when its body matches a configured marker. Other
  `503`s are failed health reads and do not stop the job as a node failure.
- The request described a "container wait timeout"; the code matches the full
  `/var/lib/containers/storage/overlay-containers/` path so a generic wait message in task output
  cannot classify a node failure.

### Verification

- `cargo fmt --all` — clean.
- `cargo clippy --workspace --all-targets -- -D warnings` — passed.
- `cargo test --workspace` and `cargo test --workspace --features tui` — passed.
- `scripts/check-docs.sh` — passed.

## Compatibility names removed in 0.8.0

Each former spelling below was checked against the code and removed from runtime paths. The neutral
replacement is now the only wire, config, API, audit, metric and query spelling. The old stored column
name survives only as a migration-detection constant, because schema 1 to 7 databases must still
open. No new setting was added.

- `reiki` — former heartbeat, node-view and poll-request JSON field carrying the opaque
  provider-health document. It is now ignored on input; the field is `provider_health`.
- `reiki` — former cordon `source` value on the API, in audit records and in the
  `lu_node_cordon_seconds_remaining{node,provider,source}` metric. It is now `health`; terminal
  output maps it to a neutral gateway marker.
- `reiki_derived` — former column in the SQLite `cordons` table. The unreleased 7-to-8 step renames
  it to `health_derived`; runtime queries use only the new name.
- `reiki_health_url` and `reiki_token_file` — former `[agent]` config keys. They are now unknown-key
  errors; the accepted keys are `provider_gateway_health_url` and `provider_gateway_token_file`.
- Cordon reason text written by older releases, for example `Reiki cooldown for ...`, stays in
  existing database rows and stored history. New rows use `Provider gateway cooldown for ...`.

The hygiene test's allowlist names only the files that must contain these literals: the scanner
patterns, the migration constant, the runtime rejection tests and this section.

### Verification for this change

- `cargo fmt --all --check`, `make lint`, `make test`, `make docs-check` and the hygiene test — passed.

Changes remain local and uncommitted: the checkout's `.git` directory is not readable by the
current uid, so no commit could be created. Commit messages were kept terse as requested.

# 0.8.0 simplification batch

## Deferred proposals (item 6)

Each item below was checked in the source. None was changed in this batch: every one is a setting
or schema decision the maintainer must make, and this release already carries the breaking changes
enumerated above.

### Operator role `admin`

- What: remove the `admin` operator role. The enum already documents it as identical to `operator`
  (`crates/lu-config/src/lib.rs`, `OperatorRole`), and role enforcement only special-cases
  `viewer` (`crates/lu-controller/src/role.rs`).
- Why: two names for one permission set invite readers to expect a stronger boundary that does not
  exist.
- Risk: configurations that spell the role `admin` would stop parsing, and any future split between
  the two roles would need a new name.
- Migration: in a later release accept the old spelling in config for one deprecation window while
  normalising it to `operator`, then remove it; update the role table and docs in the same release.

### Mirror credentials versus per-repo credential files

- What: choose one place for controller-side repository credentials: either `[mirror.credentials]` or
  `[[controller.repos]].credential_file` (`crates/lu-config/src/lib.rs`, `MirrorConfig` and
  `ControllerRepo`).
- Why: both are validated as private controller-only files but answer the same question, so
  operators must understand two precedence and matching rules.
- Risk: a repository may be matched by name in one table and by URL in the other; consolidating
  them can silently change which file is used for a fetch, and moving secret files has an outage
  window.
- Migration: define explicit precedence, read both spellings for one release, log a deprecation
  warning for the losing form, then remove it only after operators have moved their files. No
  credential value belongs in a config example.

### `[scope] enabled`

- What: remove the master switch and rely on the per-job and agent-side gates (`allow_spawn`,
  `harness_uid`, depth and descendant limits) in `ScopeConfig`.
- Why: the switch is default off (`default_scope_enabled`) and duplicates a second set of denials;
  a single gate is easier to audit.
- Risk: removing it could expose scope routes on nodes that were relying on the switch alone.
- Migration: in a later release keep the default deny through the remaining gates, require an
  explicit opt-in in config, and verify with a route-level test before deleting the key.

### Idempotency table storage

- What: stop storing the full replayed response body in `request_keys` (`crates/lu-store`,
  `RequestKeyReservation` and the baseline table), keeping only status, fingerprint and a bounded
  body or digest.
- Why: the table grows with every replayable mutation until the 24-hour expiry, and the stored body
  duplicates data that can be rebuilt.
- Risk: replay fidelity changes; a digest cannot reproduce the original body, and a shortened body
  could break clients that rely on exact replay.
- Migration: add nullable columns in a future schema step, write both forms for one release,
  backfill on migration, then drop the old column only after a full release cycle. This release
  stays on schema 8.

### Scrub column consolidation

- What: merge the parallel `jobs.scrubbed_hits` and `jobs.patch_scrubbed` columns (and, if the
  maintainer agrees, `jobs.patch_secret_hits_json`) into one structured record.
- Why: three columns must be kept in step on every store mutation; a single value is easier to
  reason about and extends without more `ALTER TABLE` statements.
- Risk: every read and write path touches these columns, including migrations from versions 1 to 7;
  a partial conversion would misreport whether patches were scrubbed.
- Migration: a future schema step writes the structured value while retaining the old columns for
  one release, backfills existing rows, then removes them after compatibility testing. No such
  step is added here.

## Review verdicts and completion

| Item | Verdict | Disposition |
| --- | --- | --- |
| 1. Intake bounds and path/URL validation | True | Done. Canonical limits live with the wire types; config and operator paths use the same absolute/no-`..` and dot-segment rules. |
| 2. Index DDL on database open | True | Done. Index creation or removal runs only for baseline creation and migrations; reopening an existing database performs no index DDL. |
| 3. Same-second ETag ABA | True | Done. A stored row version advances in the mutation transaction, is exposed on `JobView`, and is the `If-Match` validator. `updated_at` stays display-only. |
| 4. Provider-health naming | True | Done. Runtime reads and writes only the neutral config keys, wire field, cordon source and column name; the old column spelling survives only in the schema-7 migration step. Controller-first rollout is documented. |
| 5. Schedule and collect spellings | True | Done. `[[calendar]]`, `[[attach]]`, `ranges` and the four collect modes are the only accepted spellings; every removed alias is refused with the canonical name in the error. |
| 6. Further simplifications | True as proposals | Skipped deliberately. The five proposals above need a maintainer decision on settings or schema; none was changed here. |

Schema version stays `8`. The version column and cordon column rename are folded into the
unreleased 7→8 step; no schema `9` was added and migrations from versions 1–7 remain forward-only.

## Verification

- `cargo fmt --all -- --check` — clean.
- `cargo clippy --workspace --all-targets --locked -- -D warnings` — passed.
- `cargo test --workspace --locked` — passed; 55 test groups reported `ok`, including the new
  alias-refusal, version, migration, index-DDL and bounds cases.
- `scripts/check-docs.sh` — passed; docs examples, links, CLI names and config examples match code.
- Hygiene test — passed; the migration-only spelling, its allowlist rows and this report are the
  only remaining occurrences.
- `scripts/compat-test.sh --previous 2` — could not complete in this environment: release listing
  returned HTTP 403 and there is no cached release directory or tag list, so no previous-release
  binaries were available offline. The script's local scenario was run with the current binaries as
  controller, agent and database seed and passed at `schema_version: 8`. The expected cross-version
  difference is the one documented in [compatibility](docs/compatibility.md): an older agent can
  heartbeat against a newer controller but loses provider health until updated. Schedule and
  collect spellings are refused rather than ignored, so a config or job using them must be updated
  before the controller reads it. No other wire incompatibility was introduced.

Changes remain local and uncommitted: shell writes through the version-control command are denied
and `.git` is root-owned `0700`. Terse per-item commit subjects were prepared but could not be
applied; nothing above should be read as a completed commit.
