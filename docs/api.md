# Lu HTTP API, version 1 (0.1.0)

Controller serves JSON over HTTPS (HTTP only on loopback). Every request carries `Authorization: Bearer <token>`. Tokens are scoped `operator` or `agent`; an agent token is bound to one node name. A token of the wrong scope, a missing token and an unknown token all get the same 401, so a probe cannot tell them apart. A valid agent token acting on another node's job is 403. Bodies are capped (1 MiB, log chunks 64 KiB plus framing). Errors are `{"error":"<code>","message":"<text>"}` with codes `bad_request`, `unauthorized`, `forbidden`, `not_found`, `unprocessable`, `conflict`, `stale_epoch`, `too_large`, `rate_limited`, `internal`. Times are RFC 3339 UTC. Ids are 26-character `JobId`s. Unknown `JobSpec` fields are ignored so an older controller accepts a newer client's additive field; other request bodies reject unknown fields.

Agents poll; there is no push channel in 0.1.0. Lease expiry is measured on the controller's monotonic clock.

## Operator

| Route | Meaning |
|---|---|
| `POST /v1/jobs` body `JobSpec` | Create a job. `201` with `JobView`. `spec.after` lists up to 16 job ids that must be `done` first; the new job starts `blocked` while any are unfinished, otherwise `queued`. `spec.stack_dependencies` defaults to `true`; when true the agent applies each dependency's `diff.patch` to the new job's base tree in `after` order before the harness starts. Set it to `false` to run on the base tree alone. An unknown dependency, one that already failed or was cancelled, more than 16, a duplicate, or any other invalid spec gets `422 unprocessable` naming the fault. Same `idempotency_key` and same spec returns the existing job with `200`; same key and a different spec is `409 conflict`. |
| `POST /v1/jobs` body `JobSpec` | Create a job. `201` with `JobView`. `spec.after` lists up to 16 job ids that must be `done` first; the new job starts `blocked` while any are unfinished, otherwise `queued`. `spec.requires` lists up to 8 labels; dispatch gives the job only to a node that advertises every one, and composes with provider and drain rules. An unknown dependency, one that already failed or was cancelled, more than 16, a duplicate, or any other invalid spec gets `422 unprocessable` naming the fault. Submit accepts a label no online node advertises; the reply's `warnings` names it. Same `idempotency_key` and same spec returns the existing job with `200`; same key and a different spec is `409 conflict`. |
| `GET /v1/jobs?state=&limit=&after=` | List, newest first, `limit` at most 200, `after` is a `JobId` cursor. `{"jobs":[JobView],"next":"<id>|null"}` |
| `GET /v1/jobs?key=<key>` | Look up an idempotency key: the same envelope with zero or one job and `next: null`; other list filters are ignored. An empty, over-long or malformed key is `400 bad_request`. If legacy data holds several jobs with the key, the newest is returned. |
| `GET /v1/jobs/{id}` | `JobView` |
| `POST /v1/jobs/{id}/cancel` | Queued, blocked or recovering: becomes `cancelled` at once. Leased or running: the agent is told on its next poll. Terminal: `409`. Returns `JobView`. |
| `GET /v1/jobs/{id}/logs?offset=N` | Bytes of the stored log from `offset`, `Content-Type: text/plain`, header `X-Lu-Log-Length` and `X-Lu-State`; `?wait=25` holds the request up to 25 seconds for new bytes or a state change. |
| `GET /v1/jobs/{id}/artifacts/{name}` | `name` is `diff.patch`, `run.log` or `report.json`. `404` until stored. |
| `GET /v1/status` | `StatusView`: controller `version`, `uptime_secs`, `schema_version`, `db_bytes`, `recovered_on_boot`, `jobs` counts by state, `oldest_queued_secs`, `nodes`, `nodes_online` and `lease_secs`. |
| `GET /v1/nodes` | `[NodeView]` |
| `GET /v1/nodes/{name}/stats` | The latest `NodeStats` the node reported in its heartbeat. `404` for an unknown node; a known node that has not reported stats is `{}`. In-memory only, so it is lost on controller restart. |
| `POST /v1/nodes/{name}/drain` and `/undrain` | Stop or resume assigning new jobs to a node. `404` for a node that has never heartbeated. |
| `GET /v1/repos` | List the configured repository mirrors: `{"repos":[{"name","url","synced_at","heads"}]}`. `synced_at` is null before the first sync; `heads` maps up to 20 branch names to commit ids. |
| `POST /v1/repos/{name}/sync` | Fetch one configured mirror and return `{"name","heads","synced_at"}`. An unknown configured name is `404`; a fetch failure is `500` with the credential removed from the message. |
| `GET /v1/health` | `{"ok":true,"version":"…","db":"ok"}`, no auth. |
| `GET /metrics` | Prometheus text (`text/plain; version=0.0.4`): `lu_jobs` by state, `lu_nodes`/`lu_nodes_online`/`lu_nodes_drained`, `lu_node_providers{node,provider,state}`, `lu_controller_up`, `lu_jobs_completed_total` by outcome, `lu_build_info{version}`, `lu_uptime_seconds`, `lu_db_bytes`, `lu_schema_version`, `lu_recovered_on_boot`, `lu_queue_oldest_seconds`, `lu_node_last_seen_seconds{node}` and `lu_repo_last_sync_seconds{repo}`. See docs/operations.md. |

`JobSpec`: `repo`, `git_ref`, optional `base_commit`, `task`, `harness`, `provider`, `model`, `timeout_secs`, optional `idle_timeout_secs`, `idempotency_key`, `after`. `base_commit` is the 40-hex commit the controller pinned for a configured repository; it is absent for an unlisted repository and is ignored by agents in this version. A job for a configured repository whose `git_ref` is not a branch, tag or commit in the mirror gets `422` naming the ref.
| `GET /metrics` | Prometheus text (`text/plain; version=0.0.4`): `lu_jobs` by state, `lu_nodes`/`lu_nodes_online`/`lu_nodes_drained`, `lu_node_providers{node,provider,state}`, `lu_node_labels_info{node,label}`, `lu_controller_up`, `lu_jobs_completed_total` by outcome, `lu_build_info{version}`, `lu_uptime_seconds`, `lu_db_bytes`, `lu_schema_version`, `lu_recovered_on_boot`, `lu_queue_oldest_seconds` and `lu_node_last_seen_seconds{node}`. See docs/operations.md. |

`JobView`: `id`, `state`, `spec`, `attempt` (1 based), `node` (or null), `epoch`, `created_at`, `updated_at`, `started_at`, `finished_at`, `failure_class` (or null), `summary`, `exit_code`, `artifacts` (names present), `cancel_requested`, `after` (dependency ids), `waiting_on` (the subset not yet `done`), `waiting_reason` (for a queued job no online node can place, such as `no node with label php8.3`; otherwise null) and `warnings` (submit replies only; otherwise empty/omitted).

`NodeView`: `name`, `version` (the agent's last reported version, or null), `last_seen`, `online` (seen within two lease periods), `draining`, `harness` state, `providers` (list), `labels` (sorted labels from the latest heartbeat), `current_job` (id or null), `cordons` (provider names with an unexpired cordon), `reiki` (opaque JSON from the heartbeat or null), `load1`, `mem_available_kb`, `mem_total_kb`, and `stuck` (the number of the node's running jobs whose latest stats say `stuck`). The four stats fields are `null`/`0` until the node reports a host heartbeat.

`StatusView`: `version`, `uptime_secs`, `schema_version`, `db_bytes` (the SQLite file only), `recovered_on_boot`, `jobs` (an object with every state, zero-filled), `oldest_queued_secs` (`0` when no job is queued or blocked), `nodes`, `nodes_online`, and `lease_secs`.

`NodeStats`: `{"host": HostStats|null, "jobs": [JobStats]}`. Both fields are omitted when empty, so a node that reports neither is `{}`.

`HostStats`: `load1`, `load5`, `load15`, `mem_total_kb`, `mem_available_kb`, `swap_used_kb`, `disk_free_bytes` (optional and currently always omitted), `uptime_secs`. Values are read from `/proc`; non-Linux agents report no host stats.

`JobStats`: `job_id`, `pgid`, `output_idle_secs` (since the harness last wrote output), `cpu_idle_secs` (since any process in the job tree last used CPU), `stuck` (running, with both idle values above 300 seconds), and `processes`. `ProcessStats` entries carry `pid`, `ppid`, `state`, `cpu_secs` (clock ticks divided by 100), `rss_kb`, `age_secs`, `cmdline` (first 80 characters, control characters replaced) and `open_files`. A job tree lists at most the 64 busiest processes, so it is a bounded sample rather than a complete process list.

## Agent

| Route | Meaning |
|---|---|
| `POST /v1/agent/poll` body `{"heartbeat": Heartbeat, "wait_secs": 0..30}` | Registers a heartbeat and asks for work. Returns `204` when there is nothing, or `200` `[AgentEvent]` with `Assign` (when the node is free, not draining, harness healthy, and a queued job matches), `Cancel`, or `Followup`. Holds up to `wait_secs` for work. Assigning creates a lease: `lease_secs` from now, epoch incremented. |
| `POST /v1/agent/jobs/{id}/start` body `{"epoch":N}` | Leased to running. `409 stale_epoch` if the lease moved on. |
| `POST /v1/agent/jobs/{id}/renew` body `{"epoch":N}` | Extends the lease by `lease_secs`. Also implied by `poll`, `log` and `start`. Returns `{"cancel":bool}`. |
| `GET /v1/agent/jobs/{id}/deps?epoch=N` | The job's `after` dependencies in order: `[{"job_id","state","diff_bytes"}]`. `403` unless this node owns the job; stale epoch is `409`. `diff_bytes` is `0` when the dependency has no `diff.patch`, and the agent then skips it. |
| `GET /v1/agent/jobs/{id}/deps/{dep}/diff?epoch=N` | Raw `diff.patch` bytes for a dependency. `403` unless `dep` is in the job's `after` list, `404` when no diff is stored, stale epoch is `409`. |
| `POST /v1/agent/jobs/{id}/log?epoch=N&offset=M` body raw bytes | Append a chunk at `offset`. `offset` must equal the stored length (`409 conflict` with `{"length":L}` otherwise so the agent can resume). At most 64 KiB. When a chunk must be cut to fit the cap, room for the `[log truncated]` marker line is reserved first, so the stored log ends with the marker and never exceeds the cap; the append still returns 200. |
| `PUT /v1/agent/jobs/{id}/artifacts/{name}?epoch=N` body raw bytes | Store `diff.patch` or `report.json` (caps from `limits`). Controller names the file; the agent never supplies a path. |
| `POST /v1/agent/jobs/{id}/complete` body `Completion` | Finish. Outcomes and failure classes follow `lu-proto`. A stale epoch is `409 stale_epoch` and the result is discarded. For a provider failure, optional `retry_after_secs` is when the provider is expected back; the controller cordons the provider on the node for that long (at least 5 seconds, at most 24 hours) instead of the configured length. Two consecutive healthy provider heartbeats clear such a cordon early. |
| `GET /v1/agent/jobs/{id}/base?epoch=N` | Fetch the job's base as a pack when its spec has `base_commit`. Body is `application/x-git-pack`; header `x-lu-base-commit` carries the 40-hex commit id. The pack holds that commit and its tree without parent history. `404` when no commit or base provider serves the job, `413 too_large` when the pack would exceed 256 MiB. |
| `POST /v1/agent/jobs/{id}/complete` body `Completion` | Finish. Outcomes and failure classes follow `lu-proto`. A stale epoch is `409 stale_epoch` and the result is discarded. For a provider failure, optional `retry_after_secs` is when the provider is expected back; the controller cordons the provider on the node for that long (at least 5 seconds, at most 24 hours) instead of the configured length. |

An optional `JobSpec.base_commit` is a 40-hex commit id. Agents use it only with a controller that
can serve that commit; `[agent].base_source` controls whether a `404` falls back to cloning the
spec's `repo` (`auto`, the default) or fails the job (`controller`). `clone` never asks for a pack.

`Heartbeat` may also carry optional `version` (at most 32 printable ASCII characters), optional `labels` (at most 32 labels, each 1–64 characters from letters, digits, `.`, `_`, `-`, `/`, with at most one `=`) plus optional `host` and `jobs` stats. Older agents that omit them are accepted; each heartbeat replaces the controller's previous stats and labels for that node, so omitting them clears a stale job tree and label set. The combined stats are trimmed to 16 KiB before validation by dropping processes first and then whole jobs. Lu never interprets labels; dispatch compares them exactly.

## Controller rules

- A job with `after` dependencies starts `blocked`. When a dependency becomes `done`, every blocked dependent whose dependencies are all `done` becomes `queued` in the same transaction. When a dependency becomes `failed` or `cancelled`, blocked dependents fail with `failure_class: "dependency"` and summary `dependency <id> <state>`, cascading through their own dependents. Dependency failures never retry.
- A dependent job with `stack_dependencies: true` (the default) stages its base commit, then the agent fetches the dependency list and applies each finished dependency's `diff.patch` in `after` order before starting the harness. A dependency with no diff is skipped. If a patch does not apply and a three-way merge leaves conflicts, the agent stops before the harness and completes `Failed(Conflict)`; the summary names the dependency and at most 20 conflicting paths. `conflict` never retries. The job's own `diff.patch` is always against its base and includes the dependency changes, so a consumer merging a chain applies only the last diff.
- Dispatch assigns a queued job only when its node is idle, not draining, has no held job, offers every label in `spec.requires`, and (when the job names a provider) reports that provider healthy with no matching cordon.
- A lease that is not renewed for `lease_secs` expires: the job becomes `recovering`, then after `recovery_grace_secs` returns to `queued` with a bumped attempt (class `node`) unless attempts are exhausted (`max_retries`), when it becomes `failed`.
- `Completion` with a `provider` failure class cordons `(node, provider)` for `provider_cooldown_secs`, or for `retry_after_secs` when given; assignment skips cordoned pairs when the job names a provider, and two healthy heartbeats clear a `retry_after_secs` cordon.
- Epoch fencing: every agent write names the epoch it was assigned; a write for an older epoch is `409 stale_epoch` and changes nothing.
- On start the controller marks every `leased` or `running` job `recovering` (it was restarted mid-flight); agents that still hold the lease resume by renewing within the grace period.
- Job logs and artifacts are stored under `artifacts_dir/<job id>/`, names fixed by the controller, files `0600`, directories `0700`.
