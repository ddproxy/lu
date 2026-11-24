# Operations

This is the operator runbook for a packaged controller: upgrades, backup and restore, restart
behaviour, provider cordons and node drains, and how to read node state. Installation and the
systemd units are in [install.md](install.md); the agent's isolation model is in
[security.md](security.md).

| Topic | Read it on |
| --- | --- |
| Service layout, installation and units | [install.md](install.md) |
| Token generation and rotation, TLS setup | [tokens-and-tls.md](tokens-and-tls.md) |
| Metrics series, scrape and alerts, `lu status` | [monitoring.md](monitoring.md) |
| Job states, transitions and failure classes | [lifecycle.md](lifecycle.md) |
| Symptom-first checks and error codes | [troubleshooting.md](troubleshooting.md) |
| Dependency stacking and the job graph | [dependencies.md](dependencies.md) |
| Node workspace retention and disk pressure | [workspaces.md](workspaces.md) |
| Provider and label dispatch | [dispatch.md](dispatch.md) |
| MCP tools | [mcp.md](mcp.md) |
| HTTP routes and limits | [api.md](api.md) |
| Node self-service commands | [ting.md](ting.md) |
| Configuration keys | [config.md](config.md) |

## Upgrade

For the agent-host half of an upgrade, see [Updating a node](../README.md#updating-a-node): the node
drains itself with `ting drain`, waits with `ting wait-idle`, is updated and health-checked, then
undrains with `ting undrain`. That needs only the node's own token.

1. Read `CHANGELOG.md` and back up the database and artifacts ([below](#backup-and-restore)).
2. Stop the controller: `systemctl stop lu-controller.service`.
3. Replace `/usr/local/bin/lu` and any changed unit files, then `systemctl daemon-reload`.
4. Run `lu check --config /etc/lu/lu.toml` to validate config values and database health, then start the controller.
5. Check `/v1/health`, `journalctl -u lu-controller`, `lu ls` and `lu nodes`. Full config and secret-file validation happens when the controller starts.

The store uses schema version 7. It creates the schema for a new database and migrates schema 1 to 6 databases in place on startup, adding the scrubbing, lineage, publish and stacked-chain columns (`staged_commit`, `blocked_reason`, `blocked_since`, `eligible_since`) and the additive `request_keys` table for HTTP idempotency. Any other schema version is refused with a clear message. Downgrades are not supported. Backups and restores require exactly the current schema version (7), so take a fresh backup after upgrading. An older backup must first be used as a database path and migrated by startup before it can be restored.

There is no online config reload. `SIGHUP` is not handled; restart the controller after changing config, TLS files or token hashes. For the `HOME` and `PATH` settings needed by the contributed unit, see [Running the controller under systemd](install.md#running-the-controller-under-systemd).

## Upgrading the controller

A controller of a newer release starts on the previous release's database and migrates it forward in place; there is no separate migration command. To upgrade:

1. Take a backup with `lu backup --out <file>` (see [Backup and restore](#backup-and-restore)); the rolling-update playbook also keeps a dated copy of the database file.
2. Stop the controller, replace the binaries, and start it. Agents of the old and the new release can stay connected throughout; see [compatibility](compatibility.md).
3. Confirm the result with `lu status --json`: `version` is the new release and `schema_version` equals the schema the release notes name (6 for 0.6.0).

The migration only moves forward. To go back to an older release, stop the controller and restore the backup taken in step 1 (or the dated database copy); do not start an older binary on a migrated database, which refuses a schema version it does not know.

## Backup and restore

The database uses SQLite WAL mode. A plain copy of `lu.db` while the controller is running can miss committed transactions held in `lu.db-wal`.

Take an online snapshot without stopping the controller:

```sh
install -d -m 0700 /var/backups/lu
lu backup --config /etc/lu/lu.toml --out "/var/backups/lu/lu-$(date +%F).db"
```

`lu backup` uses SQLite's `VACUUM INTO`, writes the copy with mode `0600`, runs
`PRAGMA integrity_check`, and prints the size, schema version and job count. It refuses to
replace an existing file unless `--force` is given. `lu backup` and `lu restore` read only
`[controller] db`; `lu check` validates config values, uses that key for its database checks and
lists the checks it ran. None of these commands opens TLS keys or token files, so they also run as
root against a service account's secrets.

For a daily snapshot, install a small wrapper, a service and a timer as root:

```sh
# /usr/local/bin/lu-backup
#!/bin/sh
set -eu
/usr/local/bin/lu backup --config /etc/lu/lu.toml \
    --out "/var/backups/lu/lu-$(date +%F).db" --force
find /var/backups/lu -name 'lu-*.db' -mtime +30 -delete
```

```sh
chmod 0755 /usr/local/bin/lu-backup
```

```ini
# /etc/systemd/system/lu-backup.service
[Unit]
Description=Lu database backup
After=lu-controller.service

[Service]
Type=oneshot
User=lu
Group=lu
ExecStart=/usr/local/bin/lu-backup
ReadWritePaths=/var/backups/lu
```

```ini
# /etc/systemd/system/lu-backup.timer
[Unit]
Description=Daily Lu database backup

[Timer]
OnCalendar=daily
Persistent=true

[Install]
WantedBy=timers.target
```

```sh
install -d -m 0700 -o lu -g lu /var/backups/lu
systemctl daemon-reload
systemctl enable --now lu-backup.timer
```

Keep the backup directory private because the database holds job tasks and summaries.

Artifacts are ordinary files under `/var/lib/lu/artifacts/<job id>/`. Back them up with modes preserved:

```sh
tar -C /var/lib/lu -czf "/var/backups/lu/artifacts-$(date +%F).tar.gz" artifacts
```

The database and artifacts are not updated in one transaction. For a consistent pair, stop the controller before taking both; otherwise accept that a job may reference an artifact that is missing from, or newer than, the snapshot. Backups contain job tasks, diffs, reports and logs, which may include source code and secrets that were present in a checked-out repository: store them with the same care as the live artifact directory.

Restore the database:

```sh
systemctl stop lu-controller.service
lu restore --config /etc/lu/lu.toml /var/backups/lu/lu-YYYY-MM-DD.db
systemctl start lu-controller.service
```

`lu restore` verifies the snapshot, refuses a file that is not a Lu database (an empty or
foreign SQLite file would replace your data with nothing), requires exactly the current schema version (7) and
every table that version creates, and refuses when the database is still in use, so stop the
controller first.
Only one restore runs at a time. It stages the copy beside the database with the old file's
owner and mode `0600`, moves the current database and every `-wal`, `-shm` and `-journal` file
to `<db>.pre-restore-<unix seconds>` names, then renames the staged copy into place. Run it as
root or as the controller's own account; either way the installed file keeps the old owner.
Boot recovery then applies to jobs that were leased or running in the snapshot.

An older backup is refused by `lu restore`. Point `[controller] db` at a copy of it, start the
controller once so startup migrates it in place, stop the controller, and take a fresh `lu backup`
before restoring that snapshot.

If a restore is interrupted, the previous database is still under its `pre-restore` name:
`mv <db>.pre-restore-<seconds> <db>`, and the same for each `-wal` or `-shm` file, puts it back.
A failed install does this itself and says so. `lu backup` refuses an output path that is the
database or one of its files, or a symlink, and removes a partial file if it fails.

Artifacts are not part of the database snapshot. Restore them separately:

```sh
rm -rf /var/lib/lu/artifacts
tar -C /var/lib/lu -xzf /var/backups/lu/artifacts-YYYY-MM-DD.tar.gz
chown -R lu:lu /var/lib/lu/artifacts
chmod 0700 /var/lib/lu/artifacts
```

## Conditional writes and request keys

Operator writes accept the optional `If-Match` and `Idempotency-Key` headers and the `?dry_run=1`
query parameter described in [api.md](api.md#conditional-and-idempotent-writes). A `412
precondition_failed` means the job changed after the ETag was read; re-read the job and retry with
the new ETag. An idempotent key is scoped to the authenticated principal and expires after 24 hours;
the `request_keys` rows are removed opportunistically when that key is used again. Every applied,
replayed, dry-run and precondition-failed mutation writes an audit record carrying the principal and
role, so a replay is distinguishable from a fresh mutation. The older `idempotency` table maps job
spec keys to jobs and is unchanged.

## What a controller restart does

Legal state transitions and the recovery state machine are in [lifecycle.md](lifecycle.md); this section covers the procedure around a restart. On startup the controller runs boot recovery before binding its listener. If recovery fails, startup fails and no traffic is served, so stale leases can never be dispatched against a fresh monotonic clock:

- Every job in `leased` or `running` becomes `recovering`. Its node and epoch are kept.
- Jobs already in `recovering` stay there, but their grace timer is restarted. Lease and recovery timestamps are based on a monotonic clock that begins again with the process, so the old values are not meaningful after a restart.
- An agent that still holds the lease and renews the same epoch within `recovery_grace_secs` moves the job back to `running` and continues.
- If the grace period expires first, a node-class failure is applied. If `attempt <= max_retries`, the job returns to `queued`, the attempt is incremented, the node is cleared, and `failure_class`, `cancel_requested` and `finished_at` are cleared. Otherwise it becomes `failed` with `failure_class: "node"`.
- `queued`, `blocked`, `done`, `failed` and `cancelled` jobs are not changed by boot recovery.

The listener stops accepting new connections on `SIGTERM` or Ctrl-C and drains in-flight connections for up to 10 seconds. The contributed unit allows 40 seconds before systemd sends `SIGKILL`.

If the controller is down for longer than a lease, agents cannot renew. When it returns, the same recovery path applies; a job can therefore consume a retry even though the harness itself did nothing wrong.

## Dependency chains and blocked dependents

A failed or cancelled predecessor leaves its blocked dependents `blocked` rather than failing them
immediately. `lu ls --state blocked` and `lu show <dependent>` show
`blocked_reason: "predecessor failed"` plus a `blocked_until` deadline;
`[controller] blocked_dependent_ttl` (default `24h`) sets how long they wait before failing with
`failure_class: "dependency"`. `lu top` and the `lu_blocked_dependents_total` metric show the waiting
count. See [dependencies.md](dependencies.md#waiting-and-promotion).

```sh
lu ls --state blocked
lu show 01ARZ3NDEKTSV4RRFFQ69G5FAV     # inspect the predecessor and the deadline
lu retry 01ARZ3NDEKTSV4RRFFQ69G5FAV     # reattach waiting dependents to the new job
lu retry 01ARZ3NDEKTSV4RRFFQ69G5FAV --chain  # also retry dependents and their references
lu cancel 01ARZ3NDEKTSV4RRFFQ69G5FAV --keep-dependents
```

Chains use one base: a dependent without a pinned `source.base` inherits its predecessor's
recorded `staged_commit`, and its exported patch is relative to that base. If several dependencies
disagree, the agent stages their newest common ancestor; if any patch then fails to apply, the job
fails with class `conflict` and the summary names every contested commit. After the chain finishes,
merge the final patch onto the moved
branch yourself — Lu does not rebase it onto a later tip. Conflict resolution for known-safe paths
is opt-in through the agent's `[stack] union_paths` and `base_paths`
([config.md](config.md#stack)); per-job `stack_union_paths`/`stack_base_paths` can only narrow those
lists, and uncovered paths fail closed. See
[dependencies.md](dependencies.md#one-base-per-chain) and
[dependencies.md](dependencies.md#policy-paths-for-known-safe-conflicts).

## Provider failure detection

Failure classes, retry budgets and cancellation are in [lifecycle.md](lifecycle.md#failure-classes-and-retries); provider choice and fallbacks are in [dispatch.md](dispatch.md). Two agent-side detections matter when reading a stalled job.

When a job resolves to a `provider` (the assigned choice, or the primary when the job names one), the agent checks it against its node's Reiki reading every five seconds, but acts only on a new reading. The feed itself refreshes every ten seconds. Reiki is the authority on keys, so this works for any harness: when two fresh readings in a row show the provider cooling or disabled (no usable key), the agent stops the job like a cancel and completes it as `Failed(Provider)`, with a summary that names the provider and the soonest return time Reiki reported. The completion carries that time plus a 60-second buffer as `retry_after_secs`, and the controller cordons that provider on the node for that long instead of the configured cordon length. Two consecutive healthy provider heartbeats after the failure clear that cordon; a non-healthy reading resets the streak.

As a fallback for a job with no provider, or a node without a Reiki feed, the agent watches a running harness's output. If the newest line matches a provider-unavailable marker (the opencode message about every key in the stream's chain, a quota message, a 429, or a rate limit) and the harness then produces no output for 120 seconds, the agent stops it the same way and completes the job as `Failed(Provider)`, with a summary naming the marker. Any other output after the marker disarms the watch.

## Stuck jobs

A job is *stuck* when the running harness has written no output and its process tree has used no
CPU for more than 300 seconds — the same observation `lu node` reports as `stuck yes`. When a job
has a stuck policy, the agent counts how long that observation has been continuous; any output or
CPU activity resets the timer. Once it reaches `stuck_after_secs` (60 to 86400 seconds, default
1800), the agent stops the harness process group through the cancel stop path, uploads the log,
diff and report, and completes the job. The summary is exactly `stuck for <n>s, stopped`, where
`<n>` is the continuous stuck time.

`on_stuck` is `none` (default), `cancel` or `retry`. `cancel` completes as `failed` with
`failure_class: "execution"`; `retry` completes with `failure_class: "node"`, so the controller
requeues while the attempt budget allows; `none` leaves the job running. A job field that is
absent uses the agent's `[agent] on_stuck` and `stuck_after_secs` defaults; an explicit
`on_stuck = "none"` disables the policy. See [config.md](config.md#agent) and
[lifecycle.md](lifecycle.md#stuck-jobs).

The controller counts successful stuck completions in
`lu_jobs_stuck_stopped_total{mode="cancel"|"retry"}`, reset on restart. See
[monitoring.md](monitoring.md#series).

## Provider cordons and node drains

These are separate mechanisms:

- **Drain** stops a node taking any new job. It is set by `POST /v1/nodes/{name}/drain` and cleared by `/undrain`, and is stored in the `nodes` table. It survives a controller restart. A node must have sent a heartbeat at least once to exist; draining an unknown node returns `404 not_found`. An operator drain is operator-owned: the agent's `POST /v1/agent/undrain` refuses it with `409 conflict` and the node view reports `drain_origin: "operator"`. A node can instead drain itself with `POST /v1/agent/drain` and clear that drain with `POST /v1/agent/undrain`, reported as `drain_origin: "agent"`. See [Updating a node](../README.md#updating-a-node) for the self-service update sequence and [the `ting` reference](ting.md) for the CLI.
- **Cordon** stops a node taking jobs whose chosen provider is cordoned. It is created when a retryable provider failure completes, keyed by the pair actually assigned rather than necessarily the primary. A completion with `retry_after_secs` creates a Reiki-derived cordon that lifts after two consecutive healthy provider heartbeats; any other provider failure uses `provider_cooldown_secs` and a fixed expiry. A job with no provider is not blocked by a cordon.

Cordon expiry is stored as monotonic milliseconds internally so dispatch behaviour is unchanged,
while `cordon_details` exposes a wall-clock `until` for fixed cooldowns. Reiki cordons expose
`until: null`, `lifts_after_healthy_heartbeats: 2` and a zero-second remaining-time metric.
`lu nodes --json` shows the active cordons in `cordon_details`; human output is described below.
Clearing a cordon early needs no database edit: run `lu uncordon <node> [<provider>]`, or call
`POST /v1/nodes/{name}/uncordon`. Boot recovery deletes every cordon row on each controller start,
because a monotonic deadline from the previous process has no meaning for the new clock. Reiki
health and new provider failures set them again.

## Reading `lu nodes`

For the one-shot controller summary, see [`lu status` on the monitoring page](monitoring.md#lu-status). Human output for `lu nodes` has seven columns:

```text
NAME    VERSION  HARNESS  LABELS                        CORDONS      LAST_SEEN            DRAINED
node-a  0.4.0   idle     !homelab,gpu,php8.3,region=eu   !zen,!mistral 2026-11-21 09:12:00Z no
```

- `NAME` is the node name bound to the agent token.
- `VERSION` is the agent version from its latest heartbeat, or `-` for an older agent.
- `HARNESS` is the last reported harness state (`idle`, `busy`, or `-` if unknown).
- `LABELS` is the comma-separated label list from the latest heartbeat, with each taint shown first with a `!` prefix, or `-` for none. A label published by a probe is followed by ` (probed)`. A long list is cut to fit the column and ends with `+N` for the hidden entries.
- `CORDONS` is the comma-separated list of cordoned providers, each with a `!` prefix, or `-` for none. It is separate from taints because a cordon can be cleared by an operator or healthy heartbeats.
- `LAST_SEEN` is the last heartbeat time in UTC.
- `DRAINED` shows the drain flag from the controller's node view.

There is no `ONLINE` column in the table. Use `lu nodes --json` to see the full controller view:
`online` (seen within two lease periods; nodes are offline after a controller restart until their
next heartbeat), `draining`, `drain_origin` (`"operator"` or `"agent"` while drained, absent otherwise),
`providers`, `provider_models`, `labels`, `label_sources`, `taints`,
`current_job`, `cordon_details`, `version` and the opaque `reiki` document. `label_sources` maps a
probed label to `probed`; other labels have no entry. Each `cordon_details` entry carries its
provider, source, reason, set time, expiry or healthy-lift count.

A node with no current job and `harness: idle` can take work. A node that is `busy`, draining, has a held job, is missing from the list, lacks a required label, reports a taint the job does not tolerate, or (when the job names a provider) cannot serve any of its provider/model choices, including a matching provider cordon, will not be assigned one. `current_job` stays set while the job is `recovering`, so a node that lost a lease is still shown as holding that job and cannot take another.

## Reading `lu node`

`lu node <name>` prints the latest host and job stats the node reported, then its cordon details from the controller's node view. The controller keeps only the most recent heartbeat's stats in memory, so they survive neither a controller restart nor an agent restart until the next heartbeat.

```text
node  node-a
labels gpu php8.3 (probed)
load1             0.50
load5             0.25
load15            0.12
mem_total_kb      8192
mem_available_kb  4096
swap_used_kb      12
disk_free_bytes   10737418240
disk_total_bytes  53687091200
disk_free_percent 20
uptime_secs       600
workspace_bytes              3221225472
workspace_count              7
workspace_reclaimed_bytes_total 10737418240
workspace_evictions_total    3
workspace_delete_errors_total 0

job 01J...  pgid 4242  output_idle 400s  cpu_idle 350s  stuck yes
  PID   PPID  STATE  CPU_S  RSS_KB  AGE_S  FILES  COMMAND
  4242  1     S      7      1024    90     3      opencode run

cordons
  zen [failure] 1m10s ago; 10m0s left
      Provider zen failed: timeout
  mistral [reiki] 20s ago; lifts on 2 healthy
      Reiki cooldown for mistral
```

- `labels` lists the node's current labels; a probed one is followed by ` (probed)`. `lu node <name> --json` includes `labels` and `label_sources` in the stats body.
- Load, memory and uptime come from the agent's `/proc` sample. `disk_free_bytes`, `disk_total_bytes` and `disk_free_percent` describe the filesystem holding the agent's `work_dir`; each is `-` when the agent cannot read it.
- The five `workspace_*` rows are the node's latest workspace counters. They describe agent-local workspaces under `work_dir`, not controller artifacts; see [workspaces.md](workspaces.md).
- Each job is a running job owned by that node. `pgid` is the harness process group, and the process table is capped at the 64 busiest members. `COMMAND` is the first 80 characters of the command line with control characters replaced.
- `output_idle` is seconds since the harness last wrote output; `cpu_idle` is seconds since any process in the job tree last used CPU. `stuck yes` means both are above 300 seconds while the job is still running.
- The `cordons` section lists each active provider with its source, how long ago it was set and
  either its remaining time or its healthy-heartbeat lift count. The indented line is the stored
  reason. `-` means no active cordons.
- A node that has not sent stats prints `host  -` and `jobs  -`. An unknown node is a 404 and exits `1`.
- `lu node <name> --json` prints the exact `GET /v1/nodes/{name}/stats` body, including an empty `{}` for a known node with no stats.
- `lu node <name> --workspaces` prints the agent's oldest-first `JOB`, `STATE`, `BYTES`, `AGE` and `POLICY` listing. It uses the node view, so it also comes from the last heartbeat.

Stats are sampled on the agent at heartbeat time, which may be slower than `lu top`'s two-second refresh. They describe the agent's view, not live controller state.

### Workspace lifecycle

The agent owns job workspaces under `[agent] work_dir`. Policies, TTLs and disk pressure settings are in [workspaces.md](workspaces.md); `[workspace]` is a top-level config table and unknown keys remain errors. Ephemeral workspaces wait for the controller to confirm that the final patch and report are stored and that no active dependant needs the tree; `keep_on_failure` and `keep` use their TTLs. Eviction is oldest first, never touches a running workspace, and retries failed deletions with backoff.

Inspect and clean from an operator host:

```sh
lu node node-a --workspaces
lu node node-a clean --all
lu node node-a clean --job build-api
```

`--job` accepts a job id or idempotency key. `clean` queues a `clean_workspaces` event for the next agent poll; it returns `queued: 1` for the request, and the agent logs how many workspaces it actually queued, refused as running, or could not find. Watch `workspace_delete_errors_total` and the agent log after a clean; a non-zero error counter means deletions are still being retried.

### Probe labels

An operator can declare `[[agent.detect]]` entries so a node advertises a label only while a
command succeeds. The agent runs each probe at start and then every `every_secs`, off the
heartbeat path; heartbeat reads the last result. Exit 0 publishes the label, and any other result,
a timeout or a spawn failure removes it with one log line on change. A probe runs as the agent
account with only `PATH`, `HOME` and `LANG`, null stdin, discarded output capped at 4 KiB and its
own process group, killed on timeout. It never overlaps itself.

```toml
[[agent.detect]]          # Rust
label = "rust/cargo"
command = ["cargo", "--version"]

[[agent.detect]]          # Go
label = "go/toolchain"
command = ["go", "version"]

[[agent.detect]]          # PHP
label = "php/cli"
command = ["php", "-v"]
```

`lu nodes` and `lu node <name>` mark these labels with ` (probed)`; the JSON views carry the same
information in `label_sources` as `{ "rust/cargo": "probed" }`. Probe labels are merged with
static and file labels under the 32-label cap, and are dropped last when the cap is full.

## Disk housekeeping

Long-running harness builds can leave gigabytes behind in directories outside the agent's
`work_dir`. An optional `[agent.housekeeping]` table removes those directories between jobs.
Workspaces inside `work_dir` are handled by the separate [workspace lifecycle](#workspace-lifecycle):

```toml
[agent.housekeeping]
scratch_globs = ["/srv/work/target-*"]
min_age_hours = 6
min_free_percent = 15
max_remove_per_pass = 10
```

A pass runs at agent start and after each finished job, never while a job is running, and at
most once every 10 minutes. It acts only when the filesystem holding a pattern's parent is
below `min_free_percent`, and stops when free space is back above `min_free_percent + 5` or
`max_remove_per_pass` directories have been removed. `min_free_percent = 0` always acts on
entries old enough by `min_age_hours`.

Safety rules:

- Only directories matching the final path component of a `scratch_globs` pattern are
  candidates. Only `*` and `?` are supported, and only in that final component.
- An entry is removed only if it is a direct child of the pattern's canonical parent and its
  whole tree's newest modification time is at least `min_age_hours` old.
- Symlinks are never followed or removed, files are never removed, and the pattern's parent
  is never removed.
- The agent's `work_dir`, and anything inside it, is never removed.
- Removal is oldest first. Each removal logs the path, a size estimate and the age at info;
  the pass logs a summary.
- Every error is logged or ignored. Housekeeping never fails a job or stops the agent.

## Display

`lu top` shows the controller and agent versions in its title (`lu top <version> · controller <version>`). The node table includes `VERSION`, `LOAD` (`load1`, one decimal or `-`), `MEM` (used percent of total, or `-`), `WORKSPACES` (`count/bytes`) and `WS TOTALS` (`reclaimed/evictions/errors`). Memory is green below 80%, orange at 80%, and red at 95%. A node with `stuck > 0` shows a red `stuck` marker after its state, and a node whose version differs from the controller's is coloured orange.

The Jobs table puts `REPO` after `NODE` and shows only `owner/name`: schemes, credentials, hosts, query strings and fragments are dropped, and unusable values become `-`. The selected job's graph detail line shows the normalised repo and its full ref. `JOB`, `STATE`, `NODE`, `REPO`, `TRY` and `AGE` keep their minimum widths; `NOTE` shrinks first, then `TITLE`, then `REPO`, and clipped text ends in `…`.

Node `STATE` badges use amber for busy, green for idle, blue for drained and red for offline or stuck. Job `STATE` badges use green for running, yellow for queued, red for failed, dim green for done and grey for cancelled. Badge colours are named tokens with contrasting foregrounds for light and dark terminals; selection is a bold underline, so it stays visible over a badge. With `NO_COLOR` set, colours are dropped and filled badges fall back to bold reverse; without 24-bit colour, named ANSI colours plus bold reverse are used.

On terminals at least 12 rows tall, a one-row strip replaces the tall gauge panels: `RUN`, `QUE`, `BUSY` and `PROV` show the latest value and a trend (`↑`, `↓`, `→`, or `·` with one sample), separated by `│`. When the job list is short, a dim summary line fills the space below it with totals by state and busy nodes over the total.

With the job list focused and a running job selected, `x` opens a process pane under the lists. It shows the selected job's three liveness counters (output idle, CPU idle, uploader behind) and its process tree (`PID`, `STATE`, `CPU_S`, `RSS`, `AGE`, `COMMAND`) from `GET /v1/nodes/{name}/stats`, refreshed every two seconds while open. States `D` and `Z` are red; both idle counters above 120 seconds are orange, and a stuck job is red. The pane also draws load and memory-used sparklines for that node from the dashboard's recent samples. `Esc` or `x` closes the pane. `--once` prints the same node columns as plain text.

`lu top` also reports the number of dependents waiting on a failed or cancelled predecessor: when the count is non-zero, the title adds `· waiting <count>` after the controller version, matching the `lu_blocked_dependents_total` metric. See [Dependency chains and blocked dependents](#dependency-chains-and-blocked-dependents).

`lu top` draws the dependency graph in a bordered `Graph` panel below the job list when the terminal is at least 24 rows tall and a component has an unfinished job. Press `g` to hide or show it. A component is a set of jobs linked by `after`, including finished jobs that connect unfinished ones; a component whose jobs are all terminal is left out. The panel gets at most a third of the terminal height and at least three content rows; it clips long graphs instead of pushing the other panes.

Each chain is one row, prerequisites first, for example `○ build ──▶ ○ test ──▶ ◔ ship`. Nodes show a state glyph and the last five id characters; an edge takes the prerequisite's colour. Only a real fan-out or fan-in adds rows: children stack to the right of their parent with `├─▶`/`└─▶` branches and `┐`/`┘` joins. An `after` id that is not in the fetched jobs is a dim stub `#id…` at the left of its chain. Unrelated single-job components share a row when they fit. The dashboard omits task titles from nodes; the graph panel's bottom border shows the selected job's full id, state and clipped title instead.

`Tab` cycles focus through Nodes → Jobs → Graph and `Shift+Tab` moves back; the focused pane's border is highlighted. The Jobs table and the graph share one selected job, so highlighting a job in either pane highlights it in the other (a job outside the visible graph window has no graph highlight). With Graph focused, `Left`/`h` and `Right`/`l` move to the first parent or child, `Up`/`k` and `Down`/`j` move to the previous or next sibling at the same depth and then to an adjacent component, and `Home`/`End` jump to the first or last drawn node; movement stops at an edge. `Enter` opens the selected job's log from either Jobs or Graph. `d` still drains only in Nodes focus. The footer lists the keys for the current focus.

Mouse capture is enabled only while `lu top` runs and is released on exit, errors and panics, leaving terminal copy and paste outside the TUI untouched. A left click on a graph node selects it and focuses Graph; a left click on a Jobs row selects that job and focuses Jobs; a left click on a Nodes row selects that node. A double click on a node or job opens its log. The wheel scrolls the focused table by three rows, or the graph window by three graph rows when the pointer is over the graph.

| State | Glyph | Colour |
| --- | --- | --- |
| `done` | `✓` | dim green |
| `running`, `leased` | `▶` | bright green |
| `blocked` | `◌` | orange |
| `queued`, `recovering`, `needs_input` | `○` | yellow |
| `failed` | `✗` | red |
| `cancelled` | `–` | grey |

The job list uses the same colours. The graph shows at most 6 components and 40 jobs; the panel title adds counts such as `+3 jobs, 1 component` when any were left out, and `+N rows` when a tall graph is clipped.

`lu graph [JOB]` prints the same horizontal graph without the panel, keeping task titles clipped to 30 characters and using up to 200 jobs from the controller. `JOB` selects the component containing that job, even if every job in it is finished. Output is coloured only when stdout is a terminal and `NO_COLOR` is unset. `lu graph --json` prints `{"nodes":[{"id","state","title","after"}]}` for the shown jobs, without stubs or the summary line.

## Repository mirrors

Mirror configuration, sync rules and the `lu repos` / `lu sync` commands are in [config.md](config.md) and the [client guide](client-guide.md#command-map); route details are in [api.md](api.md).

## Troubleshooting

For symptom-first checks, log and artifact problems, and HTTP error codes, see [troubleshooting.md](troubleshooting.md). Limits and route details are in [api.md](api.md), config keys and validation rules in [config.md](config.md), and the threat model in [security.md](security.md).
