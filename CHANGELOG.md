# Changelog

All notable changes are recorded here. The format follows Keep a Changelog; releases use semantic
versioning.

## [Unreleased]

### Changed

- Breaking: 0.8.0 removes legacy provider-health names, cordon source values and the former stored column name; schema 8 migrates databases 1–7 in place.
- Breaking: 0.8.0 removes legacy schedule tables, calendar `dates` and collect-mode aliases; canonical spellings are required.
- Breaking: Roll the controller first, then agents; 0.7.x agents heartbeat but their provider-health document is ignored until updated.
- Breaking: Job rows carry a version; `If-Match` compares it and stale writes return `412`.
- `lu check` and `lu serve` share value validation; `lu check` opens no secrets, binds no port and never writes the database.
- Operator commands have no `[agent]`-table credential fallback; missing settings name `LU_URL`, `LU_TOKEN_FILE`, optional `LU_CA_FILE`, `url` and `token_file`.
- Provider health uses `[agent] provider_gateway_health_url`, `provider_gateway_token_file`, `provider_health`, `health` and `health_derived`.
- Cordon reasons and provider failures name the gateway; stored older reasons display unchanged.
- Intake enforces priority `0`–`9`, timeout `1`–`86400`, no-`..` paths and dot-segment-free URLs.
- Node-class failures requeue on a different node; lease timeouts exclude nodes and `lu retry` clears exclusions.
- No automatic retry after a stored `diff.patch` or cancellation.

### Added

- `--timeout` and `--idle-timeout` accept seconds or an `s`/`m`/`h` suffix; batch defaults and sidecars accept `idle_timeout`.
- Node-class faults report `harness_unreachable`, `container_start`, `container_wait_timeout`, `gateway_unavailable` or `workspace_enospc`.
- `[agent] gateway_fault_markers` accepts up to 16 markers; an empty list leaves gateway `503` unclassified.
- `[controller] node_attempts` (default `2`, range `1`–`4`) caps node tries at `min(max_retries + 1, node_attempts)`.
- Job views expose `failure_reason` and `excluded_nodes`; exclusions write `node_excluded` audit records.

### Removed

- Provider-gateway integration page.

## [0.7.0]

### Changed

- Breaking: Schema 7 adds `request_keys`; backups and restores require version 7.
- `lu top` has sortable columns via `s`, `S`, `r` and header clicks; `PRI` follows `STATE` and selection follows rows.
- Heartbeats carry `harness_ok` and `disk_free_pct`; dispatch skips unhealthy harnesses or disk below `[schedule] min_disk_free_pct` (default `8`, `0` disables).
- `GET /v1/nodes` accepts `min_disk_free_pct`; `lu nodes` and `lu top` show `harness down` or `disk free N%`.
- Full dev versions appear in binaries, heartbeats, health and status, `x-lu-version`, `/metrics` and MCP server info.

### Added

- Operator writes accept `If-Match`, `Idempotency-Key` with 24-hour replay and `?dry_run=1`; mutation audit events record principal and role.
- Agent housekeeping removes scratch and build directories below `[agent.housekeeping] min_free_percent` (default `15`, `0` disables); `extra_roots` and `gc_*` counters are reported.
- Automatic dev pre-releases carry attestation; stable tags must match `Cargo.toml`.

### Security

- `allow_spawn` is refused when `[agent] harness_uid` is unset; `[agent] allow_shared_uid_scope = true` opts in.

### Fixed

- `lu retry <id>` requeues a terminal job in place, `--chain` restores dependents, and retry applies `restrict_fields`.
- `lu top` shows `harness container not running on this node` when the container is gone.

## [0.6.1]

### Fixed

- `ting` clears staged workspaces left by earlier runs; unmarked directories still refuse staging.

## [0.6.0]

### Changed

- Breaking: Schema 6 adds `jobs.eligible_since`; stop the controller, keep the dated database copy, then start the new binary.
- Agent-to-controller messages ignore unknown fields, and self-drain bodies naming another node are ignored.

### Added

- Operator tokens accept `role`, `expires_at` and `restrict_fields`; viewers are read-only and never receive task text or context.
- Edits, draft edits, group limits, workspace cleans and repository syncs are audited with the operator role.
- Read API sends strong `ETag` and supports `If-None-Match`, `view=summary`, filters and bounded logs.
- `[controller] read_per_minute` (default `1200`) budgets read routes with `429` and `Retry-After`.
- `lu status --json` reports provider health; `lu top` names awaited providers and lists them first.

## [0.5.2]

### Added

- `lu stats` and `GET /v1/stats` report states, failures, retries, busy seconds and duration percentiles.
- `/metrics` adds per-node job, failure, busy-second and runtime/queue series.
- Agent self-drain adds `/v1/agent/drain`, `/v1/agent/undrain`, `/v1/agent/self` and `ting drain`, `ting undrain`, `ting wait-idle`; agent drains cannot clear operator drains.

### Changed

- Tokenless requests use a separate throttle bucket; responses send `Cache-Control: no-store` and `X-Content-Type-Options: nosniff`.
- `lu top` adds normalized repo names, state badges, compact gauges and narrow-terminal shrinking.

### Security

- Audit text fields strip URL userinfo, query and fragment.

### Fixed

- `source.repo` refuses userinfo and redacts stored values.
- Unparseable `limit`, `offset` and `wait` return `400 bad_request`.

## [0.5.1]

### Changed

- Breaking: Schema 4 adds `staged_commit`, `blocked_reason` and `blocked_since`; schema 5 adds scrubbing columns. Backups and restores require the current version.
- Dependencies inherit the staged base; disagreeing commits stage the newest common ancestor and conflicts name contested commits.
- `[stack] union_paths` and `base_paths` cover configured paths; other paths fail closed with class `conflict`.
- `[controller] blocked_dependent_ttl` (default `24h`) leaves dependents waiting; `lu retry --chain` and cancel govern them.
- `[artifacts] scrub_patch` (default `false`) stores diffs verbatim; `lu collect` refuses scrub hits without `--allow-scrubbed`.

### Fixed

- Terminal jobs reject artifact uploads and scrub updates.
- Patch application rejects symlinks, gitlinks, type changes and symlinked ancestors.
- Intake dedupe hashes the signed body; oversized bodies return `401`.
- Draft eviction respects per-source and global caps.

### Security

- Login throttling is keyed by `(peer IP, auth scope)`.
- Workspace caches and scope roots reject symlinks; mirror raw commit OIDs are refused.

## [0.5.0]

### Changed

- Breaking: Requires a new controller database; the schema chain resets to baseline schema 2 and schema 1 databases gain scrubbing columns.
- Breaking: The agent is renamed to Ting: `ting`, `tuishou-ting`, `ting mcp`, with renamed deployment paths, token/state directories and service account.
- Breaking: Repository references move to `ddproxy/tuishou-lu`; archives contain both `lu` and `ting`.
- Breaking: Removes `lu-agent`, `lu agent`, `JobSpec.stack_dependencies`, `--no-stack` and bare `cordons`.

### Added

- Publishing supports redirects, gates, snapshots, approval, CAS checks, `lu publish`, audit and metrics.
- Drafts add `draft` and `expired` states, approval and rejection, TTL and caps, plus CLI, MCP and API routes.
- Signed webhook intake adds sources, rules, templates, untrusted context, dedupe and rate limits.
- Job specs carry `context`, `origin`, `origin_ref`, `untrusted_context` and `draft` with a 32 KiB context cap.
- Mirror fetches use exact refspecs, pin commits and keep credentials in controller config.
- Batch submission adds `--batch`, `--key-prefix`, `--defaults`, sidecars, `--chain`, `--max-parallel` and `--dry-run`.
- Runtime group caps use `PUT /v1/groups/{name}/limit` with `max_running` 1–64.
- Workspaces add policies, cache directories, TTL eviction, counters and clean commands.
- Job-scoped spawn adds a per-job socket, six `/v1/scope` actions, caps, audit events, metrics and `ting mcp` tools.
- Artifact and log scrubbing adds rules, `scrubbed_hits`, `patch_scrubbed` and `--allow-scrubbed`.
- Append-only hash-chained audit adds `lu audit verify`, `lu audit tail`, `lu audit head` and write-error metrics.

### Fixed

- Publish records persist across updates; `GateFailureMode` serialises as `hold` and `push_labelled`.
- `lu restore` verifies tables before replacing the database; stale-epoch uploads return `409 stale_epoch`.
- Recovery errors stop startup before binding; syncs, base packs and connections are bounded and rate-limited.
- Queue dispatch follows submission order; node liveness clears on controller restart.

### Security

- Staging skips system and global git config, scrubs transport variables and rejects `.lu` or symlinked artifact paths.

## [0.4.0] - 2026-11-21

### Changed

- Breaking: Expected-output suspect mode requires schema 8.
- Existing cordons become `source: "failure"`; boot clears cordons.
- Dependency graphs are compact horizontal chains in `lu top` and `lu graph`.

### Added

- Expected outputs declare files, changed and forbidden globs, size bounds and `suspect` or `fail` modes.
- Priority `0`–`9` and groups cap concurrency; CLI, MCP, `lu show`, `lu ls --json` and metrics expose both.
- Stuck-job policy adds `on_stuck` and `stuck_after_secs`.
- Cordons record source, reason and set time; `cordon_details`, `lu uncordon`, API, metrics and TUI expose them.
- `lu top` graph adds keyboard and mouse selection with focused panes.

## [0.3.2] - 2026-11-21

### Changed

- No user-visible changes.

## [0.3.1] - 2026-11-20

### Fixed

- Provider cordons clear at boot; health checks and new failures set them again.

### Added

- Dependency collect modes `apply` and `both` add `on_conflict` `fail`, `skip` and `markers`.
- `lu collect <key-or-id>...` creates merge, test or review jobs with user templates.

## [0.3.0] - 2026-11-20

### Changed

- Breaking: Schema 6 adds nullable `diff_bytes`; older databases gain it empty.
- `lu_retry` inherits absent fields; empty `require` and `fallbacks` clear them.

### Added

- Dependency collection adds `collect` and `include_failed` with `.lu/inputs` staging.
- `--provider any` selects healthy configured pairs; assignments are exposed in job views.
- `lu edit` changes queued or blocked job fields; `lu mcp` accepts scheduling fields.
- Empty diffs get `no_changes`; `--allow-empty` suppresses it.
- Harness crashes within three seconds become retryable `node` failures.
- Taints, tolerations and `[[agent.detect]]` label probes are exposed in views and stats.

### Fixed

- Dependency three-way apply falls back to plain apply and `git apply --3way`.
- Stale-epoch uploads return `409 stale_epoch`; cancelling a `recovering` job ends it `cancelled`.
- Log reads stop at stored length; database-only commands read `[controller] db`.

## [0.2.0] - 2026-11-18

### Changed

- Breaking: Schema 5 adds nullable `assigned_provider` and `assigned_model`; older databases gain them empty.
- `lu mcp` write tools carry hints; destructive tools are marked as such.
- One `lu mcp` process allows 30 submissions per rolling minute.
- `lu top` adds load, memory and version columns, a process pane, stuck markers and controller version.

### Added

- Job fallbacks add up to four `{provider, model}` choices and provider-failure requeues.
- Node labels and requirements add `--require`, `LABELS` and `lu_node_labels_info{node,label}`.
- Agent disk housekeeping adds scratch cleanup and disk stats in `lu node` and `/metrics`.
- `lu mcp` adds idempotent submissions, `lu_submit_many` and paged `lu_result` diffs.

## [0.1.14] - 2026-11-18

### Changed

- Breaking: Cordon retry estimates use schema 3; schema 4 indexes the spec idempotency key.
- Agent dependency routes serve ordered dependencies and diffs; `stack_dependencies` defaults to true.
- Mirrors pin jobs to resolved commits; unknown refs return `422`.

### Added

- `--after-key` and `GET /v1/jobs?key=<key>` resolve jobs by idempotency key.
- `lu mcp` provides stdio read tools for status, jobs, logs, nodes and the graph.
- `base_commit` adds controller base packs with a 256 MiB cap and `base_source` modes.
- Dependents stack dependencies by default with three-way merge; `--no-stack` opts out.

### Fixed

- Provider cordons from retry estimates clear after two healthy heartbeats.
- `lu check`, `lu backup` and `lu restore` read only `[controller] db`.
- A node holding a `recovering` job is not offered another job.

## [0.1.13] - 2026-11-17

### Changed

- Provider failures requeue without consuming retry budget; 100 consecutive failures fail a job.

### Removed

- Breaking: Remove `[agent].slots`, `[service].shutdown_timeout_secs` and `[[controller.agents]].cordoned`.

## [0.1.12] - 2026-11-17

### Changed

- Harness start and node-control staging failures end as `node` failures and retry elsewhere; missing refs, bad credentials and unknown repositories stay `execution`.

### Added

- `lu retry <job-id>` resubmits a finished spec with `--provider`, `--model`, `--after`, `--key`, `--wait` and `--json`.

## [0.1.11] - 2026-11-17

### Added

- `lu backup --out` and `lu restore` verify snapshots, use mode `0600` and move old databases aside.
- `lu check` reports `quick_check`, schema version, size, unleased jobs and orphan dependency rows.
- `/v1/status` and `lu status` report version, uptime, schema, database size, recovery, counts and ages.
- Prometheus adds build, uptime, database, schema, recovery, queue-age and last-seen series.
- Heartbeats carry `version`; node views and `lu nodes` show it.

### Fixed

- Stale-epoch uploads return `409 stale_epoch`; cancelling a `recovering` job ends it `cancelled`.
- Log truncation preserves the marker; drain and undrain on unknown nodes return `404 not_found`.

## [0.1.10] - 2026-11-17

### Added

- `/metrics` exposes job states, node counts, provider states and completed outcomes.
- Host and process stats ride heartbeats; `/v1/nodes/{name}/stats` and `lu node <name>` expose them.
- `lu graph` draws text dependency graphs; `lu top` shows a graph panel toggled with `g`.

## [0.1.9] - 2026-11-17

### Changed

- Breaking: Roll controllers first; completion `retry_after_secs` is refused by older controllers.

### Added

- `lu submit --idle-timeout <secs>` accepts `1`–`86400`; `0` is a usage error.

### Fixed

- Provider failures report return time; cordons last 5 seconds to 24 hours.
- Two gateway readings with no usable key move a provider-named job to another node.
- Provider-unavailable marker plus silence ends as a provider failure after 120 seconds.

## [0.1.8] - 2026-11-17

### Added

- `lu top --once` prints plain tables; `--json` prints one controller snapshot.
- `lu top` row selection opens logs, drains or undrains with `d`, and switches panes with `Tab`.
- Job dependencies add `after` up to 16, `blocked` state, `waiting_on`, `AFTER` column and states in `lu show`.

## [0.1.7] - 2026-11-17

### Added

- `lu top` adds five-minute sparklines and job titles from task first lines.
- `/etc/lu/operator.toml` is read after the per-user operator file.

## [0.1.6] - 2026-11-16

### Changed

- Operator commands read `~/.config/lu/operator.toml` when `LU_URL` is unset and `--config` is absent.

## [0.1.5] - 2026-11-16

### Changed

- `lu top` shows live nodes and jobs refreshed every two seconds.

## [0.1.4] - 2026-11-16

### Fixed

- With `workspace_group`, the task file is written beside the working tree.

## [0.1.3] - 2026-11-16

### Added

- `[agent] harness_wrapper` wraps the harness with `{cwd}` expanded to the job tree.
- Heartbeats carry provider gateway key health.

### Fixed

- Cancelling a running job ends it `cancelled`.

## [0.1.2] - 2026-11-16

### Added

- `[agent] opencode_password_file` passes the server password as `OPENCODE_SERVER_PASSWORD`.

## [0.1.1] - 2026-11-16

### Added

- `[agent] workspace_group` grants a group write access to each job working tree.

## [0.1.0] - 2026-11-16

First packaging release.

### Added

- One `lu` binary with controller, operator CLI and optional terminal dashboard (`--features tui`).
- HTTP(S) API over SQLite with submission, idempotency, listing, logs, artifacts, cancel, nodes, drains and cordons.
- Bearer tokens with operator and agent scopes, SHA-256 hashes, throttling, caps and request limits.
- Lease assignment with epoch fencing, boot recovery, retries and `max_retries`.
- CLI commands: `lu check`, `lu serve`, `lu submit`, `lu ls`, `lu show`, `lu logs`, `lu fetch`, `lu cancel`, `lu nodes`, `lu drain`, `lu undrain`.
- Validated TOML, private-file checks, loopback-only plain HTTP and TLS for non-loopback listeners.
- Harness supervision, per-job staging, diffs, artifact limits and cleanup.
- systemd units, sample configs and tagged release builds with SHA-256 checksums.
