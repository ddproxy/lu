# Node workspaces

Each job gets one working tree under the Ting agent's `[agent] work_dir`, named by the job id. The checkout, build output and any directories a job names in `cache_dirs` live inside that tree. The agent removes it according to the job's workspace policy, so a node can run CI jobs for a long time without filling its disk.

Workspace state is agent-local. The controller keeps the latest counters and listing from the agent's heartbeat in memory; they are lost on a controller restart until the next heartbeat. No store schema change is needed.

## Job spec fields

| Field | Type | Rule |
| --- | --- | --- |
| `workspace` | string or omitted | `ephemeral`, `keep_on_failure` or `keep`. Omitted uses `[workspace] default` on the node. |
| `cache_dirs` | array of strings | Up to 16 relative paths, each 1–256 bytes, created inside the workspace before the harness starts. Empty by default. |

`lu submit` and `lu collect` accept `--workspace <policy>` and repeatable `--cache-dir <relative/path>`. A cache directory must be relative, must not contain `..`, `.`, empty segments, backslashes or unsafe characters, and an existing symlink at that path is refused. Caches are part of the job's workspace: they are removed with it and are not a cross-job persistent cache.

### Policies

| Policy | Behaviour |
| --- | --- |
| `ephemeral` (default) | After the harness finishes, the agent asks the controller whether the job's final artifacts are stored and whether any dependant still needs the tree. It deletes the workspace once released, retrying the check while artifacts are still being stored. Stacking and collection never keep the workspace; the controller uses its stored `diff.patch` for dependants. |
| `keep_on_failure` | A failed job keeps its workspace for `failed_ttl`. A successful job is treated like `ephemeral` and deleted as soon as the controller releases it. |
| `keep` | Keeps the workspace for `keep_ttl` after the job finishes, then deletes it. |

Cancellation follows the outcome recorded for the job. A running workspace is never evicted or removed by an operator clean request. Final artifacts are uploaded before release is requested, so a failed deletion is retried instead of losing the patch or report.

## Agent configuration

`[workspace]` is a top-level table in the same TOML file as `[agent]`; the workspace root is always `[agent] work_dir`. There is no `[agent] workspace` key, and unknown keys are rejected as usual.

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `default` | no | `ephemeral` | One of `ephemeral`, `keep_on_failure` or `keep`. Applies only to jobs that omit `workspace`. |
| `failed_ttl` | no | `6h` | Duration for failed `keep_on_failure` workspaces. |
| `keep_ttl` | no | `48h` | Duration for `keep` workspaces. |
| `max_bytes` | no | none | Positive integer cap on total workspace bytes. Eviction starts above the cap. |
| `low_water_percent` | no | `65` | Disk use percent at or below which eviction stops. |
| `high_water_percent` | no | `80` | Disk use percent above which eviction starts. |
| `sweep_interval` | no | `60s` | How often TTLs, retries, disk pressure and the byte cap are checked. |

Durations accept a trailing `s`, `m`, `h` or `d`, or a bare number of seconds, and must be at least one second. Water marks are 0–100 with `low_water_percent < high_water_percent`. A sweep runs off the job path, so cleanup never blocks a job start. Controller release checks run once a second with backoff after a refusal; `sweep_interval` controls the TTL and disk-pressure sweeps.

When disk use is above the high-water mark, or total workspace bytes are above `max_bytes`, the agent deletes kept workspaces oldest first until use is at or below the low-water mark and the cap is satisfied. Running and already-deleting workspaces are skipped. Each eviction is logged with the job id, bytes, age and reason; logs never contain paths outside the workspace root.

Deletion walks with `symlink_metadata`: symlinks are unlinked as links and never followed. Paths are confined to the canonical workspace root, the root itself is never deleted, and a failed deletion is logged, counted and retried with exponential backoff capped at 64 seconds.

Set `max_bytes` to a value appropriate for the node's disk; omit it when disk watermarks alone
should control eviction. The `[agent] work_dir` itself is set in the agent's base configuration.

## Observing and cleaning workspaces

Every heartbeat reports `workspace_bytes`, `workspace_count`, `workspace_reclaimed_bytes_total`, `workspace_evictions_total` and `workspace_delete_errors_total`, plus the oldest-first workspace listing. `lu node <name>` prints those counters and the separate `gc_removed_total` and `gc_bytes_freed_total` from housekeeping outside `work_dir`; `lu top` shows workspace count and bytes and the three cumulative workspace totals. `/metrics` exposes each counter with a `node` label; see [monitoring.md](monitoring.md#series).

```sh
lu node node-a --workspaces
lu node node-a clean --all
lu node node-a clean --job 01ARZ3NDEKTSV4RRFFQ69G5FAV
```

`--workspaces` prints `JOB`, `STATE`, `BYTES`, `AGE` and `POLICY`. `clean` queues a request for the agent's next poll; it never deletes a running workspace, and the reply's `queued: 1` means the request was queued, not that one tree was already deleted. The agent logs how many trees it queued, refused or could not find. Route details are in [api.md](api.md#operator).

See [operations.md](operations.md#workspace-lifecycle) for the runbook and [troubleshooting.md](troubleshooting.md#disk-fills-with-job-workspaces) for symptoms.
