# Lu HTTP API, version 1

The controller serves JSON over HTTPS; plain HTTP is accepted only on loopback. Routes use `Authorization: Bearer <token>` except `GET /v1/health`. Tokens have `operator` or `agent` scope; an agent token is bound to one node name. Missing, unknown and wrong-scope tokens all get `401 unauthorized` with `WWW-Authenticate: Bearer`, so a probe cannot tell them apart. A valid agent token used for another node's job or heartbeat gets `403 forbidden`. JSON bodies are capped at 1 MiB, log chunks at 64 KiB plus framing, diffs at 10 MiB plus framing and reports at 1 MiB plus framing. Errors are `{"error":"<code>","message":"<text>"}` with an optional `length` field (for example, a log-offset conflict). Codes are `bad_request`, `unauthorized`, `forbidden`, `not_found`, `unprocessable`, `conflict`, `stale_epoch`, `too_large`, `rate_limited` and `internal`. Times in JSON are integer Unix seconds. Job ids are 26-character Crockford base32 strings. Unknown `JobSpec` fields are ignored; other request bodies reject unknown fields.

Agents poll; the controller has no push channel. Lease expiry uses the controller's monotonic clock.

Operator clients send an operator token from `LU_TOKEN_FILE` or an operator file (`$XDG_CONFIG_HOME/lu/operator.toml`, `~/.config/lu/operator.toml` or `/etc/lu/operator.toml`). An agent token on an operator route gets `401`; `--config` is not an operator credential source.

## Operator

| Route | Meaning |
|---|---|
| `POST /v1/jobs` body `JobSpec` | Create a job. `201` with a `JobView` and a `warnings` array. `spec.after` holds at most 16 job ids; an unknown, failed, cancelled or duplicate dependency or any other invalid spec is `422 unprocessable` and names the fault. A failed or cancelled dependency is accepted only when `spec.include_failed` is true. `spec.collect` selects dependency-input collection and overrides `spec.stack_dependencies`; file collection with known stored diffs over 64 MiB is rejected. `spec.on_conflict` selects `fail`, `markers` or `skip` for input patches that do not apply. `spec.stack_dependencies` defaults to `true`, so the agent applies each dependency's `diff.patch` in `after` order before the harness starts unless `spec.collect` says otherwise. `spec.requires` holds at most 8 labels; an unknown label is accepted with a warning in the reply. A repeated `idempotency_key` with the same spec returns the existing job with `200`; the same key with a different spec is `409 conflict`. |
| `GET /v1/jobs?state=&limit=&after=` | List jobs, newest first. `limit` defaults to 200 and is capped at 200. `after` is a `JobId` cursor; an invalid value is `400 bad_request`. Returns `{"jobs":[JobView],"next":"<id>|null"}`. |
| `GET /v1/jobs?key=<key>` | Look up an idempotency key. Returns the same envelope with zero or one job and `next: null`; other filters are ignored. An empty, over-long or malformed key is `400 bad_request`. |
| `GET /v1/jobs/{id}` | Return a `JobView`. An unknown id is `404 not_found`. |
| `POST /v1/jobs/{id}/cancel` | `blocked`, `queued` and `recovering` jobs become `cancelled` at once; `leased`, `running` and `needs_input` jobs get `cancel_requested = true` and the agent is told on its next poll. A terminal job is `409 conflict`. Returns a `JobView`. |
| `POST /v1/jobs/{id}/edit` body `JobEdit` | Change a `queued` or `blocked` job's provider, model, fallbacks, requirements, tolerations, dependencies, collect mode, conflict policy or failed-input opt-in in place. Absent fields stay unchanged; an empty array clears a list. The job keeps its id and dependents. Returns a `JobView`. Another state is `409 conflict`; a dependency cycle is `400 bad_request`; an invalid spec or an unknown, failed or cancelled dependency is `422 unprocessable`, naming the fault. `include_failed` also permits an already dead dependency. Edited dependencies move the job between `blocked` and `queued` and respect the 16-dependency limit. |
| `GET /v1/jobs/{id}/logs?offset=N&wait=25` | Return raw log bytes from `offset` with `Content-Type: text/plain`, `X-Lu-Log-Length` and `X-Lu-State`. `wait` holds the request for up to 25 seconds while no new bytes arrive. An unknown id is `404`. |
| `GET /v1/jobs/{id}/artifacts/{name}` | Return one fixed artifact name: `diff.patch`, `run.log` or `report.json` from any job that stored it, including a `failed` job with a partial diff. An unknown name or an artifact that is not stored is `404`. |
| `GET /v1/status` | Return a `StatusView`. |
| `GET /v1/repos` | Return `{"repos":[{"name","url","synced_at","heads"}]}`. `synced_at` is null before the first sync; `heads` maps up to 20 branch names to commit ids. |
| `POST /v1/repos/{name}/sync` | Fetch one configured mirror and return `{"name","heads","synced_at"}`. An unknown configured name is `404`; a fetch failure is `500` with any credential removed from the message. |
| `GET /v1/nodes` | Return an array of `NodeView`s. |
| `GET /v1/nodes/{name}/stats` | Return the latest `NodeStats` from the node's heartbeat. An unknown node is `404`; a known node with no stats returns `{}`. Stats live in memory and are lost on controller restart. |
| `POST /v1/nodes/{name}/drain` | Stop assigning new jobs to the node. A node that has never heartbeated is `404`. |
| `POST /v1/nodes/{name}/undrain` | Let the node take new jobs again. A node that has never heartbeated is `404`. |
| `POST /v1/nodes/{name}/uncordon` body optional `{"provider":"<name>"}` | Clear one provider cordon, or every cordon on the node with an empty body. An unknown node is `404`. Returns `{"removed":["<provider>",…]}` with the providers actually removed. |
| `GET /metrics` | Return Prometheus text with `Content-Type: text/plain; version=0.0.4; charset=utf-8`. It needs an operator token. Series are listed in [monitoring.md](monitoring.md#series). |
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
| `provider` | string | no | Omitted when absent. `any` selects the node's published provider/model pairs in preference order. |
| `model` | string | no | Omitted when absent; forbidden when `provider` is `any`. |
| `timeout_secs` | integer | no | 1 to 86400; defaults to 3600. |
| `idle_timeout_secs` | integer | no | 1 to 86400; omitted when absent. |
| `on_stuck` | string | no | One of `none`, `cancel` or `retry`; omitted when absent. An absent value uses the agent default; an explicit `none` disables the policy. |
| `stuck_after_secs` | integer | no | 60 to 86400; omitted when absent. Continuous stuck time before the agent acts; an absent value uses the agent default. |
| `idempotency_key` | string | no | Omitted when absent. |
| `after` | array of job ids | no | At most 16 unique ids; omitted when empty. |
| `stack_dependencies` | boolean | no | Defaults to `true`; omitted when `true`. When true, dependency diffs are applied to the job's base tree. |
| `collect` | string | no | One of `none`, `files`, `apply` or `both`; aliases `off`, `false`, `diffs`, `diff`, `patches` and `patch` are accepted case-insensitively. Omitted when absent, in which case `stack_dependencies` decides the effective mode. An explicit mode wins: `files`/`both` stage `.lu/inputs`; `apply`/`both` use the existing diff-stacking path. |
| `include_failed` | boolean | no | Defaults to `false`; omitted when `false`. When true, failed or cancelled dependencies are accepted and released instead of failing this job; their artifacts become inputs only when a diff was stored. |
| `on_conflict` | string | no | One of `fail`, `markers` or `skip`, case-insensitive; defaults to `fail` and is omitted when default. Controls input patches that do not apply; explicit values are echoed in `JobView.spec`. |
| `requires` | array of labels | no | At most 8 labels; omitted when empty. |
| `tolerates` | array of taints | no | At most 8 taints; omitted when empty. A tainted node takes the job only if it tolerates every taint. |
| `fallbacks` | array of objects | no | Up to 4 alternative `{provider, model}` choices, tried in order when the primary provider is unavailable on a node. Omitted when empty; forbidden when `provider` is `any`. |
| `priority` | integer | no | `0` to `9`; defaults to `5`. Higher dispatches first, then creation time. Always present in views. |
| `group` | string | no | `1` to 64 characters from `A-Z a-z 0-9 . _ -`; omitted when absent. A configured `[[controller.groups]]` entry caps concurrent jobs with the same value. |
| `allow_empty` | boolean | No | Defaults to `false`; omitted when `false`. When true, a successful job with no diff gets no `outcome_note`. |
| `expect` | object | no | Expected outputs of a successful run; omitted when absent. Accepts `{ files, changed, forbid_changed, min_diff_bytes, max_diff_bytes, mode }`. Each list holds at most 16 entries. `files` are relative paths without `..`; `changed` and `forbid_changed` accept `*`, `**` and `?` globs. `min_diff_bytes` and `max_diff_bytes` bound the exported diff size. `mode` is `suspect` (default) or `fail`. |

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
| `waiting_reason` | string or null | Why a queued job cannot be placed, such as `group limit (build 4/4)`, `no node with label php8.3`, `only tainted nodes are free: homelab` or `no node has a healthy provider for provider "any"`. A group cap is reported before label, taint and provider reasons. |
| `assigned_provider` | string or null | The provider choice the job was assigned with, when it has fallbacks. |
| `assigned_model` | string or null | The model of that choice. |
| `diff_bytes` | integer or null | Present only for terminal jobs (`done`, `failed`, `cancelled`). Size of the stored `diff.patch` in bytes, or null when none. A failed job can have a non-empty partial diff; clients can fetch it with `GET /v1/jobs/{id}/artifacts/diff.patch`. |
| `outcome_note` | string or null | Omitted when absent. `no_changes` when the job is `done` and its stored diff is empty and `allow_empty` is false; otherwise `expect_failed` when a suspect expectation failed. `no_changes` takes precedence when both apply. |
| `warnings` | array of strings | Submit replies only. |

### `JobEdit`

All fields are optional and omitted when absent. Absent fields stay unchanged; a supplied array replaces the list, and an empty array clears it. Unknown fields are rejected. `any` with an explicit `model` or `fallbacks` is `422`.

| Field | Type | Rule |
| --- | --- | --- |
| `provider` | string | Valid provider name, or `any`. |
| `model` | string | At most 128 characters; forbidden with `provider: "any"`. |
| `fallbacks` | array of objects | At most 4 `{provider, model}` choices; forbidden with `provider: "any"`. |
| `on_stuck` | string | One of `none`, `cancel` or `retry`; overrides the stuck policy. |
| `stuck_after_secs` | integer | 60 to 86400; overrides the continuous stuck interval. |
| `requires` | array of labels | At most 8 labels. |
| `tolerates` | array of taints | At most 8 taints. |
| `collect` | string | One of `none`, `files`, `apply` or `both`, or an accepted alias; overrides `stack_dependencies`. |
| `include_failed` | boolean | When true, failed or cancelled dependencies are accepted and their stored diffs are available as inputs. |
| `on_conflict` | string | One of `fail`, `markers` or `skip`, case-insensitive; replaces the conflict policy. |
| `priority` | integer | `0` to `9`; higher dispatches first, then creation time. |
| `group` | string or null | `1` to 64 characters from `A-Z a-z 0-9 . _ -`, or `null` to clear the group. |
| `after` | array of job ids | At most 16 unique ids, without a cycle or a finished dependency unless `include_failed` is set. |
| `expect` | object | Replacement expectation fields: `files`, `changed`, `forbid_changed`, `min_diff_bytes`, `max_diff_bytes`, `mode`. Present lists replace the stored ones; present bounds and mode are set; absent fields stay unchanged. |

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
| `provider_models` | array | Healthy `{provider, model}` pairs published by the agent for `provider = "any"` jobs. Always present, including `[]` when empty. |
| `labels` | array of strings | Labels from the latest heartbeat. |
| `label_sources` | object | Labels published by an operator-declared probe, each mapped to `probed`. Other labels have no entry. |
| `taints` | array of strings | Taints from the latest heartbeat. |
| `current_job` | string or null | Job id the node holds. |
| `cordons` | array | Providers with an unexpired cordon on this node. |
| `cordon_details` | array of `CordonView` | The same cordons with their source, reason and timing. Always present, including `[]`, in schema 8; older controllers omit it. |
| `reiki` | object or null | Opaque Reiki document from the heartbeat. |
| `load1` | number or null | One-minute load from the latest host stats. |
| `mem_available_kb` | integer or null | Available memory from the latest host stats. |
| `mem_total_kb` | integer or null | Total memory from the latest host stats. |
| `stuck` | integer | Number of the node's running jobs whose latest stats say `stuck`. |

### `CordonView`

| Field | Type | Meaning |
| --- | --- | --- |
| `provider` | string | Cordoned provider name. |
| `source` | string | `reiki` for a cordon with a Reiki retry estimate; `failure` for a fixed provider-failure cooldown. Existing rows upgraded from older schemas are `failure`. |
| `reason` | string | Why the cordon was set, including the summary and retry estimate when that is known. Existing rows upgraded from older schemas say `Cordoned before upgrade`. |
| `set_at` | integer | Unix seconds when the cordon was set. Rows upgraded from older schemas use the upgrade time. |
| `until` | integer or null | Unix seconds when a `failure` cordon expires; null for a Reiki cordon that lifts on healthy heartbeats. |
| `lifts_after_healthy_heartbeats` | integer or null | Number of consecutive healthy provider heartbeats that clear a Reiki cordon, currently `2`; null for a `failure` cordon. |

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

`GET /v1/nodes/{name}/stats` returns `NodeStats`. Empty fields are omitted, so a node that reports nothing is `{}`.

| Shape | Fields |
| --- | --- |
| `NodeStats` | `host` (`HostStats` or null), `jobs` (array of `JobStats`), `labels` (array of strings), `label_sources` (object mapping a probed label to `probed`). |
| `HostStats` | `load1`, `load5`, `load15`, `mem_total_kb`, `mem_available_kb`, `swap_used_kb`, `disk_free_bytes`, `disk_total_bytes`, `uptime_secs`. The two disk fields are optional and omitted only when unreadable. Counters come from `/proc` and the disk fields from the work filesystem; non-Linux agents report no host stats. |
| `JobStats` | `job_id`, `pgid`, `output_idle_secs`, `cpu_idle_secs`, `stuck`, `processes` (array of `ProcessStats`, at most 64 busiest entries). |
| `ProcessStats` | `pid`, `ppid`, `state`, `cpu_secs`, `rss_kb`, `age_secs`, `cmdline` (first 80 characters with control characters replaced), `open_files`. |

## Agent

| Route | Meaning |
|---|---|
| `POST /v1/agent/poll` body `{"heartbeat": Heartbeat, "wait_secs": 0..30}` | Register a heartbeat and ask for work. `wait_secs` above 30 is `400`. Returns `204` when there is no work, or `200` with an array of events: `{"kind":"cancel","job_id":…,"epoch":…}` and `{"kind":"assign","job_id":…,"epoch":…,"spec":JobSpec}`; an `assign` may also carry optional `"provider"` and `"model"` fields. An `assign` creates a lease for `lease_secs` and increments the epoch. A heartbeat whose node differs from the token's node is `403`. |
| `GET /v1/agent/jobs/{id}/base?epoch=N` | Fetch the job's base pack when the spec has `base_commit`. Returns `application/x-git-pack` with header `x-lu-base-commit`; the pack holds that commit and its tree without parent history. Missing commit, wrong node, unavailable base or stale epoch gives `403`, `404` or `409` as appropriate; a pack over 256 MiB gives `413 too_large`. |
| `POST /v1/agent/jobs/{id}/start` body `{"epoch":N}` | Move a leased job to `running` and return its `JobView`. A wrong node is `403`; a stale epoch is `409 stale_epoch`. The lease is extended. |
| `POST /v1/agent/jobs/{id}/renew` body `{"epoch":N}` | Extend the lease by `lease_secs` and return `{"cancel":bool}`. Log append and `start` also extend the lease; `poll` renews every job held by the node. |
| `POST /v1/agent/jobs/{id}/log?epoch=N&offset=M` body raw bytes | Append a chunk at `offset`. The offset must equal the stored length; otherwise `409 conflict` returns `{"error":"conflict","message":"conflict","length":L}`. The body is limited to 64 KiB plus framing. When the 50 MiB log cap is reached, room for the `[log truncated]` marker is reserved first so the stored log ends with that marker. |
| `PUT /v1/agent/jobs/{id}/artifacts/diff.patch?epoch=N` body raw bytes | Store the diff, capped at 10 MiB. The controller chooses the path and returns `200` or `413 too_large`. |
| `PUT /v1/agent/jobs/{id}/artifacts/report.json?epoch=N` body raw bytes | Store the report, capped at 1 MiB. The controller chooses the path and returns `200` or `413 too_large`. |
| `GET /v1/agent/jobs/{id}/deps?epoch=N` | Return the job's dependencies in `after` order. Each entry has `job_id`, `state`, `diff_bytes` (`0` when no diff is stored), `idempotency_key`, `provider`, `model`, `node`, `started_at`, `finished_at`, `failure_class` and `summary`. Wrong node is `403`; stale epoch is `409 stale_epoch`. |
| `GET /v1/agent/jobs/{id}/deps/{dep}/diff?epoch=N` | Return the dependency's raw `diff.patch`. A `dep` outside the job's `after` list is `403`; a missing diff is `404`; a stale epoch is `409 stale_epoch`. |
| `GET /v1/agent/jobs/{id}/deps/{dep}/artifacts/{name}?epoch=N&tail=M` | Return one fixed dependency artifact: `diff.patch`, `report.json` or `run.log`. Any other name is `404`; a `dep` outside `after` is `403`. `tail=M` returns only the last `M` bytes. Stale epoch is `409 stale_epoch`. |
| `POST /v1/agent/jobs/{id}/complete` body `Completion` | Finish the job and return its `JobView`. A stale epoch is `409 stale_epoch` and the result is discarded. For a provider failure, `retry_after_secs` sets the cordon length on the node, clamped to 5 seconds through 24 hours. Two healthy provider heartbeats clear such a cordon early. A stuck-policy stop may carry `stuck_mode: "cancel"|"retry"`; it is counted in `lu_jobs_stuck_stopped_total` and does not change how the outcome is stored. |

A `JobSpec.base_commit` is a 40-hex commit id. The agent fetches a pack for it unless `base_source` is `clone`. With `controller`, a `404` fails the job; with `auto`, the default, a `404` falls back to cloning the spec's `repo`.

`Heartbeat` may also carry an optional `provider_models` array of `{provider, model}` pairs (at most 64, unique provider names, valid names and models). Agents publish only pairs whose provider Reiki reports healthy; the controller records them on the node and uses them for `provider = "any"` jobs. Older agents that omit the field are accepted. `Heartbeat` may also carry optional `version` (at most 32 printable ASCII characters), optional `labels` (at most 32 labels, each 1–64 characters from letters, digits, `.`, `_`, `-`, `/`, with at most one `=`), optional `label_sources` (a map of probed labels to `probed`; keys must also appear in `labels`, at most 32 entries), optional `taints` (at most 8, same label syntax) plus optional `host` and `jobs` stats. Older agents that omit them are accepted; each heartbeat replaces the controller's previous stats, labels, label sources and taints for that node, so omitting them clears a stale job tree, label set and taint set. The combined stats are trimmed to 16 KiB before validation by dropping processes first and then whole jobs. Lu never interprets labels or taints; dispatch compares them exactly.

## Controller rules
- Queue order is priority descending, then creation time ascending. A job in a configured group is dispatched only while its group has fewer than `max_running` jobs in `leased`, `running`, `recovering` or `needs_input`; a job blocked by a full group stays `queued` with `group limit (<name> <running>/<max>)` as its waiting reason. Provider, label and taint rules still apply first: priority and group caps only order eligible jobs, they never make an ineligible node eligible.

- A job with `after` dependencies starts `blocked`. When a dependency becomes `done`, every blocked dependent whose dependencies are all `done` becomes `queued` in the same transaction. When a dependency becomes `failed` or `cancelled`, blocked dependents fail with `failure_class: "dependency"` and summary `dependency <id> <state>`, cascading through their own dependents. A dependent with `include_failed: true` is instead released when its other dependencies are ready; a dead dependency was accepted at submit time only for such dependents. Dependency failures never retry.
- A dependent job with `stack_dependencies: true` (the default) stages its base commit, then the agent fetches the dependency list and applies each finished dependency's `diff.patch` in `after` order before starting the harness. An explicit `spec.collect` overrides this fallback: `files` stages inputs, while `apply` and `both` also apply them. A dependency with no diff is skipped. Each patch is tried with libgit2 apply, libgit2 three-way, then `git apply --3way` when `git` is on the agent's PATH. `spec.on_conflict = "fail"` (the default) stops before the harness and completes `Failed(Conflict)` with a summary naming the input, file and hunk; `skip` leaves the patch unapplied and records the first failing file and hunk in `INDEX.md`; `markers` leaves labelled conflict markers in the affected files and records `markers (N files)`. Later inputs still apply under `skip` and `markers`. `conflict` never retries. The job's `diff.patch` is always against its base, includes applied dependency changes and any conflict markers, and excludes `.lu`. At most 16 inputs and 64 MiB of stored diffs are accepted, checked again at stage time.
- Dispatch first applies labels and taints: it assigns a queued job only when its node is idle, not draining, has no held job, offers every label in `spec.requires`, and the job's `spec.tolerates` covers every taint the node reports. A node with no taints accepts any job; a job tolerating nothing avoids all tainted nodes. A job with no `spec.provider` then runs on that node. A job with `spec.provider = "any"` uses the node's published `provider_models` in order and takes the first pair whose provider is healthy and has no unexpired cordon; with no such pair on any eligible node it stays `queued` with a waiting reason. Otherwise the choices are tried in order—the primary `spec.provider`/`spec.model`, then `spec.fallbacks` in list order—and the job uses the first choice whose provider the node reports healthy and has no unexpired cordon for. The chosen pair is stored as `assigned_provider`/`assigned_model`; a fallback without a model uses the primary `spec.model`. A requeued job is dispatched afresh and may choose another pair, including on the same node.
- A lease that is not renewed for `lease_secs` expires: the job becomes `recovering`, then after `recovery_grace_secs` returns to `queued` with a bumped attempt (class `node`) unless attempts are exhausted (`max_retries`), when it becomes `failed`.
- `Completion` with a `provider` failure class cordons `(node, assigned_provider)` for `provider_cooldown_secs`, or for `retry_after_secs` when given; the pair is the one chosen at dispatch, not necessarily the primary. Assignment skips a cordoned choice, so the requeued job can move to its next fallback, another node, or the same node under a different provider. Two healthy heartbeats clear a `retry_after_secs` cordon.
- Epoch fencing: every agent write names the epoch it was assigned; a write for an older epoch is `409 stale_epoch` and changes nothing.
- On start the controller marks every `leased` or `running` job `recovering` (it was restarted mid-flight); agents that still hold the lease resume by renewing within the grace period.
- Job logs and artifacts are stored under `artifacts_dir/<job id>/`, names fixed by the controller, files `0600`, directories `0700`.
