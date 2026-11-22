# Job-scoped capability

A harness running inside a job can ask its node agent for its own context and for follow-up work
through a per-job Unix socket. The harness never talks to the controller and never holds a
controller URL or token. The node agent forwards requests to the controller with its own agent
credential; the controller is the security boundary, and the MCP server is only a convenience for
the harness.

The capability is off by default. An operator grants it at submit time with
`lu submit --allow-spawn`; a job cannot grant the right to itself. See
[Configuration](config.md#scope) for the limits and [MCP server](mcp.md#scoped-mcp-ting-mcp) for
the harness-facing command.

## Threat model

Assume the harness is compromised and hostile. With job-scoped access it can use **only its own
per-job socket**, and only for the six actions below. It never receives controller or operator
credentials, in the environment, on the command line or in a config file. A leaked socket from one
job is not a credential for any other job.

| A compromised harness can | A compromised harness cannot |
| --- | --- |
| Read its own spec, state and dependency list (status only). | Read another job's task text, including its parent's or a sibling's. |
| Submit children within the parent's repo, provider, label, priority and time budget. | Grant itself `allow_spawn`, raise its priority, widen the repo list or name another job's dependency. |
| List, inspect, fetch scrubbed `diff.patch`/`report.json` from, and cancel its own direct children. | List unrelated jobs or read their logs, artifacts or idempotency keys. An unrelated or missing id returns the same `404 not found`. |
| Spend a bounded per-job and per-minute spawn budget. | Reach node, drain, cordon, config, metrics, backup, audit or `collect` apply routes. |
| See child output, which is labelled as untrusted data. | Treat child output as trusted instructions; the environment and result envelope mark its source. |

Limits protect the controller and the operator's budget from runaway fan-out and large reads. They
are enforced at the controller even if the agent or MCP server is bypassed.

## The per-job socket

When a job with `allow_spawn = true` starts, the agent creates a Unix domain socket for that job in
a private directory under `<work_dir>/scope`: directory mode `0700`, socket mode `0600`. Set
`[agent] harness_uid` (and optionally `harness_gid`) when the harness runs as a different account;
the agent then chowns the socket root and each per-job directory/socket to that account and checks
`SO_PEERCRED` against its uid. Without `harness_uid`, the harness must share the agent's uid. The
harness sees only `LU_JOB_SOCKET=<path>`; there is no token, controller URL or config path to read.
A peer from another uid is refused even if it can reach the path.

The socket is a small attack surface: one bounded JSON line per request (the request envelope is
capped at 272 KiB: `MAX_TASK_BYTES` plus a 16 KiB framing allowance), a request rate limit, no
shelling out, and a shared strict decoder that is
fuzzed on both the agent socket and the controller side. The workspace forbids `unsafe_code`; peer
credentials come from a safe crate.

The socket dies when the job reaches a terminal state, loses its lease, changes epoch or is
cancelled. A retry or a fresh attempt gets a fresh socket. Requests from a stale socket are refused
by the controller's lease and epoch checks.

## The six actions

Every route accepts one bounded JSON request with the calling job's `job_id` and lease `epoch`, plus
an optional action-specific payload. Results carry `source: "child_job"` and `untrusted: true`.

| Action | Input | Result |
| --- | --- | --- |
| `job_context` | — | Own job id, state and spec, plus its dependency list: id, idempotency key, state, failure class and a summary capped at 4 KiB. A missing dependency is reported with state `unknown` rather than as a different job's data. |
| `submit_child` | `task`, plus optional `repo`, `provider`, `model`, `priority`, `timeout_secs`, `requires`, `after`, `idempotency_key` | New child job id. Requires `allow_spawn` on the parent and a spawn slot. |
| `list_children` | — | Direct children: id, state, failure class, summary (4 KiB cap). |
| `get_child` | `child` | One direct child's state, failure class and short summary. |
| `get_child_artifact` | `child`, `artifact` | A direct child's `diff.patch` or `report.json`, re-scrubbed on read and capped per call. Available only while `[artifacts] scrub = true` (the default). Any other artifact name is refused. |
| `cancel_child` | `child` | Cancel one direct child and, as for operator cancel, its descendants. |

Each action has exactly one controller route, `POST /v1/scope/{route}`; the route name must match
the action in the request. See [HTTP API](api.md#job-scoped-routes) for the wire envelope and status
codes.

## Controller checks

`/v1/scope/{route}` accepts **only** an agent token, a job id and a lease epoch:

- Operator tokens are refused; agent tokens work only for a job currently leased to that agent's
  node at exactly that epoch. Node, lease and epoch checks fence a stale or transferred job.
- A job that is missing, unrelated, not leased to the caller or outside the lease window returns the
  same `404 not_found` as a missing id, so a harness cannot probe for other jobs.
- Every limit failure is a typed `limit_exceeded` response carrying the limit name in an `error.limit`
  field; nothing is silently dropped.
- No scope route reads task text for a job other than the caller's own spec, and error messages are
  fixed or sanitised so child bytes are never echoed.

## Limits

All limits live in `[scope]` on the controller; the defaults are safe and `enabled = false`
refuses every scoped call. An operator must also set `allow_spawn` on the job before its harness
gets a socket.

| Limit | Default | Protects |
| --- | --- | --- |
| `enabled` | `false` | Master switch; no scoped route is reachable until an operator enables it. |
| `max_children_per_job` | `8` | Direct children per parent job. |
| `max_descendants_per_root` | `32` | Total descendants with the same `root_job_id`. |
| `max_depth` | `2` | Distance from the root; a child at the cap cannot spawn again. |
| `max_task_bytes` | `64 KiB` | Task text of one child. |
| `max_spawn_per_minute` | `6` | Submit rate per parent job in a rolling minute on the controller. |
| `max_artifact_read_bytes` | `1 MiB` | Bytes returned by one `get_child_artifact` call. |
| `max_summary_bytes` | `4 KiB` | Length of a child or dependency summary shown through scope tools. Summaries are truncated to this cap, not rejected. |

Exceeding a hard limit returns `limit_exceeded` with the corresponding limit name, also counted in
`lu_scope_limit_exceeded_total{limit}`.

## Submitting a child

A child starts from its parent's spec with the harness-facing choices clamped by the controller:

- `repo` stays the parent's unless the parent's `spawn_repos` allowlist names another one;
  `git_ref` and `base_commit` cannot be changed.
- `provider` and `model` must be the parent's own choice or an entry of the parent's `fallbacks`.
- `priority` is never above the parent's, `requires` is a subset of the parent's labels, and
  `timeout_secs` is capped by the parent's remaining time.
- `after` may name only the parent's own children or dependencies from the parent's spec.
- `idempotency_key` is namespaced as `scope:<parent-id>:<key>`, so a harness cannot collide with or
  probe another job's key.
- Children inherit `allow_spawn` only when the parent's spec sets `spawn_inherit = true`; depth is
  counted on every spawn.

## Lineage and operator views

Jobs carry `parent_job_id`, `root_job_id` and `depth` (top-level jobs are depth 0). These columns
are part of the schema 3 baseline; schema 1 and 2 databases gain them in place at startup. Operators
retain full visibility: `lu ls --tree` groups jobs by parent, `lu show` prints parent, root, depth
and the spawn settings, and `lu top` marks a spawned job with `scope:d<depth>`.

Cancelling a parent cancels its descendants as well. `lu cancel --keep-children` leaves them
running; `cancel_child` uses the default cascade.

## MCP for the harness

`ting mcp` is a stdio MCP server that reads
`LU_JOB_SOCKET` and exposes only the six tools above. An opencode configuration uses:

```toml
mcp = { lu = { command = ["ting","mcp"] } }
```

The server refuses to start if operator credentials are visible in the environment (`LU_URL`,
`LU_TOKEN_FILE`) or at the standard operator config paths (`$XDG_CONFIG_HOME/lu/operator.toml`,
`~/.config/lu/operator.toml`, `/etc/lu/operator.toml`), and says which source was found. Tool
descriptions mark child output as untrusted data and every result has `source: "child_job"`. The
operator's `lu mcp` is unchanged and keeps full access; it is for humans, not for harnesses.

## Audit and observability

With `[audit] path` set, each scope action writes a `scope_action` event with actor `job:<id>`, the
agent's node, `source: "child_job"`, the action, its outcome and the epoch. The record never
contains task text, prompts, artifact content or tokens. `/metrics` exports
`lu_scope_requests_total{route,result}` and `lu_scope_limit_exceeded_total{limit}`; `lu status`
exposes the `scope_*` settings, and `lu show` prints the `audit_write_errors` counter for the job,
as it does for other controller actions.

## Deployment examples

- [Container harness bind mount](scope-container.md) — keep `SO_PEERCRED` working when the harness
  runs in a container.
- [Ansible example](scope-ansible.md) — create the private account and directories, set `[scope]`
  and install the container helper.
