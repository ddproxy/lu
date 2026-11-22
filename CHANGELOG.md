# Changelog

All notable changes are recorded here. The format follows Keep a Changelog, and the project
uses semantic versioning once it reaches 1.0.

## [Unreleased]

### Added

- One base per dependency chain. The agent records the commit it staged in `JobView.staged_commit`, also exposed on entries from `GET /v1/agent/jobs/{id}/deps`. A dependent with no pinned `source.base` inherits the `staged_commit` of its dependencies; all must agree, and disagreeing commits stage their newest common ancestor; if a patch then fails to apply, a conflict summary names every contested commit. A dependent's `diff.patch` is relative to the inherited base, and the operator merges the final patch onto the moved branch afterwards. See [docs/dependencies.md](docs/dependencies.md#one-base-per-chain).
- Opt-in agent `[stack]` conflict policies: `union_paths` keeps both sides' added lines in order and base deletions, while `base_paths` takes the base side for generated lock files. Per-job `stack_union_paths`/`stack_base_paths` can only narrow the configured lists; every other path fails closed with class `conflict`. A resolved conflict's summary names each file and hunk, whether the base moved since staging, and the covered policy paths. See [docs/config.md](docs/config.md#stack) and [docs/dependencies.md](docs/dependencies.md#policy-paths-for-known-safe-conflicts).
- `[controller] blocked_dependent_ttl` (default `24h`). A failed or cancelled predecessor leaves blocked dependents waiting with `blocked_reason: "predecessor failed"`, shown in job views (`blocked_until` deadline) and `lu ls`. `lu retry` reattaches waiting dependents, `lu retry --chain` also retries dependents and their references, cancel takes waiting dependents unless `--keep-dependents`, and TTL expiry fails them with class `dependency`. `lu_blocked_dependents_total` and `lu top` show the waiting count. See [docs/operations.md](docs/operations.md#dependency-chains-and-blocked-dependents).

### Changed

- The store has a single baseline at schema version 4, adding `staged_commit`, `blocked_reason` and `blocked_since` to jobs. Databases at versions 1, 2 and 3 migrate in place at startup; backups and restores require exactly version 4. The Ting rename still does not change the schema baseline. Schema references were updated across the operator docs.

## [0.5.0]

Requires: a new controller database (the schema was reset; recreate it before upgrading).

### Changed

- Renamed the node agent to Ting. The crate is now `tuishou-ting` (library) and the binary is `ting`; the heartbeat `agent_version` field is unchanged and now reports the Ting package version. The scoped harness MCP server is `ting mcp`, whose `serverInfo.name` is `ting`. The `[agent]` config table, wire routes, node views and `agent_version` field are unchanged; no schema version bump.
- Renamed the deployment layout for the agent: `contrib/lu-agent.service` to `contrib/ting.service`, `/etc/lu/agent.toml` to `/etc/ting/ting.toml`, the agent token to `/etc/ting/ting.token`, the state and work directory to `/var/lib/ting`, the service account to `ting`, and the optional log target to `/var/lib/ting/ting.log`. The controller keeps its existing `/etc/lu`, `/var/lib/lu` and `lu` account. Environment variables keep `LU_*` for the controller/CLI and `LU_JOB_SOCKET` for scoped jobs; there were no `LU_AGENT_*` variables or `lu_agent_*` metrics to rename. See [docs/migration-ting.md](docs/migration-ting.md).
- Renamed all repository references from `ddproxy/lu` to `ddproxy/tuishou-lu` in badges, security links, release notes and install snippets. Release archives keep their `lu-<tag>-<target>.tar.gz` name and now contain both `lu` and `ting`; fuzz targets, CI and Docker/Makefile packaging were updated to match.
- Updated dependencies: `getrandom` 0.4, `sha2` 0.11, `toml` 1.2, and minor/patch bumps of `clap`, `flate2`, `libc` and `serde_json`.
- `lu_submit_many` now uses the same resolution, chaining and partial-failure helpers as batch submission. Its arguments, validation order and result shape are unchanged.

### Removed

- Removed the `lu-agent` binary and the redundant `lu agent` subcommand. There is no compatibility shim: run `ting` for the agent loop and `ting mcp` for the scoped harness server.
- Removed `JobSpec.stack_dependencies` and the `--no-stack` flag from the CLI and MCP; dependency handling is controlled by `collect`.
- Removed the bare `cordons` name list from node views; `cordon_details` is the only cordon field.
- Collapsed the store migration chain into a single baseline schema version 2. Schema 1 databases gain the scrubbing columns additively; any other schema version is refused with a message telling the operator to create a new database. Backup and restore follow the same rule.
- Removed `docs/stable-fields.md` and its navigation links.

### Added

- Optional controller-side publishing with target redirects and a separately gated source-branch path. `[publish]` declares HTTPS remotes with private token files, branch prefixes, a rolling successful-push budget and named gates; `[publish.redirects]` maps `host/owner/name` source patterns to a remote with exact-then-most-specific matching. A job's publish step is explicit and selects `snapshot` (default) or `source_branch`; target precedence is spec remote, redirect, source that is itself a publish remote, then default-deny. Source-branch pushes require the remote flag, the spec target, the origin allowance and operator approval, use a branch name resolved from the controller mirror rather than payload text, refuse fork heads and protected names, run gates first with no failure override, and use a non-force exact refspec with a CAS check against the job's starting commit. A CAS miss falls back to `snapshot/<key>` when allowed or stops with `no_snapshot_prefix`. `lu publish`, `lu show` target/record lines, a `lu top` note, `publish` audit events and `lu_publish_total{target,result}` expose the result. See [docs/publish.md](docs/publish.md), [docs/config.md](docs/config.md#publish) and the publishing section of [docs/security.md](docs/security.md#publishing).
- Drafts and a human approval boundary. Two job states join the wire enum, making the full list `draft`, `blocked`, `queued`, `leased`, `running`, `recovering`, `needs_input`, `done`, `failed`, `cancelled`, `expired`. Drafts are stored, visible and editable but never dispatched; approval moves `draft` to `queued`, or to `blocked` when dependencies are unmet, rejection cancels, and `[drafts] ttl` (default 7 days) moves an unapproved draft to terminal `expired`. `[drafts] max_drafts` and `max_drafts_per_source` cap drafts overall and per origin, remove the oldest expired drafts first, then refuse a new draft with `429 limit_exceeded` when no room can be made. `lu drafts`, `lu show`, `lu edit <draft>`, `lu approve <draft> [--edit]` and `lu reject <draft> [--reason]`, the operator-only MCP read tool `lu_drafts`, and `GET /v1/drafts`, `PUT /v1/drafts/{id}`, `POST /v1/drafts/{id}/approve` and `POST /v1/drafts/{id}/reject` expose the workflow; job-scoped agent paths cannot see drafts. `lu top` shows the draft count and oldest draft age. See [docs/drafts.md](docs/drafts.md).
- Signed webhook intake, off unless `[intake]` is configured. `[[intake.sources]]` declares `name`, `secret_file` (HMAC-SHA256 key, mode `0600`), `header` (default `X-Hub-Signature-256`), `max_body_bytes`, `rate_per_minute`, `dedupe_header`, `dedupe_ttl_secs`, `allow_plain_http` and `[[intake.sources.rules]]`. Rules match JSON pointers with `equals`, `in`, `prefix`, bounded RE2-style `regex` and `exists`, apply an operator `allow` condition (typically an actor allowlist), resolve `repo`, optional `base` and `ref` from a pointer or literal, keep `task_template` literal unless `unsafe_inline = true` (then only `task_vars` pointers may be substituted), with length caps and stripped control characters. `context_from` copies external data into the separate untrusted context; `auto_approve` defaults to false and `unsafe_inline` defaults to false and is dangerous. `POST /v1/intake/<source>` needs no operator token, verifies only the source signature, returns `202` with a job id and state, `204` for rule drops or duplicates, `429` for rate or draft-cap limits and `400` for invalid deliveries, gives the same `401` for missing and bad signatures, and never logs payload text. Per-source rate limits and a bounded dedupe TTL are documented, as are loopback/TLS termination through a reverse proxy or tunnel. See [docs/intake.md](docs/intake.md).
- Job spec wire object and external context. `source` carries `repo` (https URL), `ref` (branch, tag, commit or `refs/pull/<n>/head|merge`) and optional `base`; new optional fields are `context`, `origin`, `origin_ref`, `untrusted_context` and `draft`. `task` is operator-authored, while `context` is external text capped at 32 KiB with a truncation marker. The agent stages it in `.lu/inputs/context.md` under an **UNTRUSTED EXTERNAL TEXT** header and the prompt instructs the harness to treat it as data. See [docs/api.md](docs/api.md#jobspec).
- Git source hardening. The controller mirror fetches a named ref with restricted exact refspecs, never `+refs/*`, supports pull-request refs, pins the resolved commit on the job, and intake refuses fork heads unless its rule sets `allow_forks = true`. Private credentials stay in controller config `[mirror.credentials]` token files (mode `0600`) and never appear in logs, artifacts, `lu show` or audit records.
- Intake and draft observability: `lu_drafts{state}` and `lu_intake_total{source,result}` with results `accepted`, `dropped`, `bad_signature`, `duplicate`, `rate_limited` and `invalid`. The threat model in [docs/security.md](docs/security.md#intake-and-drafts) spells out what a stranger who can post a comment can and cannot do, and [SECURITY.md](SECURITY.md) points to it.
- Batch submission. `lu submit --batch <dir|file> --key-prefix <prefix>` turns a directory of task files or a Markdown checklist into one job per task. Keys are `<prefix>-<slug>`, so reruns are idempotent. `--defaults <file.toml>` supplies shared fields; sidecars add per-task fields; `--chain` depends each job on all earlier jobs; `--max-parallel N` sets a runtime group cap; `--dry-run` prints the plan without contacting the controller or reading an operator token. All local errors are collected before the first request, and a controller failure partway through prints the submitted rows for a safe retry. MCP gains `lu_submit_batch` with the same planner and submission helpers. No schema change: batch inputs map to existing `JobSpec` fields. See [docs/batch.md](docs/batch.md).
- Runtime group caps. `PUT /v1/groups/{name}/limit` with `{"max_running":N}` sets a cap of 1 to 64 for a configured or previously uncapped group. Unknown body keys are refused, bad names and values are `400`, and the active cap appears in `lu_group_limit{group}`. Caps live in controller memory and return to configured values on restart.
- Added [docs/migration-ting.md](docs/migration-ting.md), a one-page operator note for moving an existing agent host from `lu agent` to `ting`.
- CI-style node workspace lifecycle. `JobSpec.workspace` selects `ephemeral` (the default), `keep_on_failure` or `keep`, and repeatable `JobSpec.cache_dirs` names safe build-cache directories inside the job's tree; `lu submit` and `lu collect` expose `--workspace` and `--cache-dir`. New top-level `[workspace]` config sets the default policy, `failed_ttl` (6h), `keep_ttl` (48h), `max_bytes`, `low_water_percent` (65), `high_water_percent` (80) and `sweep_interval` (60s); unknown keys remain errors. Ephemeral deletion waits for the controller's `/v1/agent/jobs/{id}/workspace/release` confirmation that final artifacts and any active stacking/collect dependants are done, using the stored patch rather than keeping the tree. Kept workspaces expire by TTL or oldest-first disk-pressure eviction; running workspaces are never deleted, symlinks are unlinked rather than followed, paths stay under `[agent] work_dir`, and deletion failures are logged, counted and retried off the job path. Heartbeats and `lu node <name>` report five workspace counters, `/metrics` exports them, `lu top` shows workspace usage and totals, `lu node <name> --workspaces` lists job, state, bytes, age and policy, and `lu node <name> clean [--all|--job <key>]` queues an agent clean request. Agent-local and in-memory controller state needs no store schema bump. See [docs/workspaces.md](docs/workspaces.md).
- Job-scoped capability for harness-initiated follow-up work, off by default (`[scope] enabled = false`). A job submitted with `--allow-spawn` gets a per-job agent socket in a `0700` directory (`0600`, chowned to `[agent] harness_uid` or the agent uid when unset, `SO_PEERCRED` checked, bounded and rate-limited) that dies on terminal state, lease loss, epoch change or cancel; the harness receives only `LU_JOB_SOCKET`. The controller's six `/v1/scope/{route}` actions (`job_context`, `submit_child`, `list_children`, `get_child`, `get_child_artifact`, `cancel_child`) require the agent's token, job id and lease epoch, refuse operator tokens and stale or foreign leases, and return an identical `404` for missing and unrelated jobs. Configurable caps default to 8 children, 32 descendants, depth 2, 64 KiB tasks, 6 spawns/minute and 1 MiB artifact reads, with typed `limit_exceeded` errors naming the limit; summaries are capped at 4 KiB and child artifacts are re-scrubbed on read while `[artifacts] scrub = true` (the default). `ting mcp` serves only those six tools, refuses when operator credentials are visible, and marks child output `source: "child_job"`; `lu mcp` is unchanged. Jobs carry `parent_job_id`/`root_job_id`/`depth` in the single schema baseline with no version bump; operator views gain `lu ls --tree`, parent/root in `lu show`, a `scope:dN` marker in `lu top` and cancel cascades unless `--keep-children`. Scope actions write `scope_action` audit events with actor `job:<id>` and no task text, and export `lu_scope_requests_total{route,result}` and `lu_scope_limit_exceeded_total{limit}`. See [docs/scope.md](docs/scope.md), including container bind-mount and Ansible examples.
- Optional controller-side scrubbing of stored artifacts and logs. `[artifacts] scrub` defaults to `true`; `[[artifacts.extra_rules]]` adds operator regexes with `[scrubbed:<name>]` tags. Built-in generic rules cover private keys, bearer tokens, well-known token prefixes, JWTs and long assignment values. Matches are counted in `JobView.scrubbed_hits`; a changed `diff.patch` sets `patch_scrubbed`, and `lu collect` refuses such an input unless `--allow-scrubbed` is given. Patch scrubbing keeps hunk headers, line counts, markers and line endings. Log appends return the new stored length in `X-Lu-Log-Length` so offset-based uploads stay aligned.
- Optional append-only, hash-chained audit record of job lifecycle events. `[audit] path` (off by default) makes the controller append JSON Lines with a sequence number, UTC time, event, job id, node, actor, a small allow-listed data object (never task text, prompts, artifact content or tokens), `prev` and a SHA-256 `hash`. Events cover submit, lease, start, finish, fail, retry, cancel, cordon set/clear, drain and collect apply. Writes use a bounded channel and never fail a job: enqueue/write errors increment `lu_audit_write_errors_total` in `/metrics` (also `audit_write_errors` in `lu status` and `lu show`) and log an error. New `lu audit verify <path> [--head <hash>]`, `lu audit tail` and `lu audit head` inspect the chain. See [Audit record](docs/audit.md) for the guarantees, their limits and head anchoring.
- Cargo-fuzz harnesses for untrusted-input parsers (binary patch preflight and apply, staging and artifact paths, job specs, TOML config, IDs, controller wire requests, secret scrubbing, intake body decoding and intake rule evaluation), with hand-written seed corpora and a weekly `fuzz.yml` workflow that runs each target for 120 seconds and uploads crashing inputs.

### Fixed

- Stored publish records are persisted in `update_job_tx`, so `lu show` no longer loses a publish attempt after the next job update. `GateFailureMode` now serialises as `hold` and `push_labelled`, matching its parser and documentation.
- `lu restore` now verifies that a snapshot contains every Lu table for its schema version, not just a matching schema version stamp. A file with a forged or stale version but missing `jobs`, `nodes`, `cordons`, `idempotency` or `job_deps` tables is refused before it can replace the live database.
- Input diffs with `GIT binary patch` blocks are bounded before libgit2 inflates them: a declared or actual inflated side above 256 MiB is refused, and malformed or size-mismatched binary data is a staging error even under `skip` or `markers` conflict policies.
- The controller now runs boot recovery before binding its listener and exits on a recovery error instead of logging and serving. This prevents a fresh monotonic clock from making stale leases look unexpired and dispatching the same job twice.
- Artifact uploads recheck the job's node and epoch immediately before the atomic file rename. A lease transfer that happens after the artifact is recorded now returns `409 stale_epoch` instead of overwriting the newer attempt's file.
- Long-poll and log-wait wakeups no longer miss a change that lands between reading state and registering the waiter: notifications store a permit as well as waking already-registered waiters.
- `GET /v1/health` now shares one 5-second, single-flight SQLite probe. Concurrent anonymous checks can no longer turn the unauthenticated endpoint into unbounded blocking database work.
- Repository syncs and base-pack generation are bounded: one mirror sync and four base packs at a time, with per-principal budgets of 6 and 30 per minute. Contention returns `429 rate_limited` with `Retry-After` instead of queueing more git work.
- HTTP connections now have a whole-connection idle/watchdog timeout in addition to the per-request timeout. A client that stops reading a response or goes quiet on a keep-alive connection loses its connection and its slot; an active request keeps the connection alive.
- Artifact and log downloads are streamed rather than read whole into memory. Reads enforce the stored per-file caps and seek straight to `len - tail` for tail requests, so a large log no longer has to be loaded to return its last bytes.
- Queue dispatch now follows submission order even when the wall clock steps back, using the persisted `dispatch_seq` column in baseline schema version 1. Databases with any other schema version are refused; there is no in-place migration.
- Node liveness no longer survives a controller restart. Boot recovery clears persisted monotonic heartbeat stamps, so `online`, the `lu nodes` JSON view and `lu_nodes_online` stay false until each agent heartbeats again instead of trusting a process-local clock stamp from the previous process.
- Staging now applies the same git hardening to every invocation, including `git apply --3way`: system and global config are skipped, hooks disabled, transports restricted, and inherited config, askpass, proxy and repository-location variables scrubbed.
- Tracked `.lu` paths are rejected and `.lu` paths at any depth are stripped from exported diffs, so a nested `.lu` directory cannot smuggle files into or out of a job's patch.
- Artifact and log reads and log appends refuse symlinks instead of following them out of the job directory; reads also reject non-regular files, and log directories are checked for symlinks before opening the log.
- The security policy supports only the latest release and directs private reports to the forge's vulnerability-reporting channel instead of a placeholder email address.

## [0.4.0] - 2026-11-21

### Added

- Expected outputs. `JobSpec.expect` declares relative `files`, `changed` and `forbid_changed` globs, `min_diff_bytes`/`max_diff_bytes` bounds and a `mode` of `suspect` (default) or `fail`. The agent evaluates them after a successful harness against the uploaded diff, excluding `.lu/`, and records unmet expectations in `report.json` under `expect_failures`. Suspect failures keep the job `done` with `outcome_note: expect_failed` and increment `lu_jobs_expect_failed_total`; fail-mode failures end in `failed` with class `execution`. A `no_changes` note takes precedence over `expect_failed`. CLI adds the expect flags to `submit`, `retry` and `edit`, and MCP adds matching arguments. Schema 8 includes the suspect flag.
- Job priority and concurrency groups. `JobSpec.priority` (0 to 9, default 5) dispatches higher values first, then creation time. `JobSpec.group` (1 to 64 characters from `A-Z a-z 0-9 . _ -`) and `[[controller.groups]]` entries with `name` and `max_running` (1 to 64) cap concurrent `leased`, `running`, `recovering` and `needs_input` jobs per group; unconfigured groups are unlimited, and a blocked job stays `queued` with `group limit (<name> <running>/<max>)` as its waiting reason. `lu submit`, `lu retry` and `lu edit` gain `--priority` and `--group`, with `--no-group` on retry and edit; MCP submissions accept `priority` and `group`. `lu show` prints both fields, `lu ls --json` exposes them in `spec`, and metrics add `lu_group_running{group}` and `lu_group_limit{group}`. Provider, label and taint rules still apply first. No schema change: both fields live in the existing JSON spec, with expression indexes created on open.
- Stuck-job policy. `JobSpec.on_stuck` (`none`, `cancel`, `retry`; default from `[agent]`) and `stuck_after_secs` (60–86400, default 1800) let the agent stop a job after the `lu node` output/CPU observation has stayed stuck for the configured interval. `cancel` fails as `execution`; `retry` fails as `node` and follows `max_retries`; `none` is unchanged. The CLI adds `--on-stuck` and `--stuck-after` to `submit`, `retry` and `edit`; MCP adds `on_stuck` and `stuck_after`/`stuck_after_secs` to `lu_submit`, `lu_submit_many` and `lu_retry`. Completions carry optional `stuck_mode`, and `lu_jobs_stuck_stopped_total{mode}` counts stops by mode since process start. No schema change: fields live in the stored spec JSON and in-memory counters.
- Provider cordons now record their source, reason and set time in schema 8. `NodeView.cordon_details` exposes `{provider, source, reason, set_at, until, lifts_after_healthy_heartbeats}`; the old bare `cordons` name list is no longer emitted. Reiki-derived cordons report `source: "reiki"`, a null `until` and a two-heartbeat lift count; failure-derived cordons report `source: "failure"` and a wall-clock expiry.
- `lu node <name>` prints each cordon's source, age, reason and remaining time or lift count. `lu nodes` adds a `CORDONS` column, and `lu top` marks cordoned provider chips with `!` (orange) and shows cordon details under the tables, with Reiki `~` markers in yellow.
- `lu uncordon <node> [<provider>]` and `POST /v1/nodes/{name}/uncordon` clear one provider cordon or every cordon on a node, returning the providers actually removed. The MCP tool `lu_uncordon` does the same and is marked destructive.
- `lu_node_cordon_seconds_remaining{node,provider,source}` exports the remaining seconds of each active cordon; Reiki cordons that lift on healthy heartbeats export `0`.
- `lu top` keyboard and mouse selection for the dependency graph. `Tab`/`Shift+Tab` cycle focus through Nodes, Jobs and Graph with a highlighted border; the Jobs table and graph share one selected job. In Graph focus, `h`/`l` move to a parent or child, `j`/`k` move between siblings and components, `Home`/`End` jump to the ends, and `Enter` opens the job's log. The graph panel's bottom border shows the selected job's full id, state and clipped title, and the footer lists the keys for the focused pane. Left clicks select rows or nodes, double clicks open logs, and the wheel scrolls the focused pane by three rows. Mouse capture is enabled only while the TUI runs and is restored on exit, error and panic.

### Changed

- Cordons created from Reiki retry estimates and fixed provider failures are now distinguishable in APIs, CLI output and the dashboard. Existing cordons upgrade to `source: "failure"` with reason `Cordoned before upgrade` and the upgrade time as their set time. The controller still clears all cordons at boot.
- Dependency graphs are compact horizontal chains, prerequisites first: `lu top` draws them in a
  bordered `Graph` panel with a fixed height share, and `lu graph` keeps clipped task titles.
  Chains share one row; only real fan-outs and fan-ins use `├─▶`/`└─▶` branches and `┐`/`┘` joins.
  The dashboard omits titles, packs unrelated single-job components into shared rows, and moves
  hidden job and component counts into the panel title.

## [0.3.2] - 2026-11-21

## [0.3.1] - 2026-11-20

### Fixed

- Provider cordons no longer outlive a controller restart. Their expiry is on the process's monotonic clock, so a restart made rows from the previous run look hours in the future and blocked dispatch to a healthy provider; boot recovery now clears them and Reiki health and new failures set them again.

### Added

- Dependency input apply modes and conflict policies. `collect = apply` and `both` apply inputs in `after` order through libgit2 apply, libgit2 three-way and `git apply --3way` when git is on the agent's PATH. `JobSpec.on_conflict` selects `fail` (default, ends `conflict` with the input, file and hunk in the summary), `skip` (leaves the patch unapplied and records the first failing file and hunk in `INDEX.md`) or `markers` (labelled conflict markers, `markers (N files)` in the index, later inputs still applied). `INDEX.md` rows include the apply result and conflicted files; markers stay in the exported `diff.patch`, while `.lu` never does. The CLI exposes `--on-conflict` on `submit`, `retry` and `edit`; MCP exposes `on_conflict` on `lu_submit`, `lu_submit_many` and `lu_retry`. Explicit values are echoed in `JobView.spec`. No schema change.
- `lu collect <key-or-id>...` creates one merge, test or review job from the named inputs, with `collect = both`, `on_conflict = markers` and `include_failed` set only when a failed input stored a diff. Built-in `merge`, `test` and `review` prompts live in a `templates` module and are overridden at `$XDG_CONFIG_HOME/lu/templates/<name>.md` (or `$HOME/.config/lu/templates/<name>.md`) read at submit time with a 64 KiB cap. `--task-file` text is appended under `## Extra instructions` within the 256 KiB task cap.

## [0.3.0] - 2026-11-20

### Added

- Dependency input collection. `JobSpec.collect` accepts `none`, `files`, `apply` or `both` (aliases `off`, `false`, `diffs`, `diff`, `patches`, `patch`) and overrides `stack_dependencies`; `files` and `both` stage `INDEX.md`, `diff.patch`, `report.json`, `run.log` and `meta.json` under `.lu/inputs` before the harness starts, with a 16-input/64 MiB cap, private modes, symlink refusal and a 256 KiB log tail. `JobSpec.include_failed` accepts failed or cancelled dependencies, releases their dependents instead of cascading a `dependency` failure, and makes their stored diffs available as inputs. The CLI adds `--collect` and `--include-failed` to `submit`, `retry` and `edit`; MCP adds `collect` and `include_failed` to `lu_submit`, `lu_submit_many` and `lu_retry`. `.lu` is excluded from exported diffs and is a staging error when tracked. No schema change: both fields live in the existing JSON spec.
- Provider-agnostic jobs. `lu submit --provider any` picks a provider from the node's `[[agent.provider_models]]` pairs, tried in configuration order. Agents publish each pair with their heartbeat while Reiki reports that provider healthy; dispatch skips cordoned providers and records the chosen pair as `assigned_provider`/`assigned_model`, which the agent runs. `any` with `--model` or `--fallback` is a validation error. With no healthy pair on any eligible node the job stays `queued` with a waiting reason. Node views expose `provider_models`; no schema change, since pairs live in the stored heartbeat.
- `lu edit <job>` changes provider, model, fallbacks, requirements, tolerations and dependencies of a `queued` or `blocked` job in place through `POST /v1/jobs/{id}/edit` (operator scope). The job keeps its id and dependents; its assignment is cleared. Edited dependencies respect the 16-dependency limit, reject cycles with `400 bad_request` and move the job between `blocked` and `queued`. Other states get `409 conflict`. No schema change.
- `lu mcp` write tools now accept the CLI's scheduling fields: `require`, `fallbacks`, `after_keys`, `no_stack`, `harness`, `timeout`/`timeout_secs` and `idle_timeout`/`idle_timeout_secs` on `lu_submit`, `lu_submit_many` and `lu_retry`. Bounds appear in `tools/list` and are enforced by the shared `JobSpec::validate`, so MCP returns the CLI's validation text.
- `lu_nodes` and the new `lu_node` tool report node labels, version and disk-free stats from each node's stats endpoint.
- Done jobs with an empty stored diff get `outcome_note: "no_changes"` in `JobView`, printed as `no changes` by `lu ls`/`lu show` and marked in the TUI job list. `JobSpec.allow_empty` (`lu submit --allow-empty`) suppresses the note. `lu_jobs_no_changes_total` exports the count since controller start. Schema 6 adds a nullable `diff_bytes` job column; older databases upgrade with it empty, so pre-existing jobs are not noted. Every terminal `JobView` now exposes `diff_bytes` too, so a failed job's partial diff is visible in `lu show` and `lu ls --json` and can be downloaded with `lu fetch`.
- A harness that exits within 3 seconds with a crash banner (`panic(`, `abort() called`, `Segmentation fault`, `Illegal instruction`, `SIGILL`, `core dumped`) is a retryable `node` failure, not a final `execution` failure. The agent publishes `harness_ok=false` as both a node label and a taint on later heartbeats, so dispatch excludes the node from harness jobs until a successful run clears it. No schema change: labels and taints already travel with heartbeats.
- Node taints and job tolerations. Agents advertise up to 8 opaque taints from `[agent].taints`, sent with each heartbeat; jobs tolerate up to 8 with `JobSpec.tolerates`. Dispatch gives a job only to a node whose every taint the job tolerates, composed with label, provider and cordon rules. `lu submit --tolerate` and `lu retry --tolerate`/`--no-tolerate` set them; `lu nodes` shows taints with a `!` prefix and `lu show` prints `tolerates`. A stalled queued job explains `only tainted nodes are free: <taint>`. No schema change: taints live in the stored heartbeat.
- Operator-declared node label probes. Up to 16 `[[agent.detect]]` entries run an argv command at start and every `every_secs` off the heartbeat path, with a wall timeout and a minimal environment; exit 0 publishes its label, any other result removes it. Probe labels join static and file labels under the existing 32-label cap and are marked `probed` in the new additive `label_sources` map on heartbeats, node views and `GET /v1/nodes/{name}/stats`. `lu nodes` and `lu node <name>` show `(probed)`; heartbeats only read the last result, so a slow probe never delays dispatch. No schema change: `label_sources` defaults to empty in stored heartbeats.

### Changed

- `lu_retry` inherits absent fields from the source spec; empty `require` or `fallbacks` clear them and any `after` or `after_keys` replaces the dependencies.
- MCP submissions reserve rate-limit budget only after arguments and the resolved spec validate.
- Mirror sync errors now name the failing step (creating or fetching a mirror) and add a hint for two recognisable causes: a failed stat of a config file under the service account's home directory ("the service account needs a readable HOME") and a failing `git` command ("the controller runs `git` as its own account; it must be able to run git and reach the remote"). The original error text is kept; systemd units should set `Environment=HOME=<state dir>` and a `PATH` that contains `git`.

### Fixed

- A dependency patch that libgit2 cannot prepare for a three-way merge (for example, a hunk whose recorded line number shifted but that `git` accepts by searching nearby lines) is retried with a plain apply and then with `git apply --3way` before being reported as a `conflict`. The failure summary names the file and hunk that failed.
- Artifact uploads from a stale lease no longer overwrite a newer file: the epoch is checked before the body is read and again when the artifact is recorded, and artifact writes use a temporary file plus rename so a failed write leaves no partial file.
- Cancelling a job in `recovering` now ends it in `cancelled` immediately, matching the documented transition and cancel effect.
- Log truncation keeps the `[log truncated]` marker even when the cap leaves less room than the marker itself.
- Draining or undraining an unknown node returns `404 not_found` instead of a success response.
- `lu check` now validates the full config with the same value checks as `lu serve`, reports the checks it ran on a `checks:` line, and still opens no TLS key, token or credential file and binds no port. `lu backup`, `lu restore` and `lu check` read only the `[controller] db` key for database work, so they run as root against a service account's secrets.

### Removed

- Operator commands no longer fall back to `[agent].controller_url` and `[agent].token_file` from `--config`. They use `LU_URL`/`LU_TOKEN_FILE` (optional `LU_CA_FILE`) or an operator file only.
- Failed log writes release the reserved length, so retries append at the right offset.
- Log reads stop at the stored length, even when the file is longer.
- Polls with pending cancellations renew every job held by the polling node.
- Controller start removes leftover `.askpass-*` directories under the mirror root.
- Agent disk readings locate `df` at fixed absolute paths, independent of `PATH`.

## [0.2.0] - 2026-11-18

### Added

- Job fallback choices. `JobSpec.fallbacks` lists up to 4 alternate `{provider, model}` choices. Dispatch applies labels first, then tries the primary `provider`/`model` and each fallback in order, assigning the first provider the node reports healthy and uncordoned; a fallback without a model reuses the primary model. The chosen pair is stored as `assigned_provider`/`assigned_model`, exposed in `JobView` and printed by `lu show`. The `Assign` event carries additive `provider`/`model` fields; older agents ignore them and run the primary choice. A provider failure cordons the assigned pair and requeues the job, so the next dispatch can choose another fallback, node, or provider on the same node. The agent builds the harness command and `report.json` from the assigned model.
- `lu submit --fallback <provider[:model]>` (repeatable, up to 4) adds fallbacks; `lu retry --fallback` replaces the list and `--no-fallbacks` clears it. `--fallback <provider>` without a colon reuses the primary model, while each fallback that names a model keeps its own.
- Node labels and job requirements. Agents advertise up to 32 opaque labels from `[agent].labels` and `[agent].label_files` (re-read before each heartbeat); jobs require up to 8 with `JobSpec.requires` and dispatch gives a job only to a node that has every required label. `lu submit --require` and `lu retry --require`/`--no-require` set them, `lu nodes` shows a `LABELS` column, and `lu show`/`lu top` explain a stalled queued job as `no node with label <label>`. Submitting an unknown label succeeds with a warning. Metrics add `lu_node_labels_info{node,label}`. No schema change: labels live in the stored heartbeat.
- Opt-in agent disk housekeeping. `[agent.housekeeping]` removes old scratch directories matching `scratch_globs` between jobs, oldest first, when the filesystem holding their parent is below `min_free_percent`; `min_age_hours` and `max_remove_per_pass` bound each pass. It never follows symlinks, removes files, escapes a pattern's parent, or touches the agent's `work_dir`. Host stats now report `disk_total_bytes` alongside `disk_free_bytes`, `lu node` prints `disk_free_bytes`, `disk_total_bytes` and `disk_free_percent`, and `lu_node_disk_free_percent{node}` is exported.

- `lu mcp` can submit and control jobs: `lu_submit`, `lu_submit_many`, `lu_result`, `lu_cancel`, `lu_retry`, `lu_drain` and `lu_undrain`, alongside the read tools. Submissions without a key derive `mcp-` plus 16 SHA-256 hex characters from the spec, so repeated identical calls are idempotent. `lu_submit_many` takes up to 12 named jobs and rejects duplicate, unknown, forward or cyclic names before submitting, reporting partial failures.
- `lu_result` returns a job's summary, report, diff stats and a paged diff (24 KiB by default, at most 32 KiB) with `next_offset` and `done`.

### Changed

- `lu mcp` initialize instructions now describe read and write tools. Write tools carry `readOnlyHint: false`; `lu_cancel`, `lu_drain` and `lu_undrain` also carry `destructiveHint: true`.
- One `lu mcp` process allows 30 submissions per rolling minute; over the limit a call returns `isError` with the wait in seconds.

- `lu top`: node `LOAD`, `MEM` and `VERSION` columns with memory thresholds, stuck markers and version-mismatch colouring; a process navigator pane (`x`, refreshed every 2s) showing job liveness counters, the process tree and per-node load/memory sparklines; controller version in the title; and the same new columns in `--once` output.

### Changed

- Schema 5 adds nullable `assigned_provider` and `assigned_model` job columns. Older databases upgrade on open with both empty.

## [0.1.14] - 2026-11-18

### Fixed

- A provider failure cordon created from Reiki's `retry_after_secs` now clears after two consecutive healthy provider heartbeats, so a stale outage estimate no longer leaves jobs queued for hours. Non-healthy readings reset the streak; output-marker cordons still wait out their timer. Store schema version is 3; older databases upgrade with their existing cordons unchanged.
- `lu check`, `lu backup` and `lu restore` read only `[controller] db` and no longer check or load TLS keys and token files, so they work when run as root against a service account's secrets. `lu serve` still validates the whole config and its secret files.

### Added

- `lu submit --after-key <key>` and `lu retry --after-key <key>` resolve a job's idempotency key to a dependency id before submitting. They are repeatable, combinable with `--after`, de-duplicated, and count towards the 16-dependency limit; an unknown key is a usage error (exit 2).
- `GET /v1/jobs?key=<key>` returns the newest job submitted with that key in the usual list envelope (zero or one job, `next: null`); an empty, over-long or malformed key gets `400 bad_request`. If legacy data holds several jobs with one key, the newest wins.

### Changed

- Schema 4 indexes the spec's idempotency key for the new lookup. Older databases gain the index on open.

### Added

- `lu mcp`: a stdio Model Context Protocol server with read-only tools for controller status, jobs, one job's detail, logs, nodes and the dependency graph.
### Added

- A job spec may carry a 40-hex `base_commit`. The controller can serve that commit as a git pack (commit and tree, no parents) at `GET /v1/agent/jobs/{id}/base?epoch=N` for the job's node and epoch. Packs over 256 MiB get `413`.
- `[controller].local_repo_roots` allows absolute local repositories to be served as bases; it is empty by default. `[agent].base_source` is `auto` by default: try the controller then clone on `404`; `controller` fails as a node failure on `404`, and `clone` never asks.
- `lu-staging` can stage from a controller pack with `git2`, without a `git` binary or forge network access. The checkout keeps the same permissions and group sharing as a clone, and `export_diff` is unchanged.

### Added

- Randomised model-based tests for the store: 200 fixed seeds replay 300 operations each against a
  fresh SQLite database, checking lifecycle, dependency, lease, epoch, log and artifact invariants
  after every step. The ignored `model_long` test runs 24 seeds for 3,000 operations each.

### Fixed

- A node holding a `recovering` job is no longer offered another job. `next_assignment` treats
  `recovering` as held, and `lu nodes` shows that job as the node's `current_job`, so one node
  cannot run two jobs after a lost lease.
### Added

- Dependent jobs now stack their dependencies by default: the agent applies each finished
  dependency's `diff.patch` to the dependent's base tree in `after` order before starting the
  harness. A patch that no longer applies is reconstructed from the base and three-way merged;
  if conflicts remain the job fails with the new `conflict` class (never retried), naming the
  dependency and up to 20 conflicting paths. The dependent's diff stays against its base and
  includes the dependency changes, so a consumer merging a chain applies only the last diff.
  `lu submit --no-stack` opts out. `report.json` records the applied dependency ids.

### Changed

- New agent routes `GET /v1/agent/jobs/{id}/deps` and
  `GET /v1/agent/jobs/{id}/deps/{dep}/diff` serve the ordered dependency list and their diffs.
  `JobSpec.stack_dependencies` defaults to `true` and is omitted from JSON when true, so older
  agents keep working against a new controller.
### Added

- Optional `[[controller.repos]]` entries (`url`, `name`, optional `credential_file` and `refs`) make the controller keep a bare mirror under `<artifacts_dir>/../mirrors/<name>.git` and pin each job for that URL to a resolved commit. `POST /v1/repos/{name}/sync`, `GET /v1/repos`, `lu sync <name>` and `lu repos` expose the mirrors; `lu_repo_last_sync_seconds{repo}` reports their age.
- `JobSpec` gains an optional `base_commit` (40 hex characters), serialised only when set. The controller fills it for a configured repository; an unknown ref is `422` naming the ref. The agent ignores it for now.

### Changed

- Deserialising a `JobSpec` now ignores unknown fields, so an older controller accepts a newer client's additive fields. Other request bodies still reject unknown fields.

## [0.1.13] - 2026-11-17

### Changed

- A provider failure no longer uses the retry budget. The job goes back to the queue and waits until a node can serve its provider again, instead of failing after `max_retries` outages. Only 100 provider failures in a row fail a job. Submitting once and letting the queue wait is now the intended way to run through a quota outage.

### Removed

- The in-memory store (`lu_store::memory::MemoryStore`) was removed; it was never used by the
  controller, which opens only the SQLite store. The store logic now exists once, in
  `crates/lu-store/src/sqlite.rs`. The controller API tests' separate fake store was removed too;
  they run against `SqliteStore` opened on a temporary file.

- `[agent].slots`, `[service].shutdown_timeout_secs` and `[[controller.agents]].cordoned` were parsed and validated but never applied, and are now removed; a config still setting them fails to load with an unknown-field error because keys are rejected rather than ignored. Fix: delete those keys (and the `[service]` table) from your config.

## [0.1.12] - 2026-11-17

### Changed

- A harness that cannot start, or a staging failure the node controls (a full or read-only disk, a container runtime error), now ends as a `node` failure and is retried on another node within the retry cap. A missing ref, bad credentials or an unknown repository stay `execution` failures. The table is in `docs/operations.md`.

### Added

- `lu retry <job-id>` resubmits a finished job's spec as a new job. `--provider <name>` and
  `--model <name>` replace those fields, `--after <job-id>` (repeatable) replaces the dependency
  list, and no other field changes. It refuses with exit 2 while the old job is not terminal, the
  new job carries no idempotency key unless `--key` is given, and `--wait` and `--json` behave like
  `lu submit`.

## [0.1.11] - 2026-11-17

### Added

- `lu backup --out <file>` takes a live SQLite snapshot with `VACUUM INTO`, writes it `0600`,
  verifies it with `PRAGMA integrity_check`, and refuses to overwrite unless `--force` is set.
  `lu restore <file>` checks the snapshot and schema, refuses while the controller holds the
  database, moves the current file to `<db>.pre-restore-<unix seconds>`, and installs the copy.
- `lu check` now also reports the controller database's `quick_check` result, schema version,
  size, `leased`/`running` jobs without a lease, and orphan `job_deps` rows when the database
  exists.
- `GET /v1/status` and `lu status` report the controller version, uptime, schema version,
  database bytes, jobs recovered at boot, job counts by state, oldest queued age, node counts
  and lease length. `lu status --json` prints the raw response.
- `lu backup` never writes through a symlink or onto the database or its WAL, creates the copy with mode `0600` from the start and removes a partial file on failure. `lu restore` refuses an empty or foreign SQLite file, serialises concurrent restores, stages the copy beside the database with the old owner and renames it into place, and moves every WAL, SHM and journal file aside with the old database; a failed install puts the old files back.
- The dashboard uses the protocol crate's text sanitiser, which also neutralises the Unicode line and paragraph separators.
- New Prometheus series: `lu_build_info{version}`, `lu_uptime_seconds`, `lu_db_bytes`,
  `lu_schema_version`, `lu_recovered_on_boot`, `lu_queue_oldest_seconds` and
  `lu_node_last_seen_seconds{node}`. `docs/operations.md` includes alert examples.
- Agent heartbeats carry an optional `version` (the crate version, at most 32 printable ASCII
  characters). The node view exposes it and `lu nodes` adds a `VERSION` column.
- `docs/operations.md` includes a systemd timer example for scheduled backups.

### Fixed

- Uploading an artifact with a stale epoch returns `409 stale_epoch` and leaves the newer attempt's artifact file untouched.
- Cancelling a `recovering` job ends it `cancelled` immediately instead of ignoring the request until requeue: its lease is released and its blocked dependents fail as a cancelled dependency.
- Appending a log chunk that must be cut to fit the log cap now reserves room for the `[log truncated]` marker, so the stored log always ends with the marker and never exceeds the cap.
- `lu drain` and `lu undrain` on a node that has never heartbeated fail with `404 not_found` in both stores instead of silently discarding the request; the CLI says which node was refused.

### Changed

- A harness that could not be started, and a staging or workspace-preparation failure that does
  not name the task's repository or ref, now end as a `node` failure and are retried on another
  node within `max_retries`. A staging error naming a missing ref, an authentication failure or
  an unknown repository stays `execution`.

## [0.1.10] - 2026-11-17

### Added

- `GET /metrics` on the controller: Prometheus text exposition of job states, node counts, provider states, and completed-job outcomes.
- Agents report optional host stats (load, memory, swap, uptime) and per-job process-tree stats (PID, parent, state, CPU, RSS, age, redacted command line, open files) in each heartbeat. The combined payload is capped at 16 KiB by dropping processes and then jobs; older heartbeats without the fields are still accepted.
- The controller keeps the latest stats per node in memory, exposes `GET /v1/nodes/{name}/stats`, adds `load1`, `mem_available_kb`, `mem_total_kb` and `stuck` to the node list, and clears a node's stats when its next heartbeat omits them.
- `lu node <name>` prints the host numbers and each running job's process tree as an indented table; `--json` prints the raw stats response.
- Job stats flag `stuck` when the harness has produced no output and the job tree has used no CPU for more than 300 seconds.
### Added

- `lu graph [JOB]` draws the dependency graph as text: components newest first, jobs laid out
  left to right by depth, unknown dependencies as stubs, and at most 6 components and 40 jobs.
  `JOB` narrows it to one component and `--json` prints the shown nodes.
- `lu top` draws the same graph under the job list on terminals at least 24 rows tall, toggled
  with `g`; graph colours and glyphs also apply to the job list.

## [0.1.9] - 2026-11-17

### Added

- `lu submit --idle-timeout <secs>` sets the job's idle-output timeout, so a silent harness is
  stopped after `1` to `86400` seconds; `0` is a usage error.

### Fixed

- A provider failure reports when the provider returns (`Completion.retry_after_secs`, Reiki's estimate plus 60 seconds); the controller cordons the provider on the node for that long, within 5 seconds and 24 hours. An older controller refuses completions that carry the field, so roll controllers first.
- A job that names a provider fails as a provider failure, and moves to another node, as soon as the node's Reiki reports no usable key for that provider on two readings in a row. The signal comes from Reiki, not from harness output.
- A harness that prints a provider-unavailable marker and then goes silent is stopped after 120
  seconds and completes as a `provider` failure, so the controller cordons that provider and
  retries the job elsewhere instead of waiting for the wall-clock timeout.

## [0.1.8] - 2026-11-17

### Added

- `lu top --once` prints one snapshot's node and job tables as plain text and exits, without
  needing a terminal; `lu top --json` prints one object with the controller's `nodes` and `jobs`.
- `lu top` highlights a row selected with the arrows or `j`/`k`; `Enter` opens the selected job's
  last 20 log lines under the tables and `Esc` closes them. Log fetches run on the feeder task.
- `lu top` drains or undrains the highlighted node with `d`, confirmed by a second `d` within
  three seconds; `Tab` switches the selection between the node table and the job list.
- Job dependencies: `JobSpec.after` names up to 16 jobs that must be `done` first. A waiting job is `blocked`, `JobView` exposes `after` and `waiting_on`, `done` dependencies promote dependents in the same transaction, and a `failed` or `cancelled` dependency fails its dependents with `failure_class: "dependency"`. `lu submit --after` sets them, `lu ls` has an `AFTER` column, and `lu show` prints each dependency's state.

## [0.1.7] - 2026-11-17

### Added

- `lu top` draws four sparklines over the last five minutes (running jobs, queued jobs, busy nodes, healthy providers) when the terminal is 24 rows or taller.
- `lu top` shows a title for each job, taken from the first line of its task.
- `/etc/lu/operator.toml` is read after the per-user operator file, for a shared operator host.

## [0.1.6] - 2026-11-16

### Added

- Operator commands, including `lu top`, read `~/.config/lu/operator.toml` (`url`, `token_file`, optional `ca_file`) when no `LU_URL` is set and the `--config` file does not exist, so no shell setup is needed.

## [0.1.5] - 2026-11-16

### Changed

- `lu top` shows the real fleet: nodes with state, last heartbeat and provider health, and the recent jobs, refreshed every two seconds. It was a demo with fake counters.

## [0.1.4] - 2026-11-16

### Fixed

- With `workspace_group` set, the task file is written beside the working tree instead of inside `.git`, so a harness in another account, such as one in a container, can read it.

## [0.1.3] - 2026-11-16

### Added

- `[agent] harness_wrapper`: a command that wraps the harness, such as `podman exec` into a container, so no harness binary runs on the host. `{cwd}` becomes the job's working tree.
- The agent sends its local Reiki's key health with each heartbeat when `reiki_health_url` and `reiki_token_file` are set, so the controller knows which providers a node can use.

### Fixed

- A job cancelled while running now ends as `cancelled`. It stayed `running` because the agent never reported it.

## [0.1.2] - 2026-11-16

### Added

- `[agent] opencode_password_file`: the opencode server's password, read from a private file and passed to the harness as `OPENCODE_SERVER_PASSWORD`. A password-protected server could not be used before.

## [0.1.1] - 2026-11-16

### Added

- `[agent] workspace_group`: gives a group write access to each job's working tree (never `.git`) after the checkout. It lets the agent run as the account that may use git while the harness works as another account, for example an opencode server in a container.

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

- The agent runs the harness as its own user unless `workspace_group` is used with a harness that runs as another account, so by default a harness can read the agent's token file.
  Run the agent as a dedicated account that holds nothing else. Privilege separation is planned.
- Leaf agents use their own credentials to check out private repositories; controller-side
  fetch is planned.
- The agent reports no provider health, so the controller does not yet steer around a cooling
  provider.
- `[service].shutdown_timeout_secs` is validated but the in-flight drain is fixed at 10 seconds.
- `[[controller.agents]].cordoned` is parsed but does not seed the node's drain state.
- Provider credentials stay on the agent/leaf host; there is no controller-side credential
  store.
