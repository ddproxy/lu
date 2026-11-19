# Lu HTTP API, version 1

The controller serves JSON over HTTPS; plain HTTP is accepted only on loopback. Routes use `Authorization: Bearer <token>` except `GET /v1/health`. Tokens have `operator` or `agent` scope; an agent token is bound to one node name. Missing, unknown and wrong-scope tokens all get `401 unauthorized` with `WWW-Authenticate: Bearer`, so a probe cannot tell them apart. A valid agent token used for another node's job or heartbeat gets `403 forbidden`. JSON bodies are capped at 1 MiB, log chunks at 64 KiB plus framing, diffs at 10 MiB plus framing and reports at 1 MiB plus framing. Errors are `{"error":"<code>","message":"<text>"}`. Codes are `bad_request`, `unauthorized`, `forbidden`, `not_found`, `unprocessable`, `conflict`, `stale_epoch`, `too_large`, `rate_limited` and `internal`. Times in JSON are integer Unix seconds. Job ids are 26-character Crockford base32 strings. Unknown `JobSpec` fields are ignored; other request bodies reject unknown fields.

Agents poll; the controller has no push channel. Lease expiry uses the controller's monotonic clock.

## Operator

| Route | Meaning |
|---|---|
| `POST /v1/jobs` body `JobSpec` | Create a job. `201` with a `JobView` and a `warnings` array. `spec.after` holds at most 16 job ids; an unknown, failed, cancelled or duplicate dependency or any other invalid spec is `422 unprocessable` and names the fault. `spec.stack_dependencies` defaults to `true`, so the agent applies each dependency's `diff.patch` in `after` order before the harness starts. `spec.requires` holds at most 8 labels; an unknown label is accepted with a warning in the reply. A repeated `idempotency_key` with the same spec returns the existing job with `200`; the same key with a different spec is `409 conflict`. |
| `GET /v1/jobs?state=&limit=&after=` | List jobs, newest first. `limit` defaults to 200 and is capped at 200. `after` is a `JobId` cursor; an invalid value is `400 bad_request`. Returns `{"jobs":[JobView],"next":"<id>|null"}`. |
| `GET /v1/jobs?key=<key>` | Look up an idempotency key. Returns the same envelope with zero or one job and `next: null`; other filters are ignored. An empty, over-long or malformed key is `400 bad_request`. |
| `GET /v1/jobs/{id}` | Return a `JobView`. An unknown id is `404 not_found`. |
| `POST /v1/jobs/{id}/cancel` | `blocked`, `queued` and `recovering` jobs become `cancelled` at once; `leased`, `running` and `needs_input` jobs get `cancel_requested = true` and the agent is told on its next poll. A terminal job is `409 conflict`. Returns a `JobView`. |
| `GET /v1/jobs/{id}/logs?offset=N&wait=25` | Return raw log bytes from `offset` with `Content-Type: text/plain`, `X-Lu-Log-Length` and `X-Lu-State`. `wait` holds the request for up to 25 seconds while no new bytes arrive. An unknown id is `404`. |
| `GET /v1/jobs/{id}/artifacts/{name}` | Return one fixed artifact name: `diff.patch`, `run.log` or `report.json`. An unknown name or an artifact that is not stored is `404`. |
| `GET /v1/status` | Return a `StatusView`. |
| `GET /v1/repos` | Return `{"repos":[{"name","url","synced_at","heads"}]}`. `synced_at` is null before the first sync; `heads` maps up to 20 branch names to commit ids. |
| `POST /v1/repos/{name}/sync` | Fetch one configured mirror and return `{"name","heads","synced_at"}`. An unknown configured name is `404`; a fetch failure is `500` with any credential removed from the message. |
| `GET /v1/nodes` | Return an array of `NodeView`s. |
| `GET /v1/nodes/{name}/stats` | Return the latest `NodeStats` from the node's heartbeat. An unknown node is `404`; a known node with no stats returns `{}`. Stats live in memory and are lost on controller restart. |
| `POST /v1/nodes/{name}/drain` | Stop assigning new jobs to the node. A node that has never heartbeated is `404`. |
| `POST /v1/nodes/{name}/undrain` | Let the node take new jobs again. A node that has never heartbeated is `404`. |
| `GET /metrics` | Return Prometheus text with `Content-Type: text/plain; version=0.0.4; charset=utf-8`. It needs an operator token. Series are listed in [operations.md](operations.md#metrics). |
| `GET /v1/health` | Return `{"ok":true,"version":"…","db":"ok"}` without authentication. |

## JSON shapes

### `JobSpec`

| Field | Type | Required | Rule |
| --- | --- | --- | --- |
| `repo` | string | yes | Git URL or absolute local path. |
| `git_ref` | string | yes | Branch, tag or commit to check out. |
| `base_commit` | string | no | 40-hex commit id. The controller pins it for a configured repository; the agent uses it as the base tree. Omitted when absent. |
| `task` | string | yes | Up to 256 KiB. |
| `harness` | string | no | Defaults to `opencode`. |
| `provider` | string | no | Omitted when absent. |
| `model` | string | no | Omitted when absent. |
| `timeout_secs` | integer | no | 1 to 86400; defaults to 3600. |
| `idle_timeout_secs` | integer | no | 1 to 86400; omitted when absent. |
| `idempotency_key` | string | no | Omitted when absent. |
| `after` | array of job ids | no | At most 16 unique ids; omitted when empty. |
| `stack_dependencies` | boolean | no | Defaults to `true`; omitted when `true`. When true, dependency diffs are applied to the job's base tree. |
| `requires` | array of labels | no | At most 8 labels; omitted when empty. |
| `fallbacks` | array of objects | Up to 4 alternative `{provider, model}` choices, tried in order when the primary provider is unavailable on a node. Omitted when empty. |
| `allow_empty` | boolean | No | Defaults to `false`; omitted when `false`. When true, a successful job with no diff gets no `outcome_note`. |

An unknown `git_ref` for a configured repository is `422 unprocessable` and names the ref. A job for an unlisted repository has no `base_commit` unless one is supplied by the client.

### `JobView`

| Field | Type | Meaning |
| --- | --- | --- |
| `id` | string | Job id. |
| `state` | string | One of `blocked`, `queued`, `leased`, `running`, `recovering`, `needs_input`, `done`, `failed`, `cancelled`. |
| `spec` | `JobSpec` | The submitted spec. |
| `attempt` | integer | 1-based attempt number. |
| `node` | string or null | Node holding the job. |
| `epoch` | integer | Lease epoch used for fencing. |
| `created_at` | integer | Unix seconds when the job was created. |
| `updated_at` | integer | Unix seconds of the last state change. |
| `started_at` | integer or null | Unix seconds when the harness started. |
| `finished_at` | integer or null | Unix seconds when the job reached a terminal state. |
| `failure_class` | string or null | `node`, `provider`, `execution`, `dependency` or `conflict`. |
| `summary` | string or null | Last failure or completion summary. |
| `exit_code` | integer or null | Harness exit code when known. |
| `artifacts` | array of strings | Fixed names currently stored: `diff.patch`, `run.log`, `report.json`. |
| `cancel_requested` | boolean | Whether cancellation has been requested. |
| `after` | array of strings | Dependency ids from the spec. |
| `waiting_on` | array of strings | Dependencies that are not yet `done`. |
| `waiting_reason` | string or null | Why a queued job cannot be placed, such as `no node with label php8.3`. |
| `assigned_provider` | string or null | The provider choice the job was assigned with, when it has fallbacks. |
| `assigned_model` | string or null | The model of that choice. |
| `outcome_note` | string or null | Omitted when absent. `no_changes` when the job is `done` and its stored diff is empty and `allow_empty` is false. |
| `warnings` | array of strings | Submit replies only. |
`HostStats`: `load1`, `load5`, `load15`, `mem_total_kb`, `mem_available_kb`, `swap_used_kb`, `disk_free_bytes` and `disk_total_bytes` (both optional; they describe the agent's work filesystem and are omitted when it cannot be read), `uptime_secs`. Host numbers are read from `/proc`; non-Linux agents report no host stats.

### `NodeView`

| Field | Type | Meaning |
| --- | --- | --- |
| `name` | string | Node name bound to the agent token. |
| `version` | string or null | Agent version from its latest heartbeat. |
| `last_seen` | integer | Unix seconds of the latest heartbeat. |
| `online` | boolean | Seen within two lease periods. |
| `draining` | boolean | Whether new assignment is stopped. |
| `harness` | string | Last reported harness state: `idle` or `busy`. |
| `providers` | array | Provider names and states from the latest heartbeat. |
| `labels` | array of strings | Labels from the latest heartbeat. |
| `current_job` | string or null | Job id the node holds. |
| `cordons` | array | Providers with an unexpired cordon on this node. |
| `reiki` | object or null | Opaque Reiki document from the heartbeat. |
| `load1` | number or null | One-minute load from the latest host stats. |
| `mem_available_kb` | integer or null | Available memory from the latest host stats. |
| `mem_total_kb` | integer or null | Total memory from the latest host stats. |
| `stuck` | integer | Number of the node's running jobs whose latest stats say `stuck`. |

### `StatusView`

| Field | Type | Meaning |
| --- | --- | --- |
| `version` | string | Controller build version. |
| `uptime_secs` | integer | Seconds since the controller started. |
| `schema_version` | integer | Schema version the binary expects. |
| `db_bytes` | integer | Size of the SQLite file, excluding `-wal` and `-shm`. |
| `recovered_on_boot` | integer | Jobs moved to `recovering` by startup recovery. |
| `jobs` | object | Counts for all nine states, zero-filled. |
| `oldest_queued_secs` | integer | Age of the oldest `queued` or `blocked` job, or `0`. |
| `nodes` | integer | Registered nodes. |
| `nodes_online` | integer | Nodes seen within two lease periods. |
| `lease_secs` | integer | Configured lease duration. |

### Repository shapes

`GET /v1/repos` returns `{"repos":[RepoInfo]}`. `POST /v1/repos/{name}/sync` returns a `SyncResult`.

| Shape | Fields |
| --- | --- |
| `RepoInfo` | `name`, `url`, `synced_at` (integer or null), `heads` (up to 20 branch names mapped to commit ids). |
| `SyncResult` | `name`, `heads`, `synced_at` (integer). |

### Node stats

`GET /v1/nodes/{name}/stats` returns `NodeStats`. Both fields are omitted when empty, so a node that reports neither is `{}`.

| Shape | Fields |
| --- | --- |
| `NodeStats` | `host` (`HostStats` or null), `jobs` (array of `JobStats`). |
| `HostStats` | `load1`, `load5`, `load15`, `mem_total_kb`, `mem_available_kb`, `swap_used_kb`, `disk_free_bytes` (reserved and currently omitted), `uptime_secs`. Read from `/proc`; non-Linux agents report no host stats. |
| `JobStats` | `job_id`, `pgid`, `output_idle_secs`, `cpu_idle_secs`, `stuck`, `processes` (array of `ProcessStats`, at most 64 busiest entries). |
| `ProcessStats` | `pid`, `ppid`, `state`, `cpu_secs`, `rss_kb`, `age_secs`, `cmdline` (first 80 characters with control characters replaced), `open_files`. |

## Agent

| Route | Meaning |
|---|---|
| `POST /v1/agent/poll` body `{"heartbeat": Heartbeat, "wait_secs": 0..30}` | Register a heartbeat and ask for work. `wait_secs` above 30 is `400`. Returns `204` when there is no work, or `200` with an array of events: `{"kind":"cancel","job_id":…,"epoch":…}` and `{"kind":"assign","job_id":…,"epoch":…,"spec":JobSpec}`. An `assign` creates a lease for `lease_secs` and increments the epoch. A heartbeat whose node differs from the token's node is `403`. |
| `GET /v1/agent/jobs/{id}/base?epoch=N` | Fetch the job's base pack when the spec has `base_commit`. Returns `application/x-git-pack` with header `x-lu-base-commit`; the pack holds that commit and its tree without parent history. Missing commit, wrong node, unavailable base or stale epoch gives `403`, `404` or `409` as appropriate; a pack over 256 MiB gives `413 too_large`. |
| `POST /v1/agent/jobs/{id}/start` body `{"epoch":N}` | Move a leased job to `running` and return its `JobView`. A wrong node is `403`; a stale epoch is `409 stale_epoch`. The lease is extended. |
| `POST /v1/agent/jobs/{id}/renew` body `{"epoch":N}` | Extend the lease by `lease_secs` and return `{"cancel":bool}`. Log append and `start` also extend the lease; `poll` renews jobs that have pending cancellations. |
| `POST /v1/agent/jobs/{id}/log?epoch=N&offset=M` body raw bytes | Append a chunk at `offset`. The offset must equal the stored length; otherwise `409 conflict` returns `{"length":L}`. The body is limited to 64 KiB plus framing. When the 50 MiB log cap is reached, room for the `[log truncated]` marker is reserved first so the stored log ends with that marker. |
| `PUT /v1/agent/jobs/{id}/artifacts/diff.patch?epoch=N` body raw bytes | Store the diff, capped at 10 MiB. The controller chooses the path and returns `200` or `413 too_large`. |
| `PUT /v1/agent/jobs/{id}/artifacts/report.json?epoch=N` body raw bytes | Store the report, capped at 1 MiB. The controller chooses the path and returns `200` or `413 too_large`. |
| `GET /v1/agent/jobs/{id}/deps?epoch=N` | Return the job's dependencies in `after` order as `[{"job_id","state","diff_bytes"}]`. `diff_bytes` is `0` when no diff is stored. Wrong node is `403`; stale epoch is `409 stale_epoch`. |
| `GET /v1/agent/jobs/{id}/deps/{dep}/diff?epoch=N` | Return the dependency's raw `diff.patch`. A `dep` outside the job's `after` list is `403`; a missing diff is `404`; a stale epoch is `409 stale_epoch`. |
| `POST /v1/agent/jobs/{id}/complete` body `Completion` | Finish the job and return its `JobView`. A stale epoch is `409 stale_epoch` and the result is discarded. For a provider failure, `retry_after_secs` sets the cordon length on the node, clamped to 5 seconds through 24 hours. Two healthy provider heartbeats clear such a cordon early. |

A `JobSpec.base_commit` is a 40-hex commit id. The agent fetches a pack for it unless `base_source` is `clone`. With `controller`, a `404` fails the job; with `auto`, the default, a `404` falls back to cloning the spec's `repo`.

`Heartbeat` may also carry optional `version` (at most 32 printable ASCII characters), optional `labels` (at most 32 labels, each 1–64 characters from letters, digits, `.`, `_`, `-`, `/`, with at most one `=`) plus optional `host` and `jobs` stats. Older agents that omit them are accepted; each heartbeat replaces the controller's previous stats and labels for that node, so omitting them clears a stale job tree and label set. The combined stats are trimmed to 16 KiB before validation by dropping processes first and then whole jobs. Lu never interprets labels; dispatch compares them exactly.

## Controller rules

- A job with `after` dependencies starts `blocked`. When a dependency becomes `done`, every blocked dependent whose dependencies are all `done` becomes `queued` in the same transaction. When a dependency becomes `failed` or `cancelled`, blocked dependents fail with `failure_class: "dependency"` and summary `dependency <id> <state>`, cascading through their own dependents. Dependency failures never retry.
- A dependent job with `stack_dependencies: true` (the default) stages its base commit, then the agent fetches the dependency list and applies each finished dependency's `diff.patch` in `after` order before starting the harness. A dependency with no diff is skipped. If a patch does not apply and a three-way merge leaves conflicts, the agent stops before the harness and completes `Failed(Conflict)`; the summary names the dependency and at most 20 conflicting paths. `conflict` never retries. The job's own `diff.patch` is always against its base and includes the dependency changes, so a consumer merging a chain applies only the last diff.
- Dispatch first applies labels: it assigns a queued job only when its node is idle, not draining, has no held job, and offers every label in `spec.requires`. A job with no `spec.provider` then runs on that node. Otherwise the choices are tried in order—the primary `spec.provider`/`spec.model`, then `spec.fallbacks` in list order—and the job uses the first choice whose provider the node reports healthy and has no unexpired cordon for. The chosen pair is stored as `assigned_provider`/`assigned_model`; a fallback without a model uses the primary `spec.model`. A requeued job is dispatched afresh and may choose another pair, including on the same node.
- A lease that is not renewed for `lease_secs` expires: the job becomes `recovering`, then after `recovery_grace_secs` returns to `queued` with a bumped attempt (class `node`) unless attempts are exhausted (`max_retries`), when it becomes `failed`.
- `Completion` with a `provider` failure class cordons `(node, assigned_provider)` for `provider_cooldown_secs`, or for `retry_after_secs` when given; the pair is the one chosen at dispatch, not necessarily the primary. Assignment skips a cordoned choice, so the requeued job can move to its next fallback, another node, or the same node under a different provider. Two healthy heartbeats clear a `retry_after_secs` cordon.
- Epoch fencing: every agent write names the epoch it was assigned; a write for an older epoch is `409 stale_epoch` and changes nothing.
- On start the controller marks every `leased` or `running` job `recovering` (it was restarted mid-flight); agents that still hold the lease resume by renewing within the grace period.
- Job logs and artifacts are stored under `artifacts_dir/<job id>/`, names fixed by the controller, files `0600`, directories `0700`.
