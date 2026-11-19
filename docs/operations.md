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
| Provider and label dispatch | [dispatch.md](dispatch.md) |
| MCP tools | [mcp.md](mcp.md) |
| HTTP routes and limits | [api.md](api.md) |
| Configuration keys | [config.md](config.md) |

## Upgrade

1. Read `CHANGELOG.md` and back up the database and artifacts ([below](#backup-and-restore)).
2. Stop the controller: `systemctl stop lu-controller.service`.
3. Replace `/usr/local/bin/lu` and any changed unit files, then `systemctl daemon-reload`.
4. Run `lu check --config /etc/lu/lu.toml` to validate config values and database health, then start the controller.
5. Check `/v1/health`, `journalctl -u lu-controller`, `lu ls` and `lu nodes`. Full config and secret-file validation happens when the controller starts.

The store has schema version 6. It creates or verifies the schema at startup and refuses a database whose schema version is newer than the binary supports. Schema 5 adds the nullable `assigned_provider` and `assigned_model` job columns and schema 6 adds `diff_bytes`, which backs the `no changes` note; an older database upgrades on open with those columns empty. Downgrades are not supported; roll back by restoring the matching database backup as well as the older binary.

There is no online config reload. `SIGHUP` is not handled; restart the controller after changing config, TLS files or token hashes. For the `HOME` and `PATH` settings needed by the contributed unit, see [Running the controller under systemd](install.md#running-the-controller-under-systemd).

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
foreign SQLite file would replace your data with nothing), rejects a schema newer than the
binary supports, and refuses when the database is still in use, so stop the controller first.
Only one restore runs at a time. It stages the copy beside the database with the old file's
owner and mode `0600`, moves the current database and every `-wal`, `-shm` and `-journal` file
to `<db>.pre-restore-<unix seconds>` names, then renames the staged copy into place. Run it as
root or as the controller's own account; either way the installed file keeps the old owner.
Boot recovery then applies to jobs that were leased or running in the snapshot.

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

## What a controller restart does

Legal state transitions and the recovery state machine are in [lifecycle.md](lifecycle.md); this section covers the procedure around a restart. On startup the controller runs boot recovery before serving traffic:

- Every job in `leased` or `running` becomes `recovering`. Its node and epoch are kept.
- Jobs already in `recovering` stay there, but their grace timer is restarted. Lease and recovery timestamps are based on a monotonic clock that begins again with the process, so the old values are not meaningful after a restart.
- An agent that still holds the lease and renews the same epoch within `recovery_grace_secs` moves the job back to `running` and continues.
- If the grace period expires first, a node-class failure is applied. If `attempt <= max_retries`, the job returns to `queued`, the attempt is incremented, the node is cleared, and `failure_class`, `cancel_requested` and `finished_at` are cleared. Otherwise it becomes `failed` with `failure_class: "node"`.
- `queued`, `blocked`, `done`, `failed` and `cancelled` jobs are not changed by boot recovery.

The listener stops accepting new connections on `SIGTERM` or Ctrl-C and drains in-flight connections for up to 10 seconds. The contributed unit allows 40 seconds before systemd sends `SIGKILL`.

If the controller is down for longer than a lease, agents cannot renew. When it returns, the same recovery path applies; a job can therefore consume a retry even though the harness itself did nothing wrong.

## Provider failure detection

Failure classes, retry budgets and cancellation are in [lifecycle.md](lifecycle.md#failure-classes-and-retries); provider choice and fallbacks are in [dispatch.md](dispatch.md). Two agent-side detections matter when reading a stalled job.

When a job resolves to a `provider` (the assigned choice, or the primary when the job names one), the agent also checks it against its node's Reiki reading every five seconds. Reiki is the authority on keys, so this works for any harness: when two fresh readings in a row show the provider cooling or disabled (no usable key), the agent stops the job like a cancel and completes it as `Failed(Provider)`, with a summary that names the provider and the soonest return time Reiki reported. The completion carries that time plus a 60-second buffer as `retry_after_secs`, and the controller cordons that provider on the node for that long instead of the configured cordon length. Two consecutive healthy provider heartbeats after the failure clear that cordon; a non-healthy reading resets the streak.

As a fallback for a job with no provider, or a node without a Reiki feed, the agent watches a running harness's output. If the newest line matches a provider-unavailable marker (the opencode message about every key in the stream's chain, a quota message, a 429, or a rate limit) and the harness then produces no output for 120 seconds, the agent stops it the same way and completes the job as `Failed(Provider)`, with a summary naming the marker. Any other output after the marker disarms the watch.

## Provider cordons and node drains

These are separate mechanisms:

- **Drain** stops a node taking any new job. It is set by `POST /v1/nodes/{name}/drain` and cleared by `/undrain`, and is stored in the `nodes` table. It survives a controller restart. A node must have sent a heartbeat at least once to exist; draining an unknown node returns `404 not_found`.
- **Cordon** stops a node taking jobs whose chosen provider is cordoned. It is created when a retryable provider failure completes, keyed by the pair actually assigned rather than necessarily the primary, and it expires after `provider_cooldown_secs`. A completion with `retry_after_secs` marks the cordon Reiki-derived; two consecutive healthy provider heartbeats clear it early. A job with no provider is not blocked by a cordon.

Cordon deadlines are stored as monotonic milliseconds. The controller does not rebase them on restart, so a surviving cordon's remaining time is approximate and may last longer than the configured cooldown if the previous process ran for a long time. `lu nodes --json` shows the currently unexpired cordons; there is no CLI command to clear one. Wait out the cooldown, let a node report the provider healthy twice, or edit the database while the controller is stopped.

## Reading `lu nodes`

For the one-shot controller summary, see [`lu status` on the monitoring page](monitoring.md#lu-status). Human output for `lu nodes` has six columns:

```text
NAME    VERSION  HARNESS  LABELS                        LAST_SEEN            DRAINED
node-a  0.1.10   idle     !homelab,gpu,php8.3,region=eu   2026-11-16 09:12:00Z no
```

- `NAME` is the node name bound to the agent token.
- `VERSION` is the agent version from its latest heartbeat, or `-` for an older agent.
- `HARNESS` is the last reported harness state (`idle`, `busy`, or `-` if unknown).
- `LABELS` is the comma-separated label list from the latest heartbeat, with each taint shown first with a `!` prefix, or `-` for none. A label published by a probe is followed by ` (probed)`. A long list is cut to fit the column and ends with `+N` for the hidden entries.
- `LAST_SEEN` is the last heartbeat time in UTC.
- `DRAINED` shows the drain flag from the controller's node view.

There is no `ONLINE` column in the table. Use `lu nodes --json` to see the full controller view: `online` (seen within two lease periods), `draining`, `providers`, `labels`, `label_sources`, `taints`, `current_job`, `cordons`, `version` and the opaque `reiki` document. `label_sources` maps a probed label to `probed`; other labels have no entry.

A node with no current job and `harness: idle` can take work. A node that is `busy`, draining, has a held job, is missing from the list, lacks a required label, reports a taint the job does not tolerate, or (when the job names a provider) cannot serve any of its provider/model choices, including a matching provider cordon, will not be assigned one. `current_job` stays set while the job is `recovering`, so a node that lost a lease is still shown as holding that job and cannot take another.

## Reading `lu node`

`lu node <name>` prints the latest host and job stats the node reported. The controller keeps only the most recent heartbeat's stats in memory, so they survive neither a controller restart nor an agent restart until the next heartbeat.

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

job 01J...  pgid 4242  output_idle 400s  cpu_idle 350s  stuck yes
  PID   PPID  STATE  CPU_S  RSS_KB  AGE_S  FILES  COMMAND
  4242  1     S      7      1024    90     3      opencode run
```

- `labels` lists the node's current labels; a probed one is followed by ` (probed)`. `lu node <name> --json` includes `labels` and `label_sources` in the stats body.
- Load, memory and uptime come from the agent's `/proc` sample. `disk_free_bytes`, `disk_total_bytes` and `disk_free_percent` describe the filesystem holding the agent's `work_dir`; each is `-` when the agent cannot read it.
- Each job is a running job owned by that node. `pgid` is the harness process group, and the process table is capped at the 64 busiest members. `COMMAND` is the first 80 characters of the command line with control characters replaced.
- `output_idle` is seconds since the harness last wrote output; `cpu_idle` is seconds since any process in the job tree last used CPU. `stuck yes` means both are above 300 seconds while the job is still running.
- A node that has not sent stats prints `host  -` and `jobs  -`. An unknown node is a 404 and exits `1`.
- `lu node <name> --json` prints the exact `GET /v1/nodes/{name}/stats` body, including an empty `{}` for a known node with no stats.

Stats are sampled on the agent at heartbeat time, which may be slower than `lu top`'s two-second refresh. They describe the agent's view, not live controller state.

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
`work_dir`. An optional `[agent.housekeeping]` table removes those directories between jobs:

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

`lu top` shows the controller and agent versions in its title (`lu top <version> · controller <version>`). The node table includes `VERSION`, `LOAD` (`load1`, one decimal or `-`), and `MEM` (used percent of total, or `-`). Memory is green below 80%, orange at 80%, and red at 95%. A node with `stuck > 0` shows a red `stuck` marker after its state, and a node whose version differs from the controller's is coloured orange.

With the job list focused and a running job selected, `x` opens a process pane under the lists. It shows the selected job's three liveness counters (output idle, CPU idle, uploader behind) and its process tree (`PID`, `STATE`, `CPU_S`, `RSS`, `AGE`, `COMMAND`) from `GET /v1/nodes/{name}/stats`, refreshed every two seconds while open. States `D` and `Z` are red; both idle counters above 120 seconds are orange, and a stuck job is red. The pane also draws load and memory-used sparklines for that node from the dashboard's recent samples. `Esc` or `x` closes the pane. `--once` prints the same node columns as plain text.

`lu top` draws the dependency graph below the job list when the terminal is at least 24 rows tall and a component has an unfinished job. Press `g` to hide or show it. A component is a set of jobs linked by `after`, including finished jobs that connect unfinished ones; a component whose jobs are all terminal is left out.

Each row is one job, placed left to right by depth: `0` with no dependency in the fetched set, otherwise one past its deepest dependency. Box-drawing connectors join a job to its dependencies, and an edge takes the dependency's colour. An `after` id that is not in the fetched jobs is a dim stub `#id…`. Job titles are clipped to the panel width.

| State | Glyph | Colour |
| --- | --- | --- |
| `done` | `✓` | dim green |
| `running`, `leased` | `▶` | bright green |
| `blocked` | `◌` | orange |
| `queued`, `recovering`, `needs_input` | `○` | yellow |
| `failed` | `✗` | red |
| `cancelled` | `–` | grey |

The job list uses the same colours. The graph shows at most 6 components and 40 jobs, and a dim line reports how many jobs and components were left out.

`lu graph [JOB]` prints the same graph without the dashboard, using up to 200 jobs from the controller. `JOB` selects the component containing that job, even if every job in it is finished. Output is coloured only when stdout is a terminal and `NO_COLOR` is unset. `lu graph --json` prints `{"nodes":[{"id","state","title","after"}]}` for the shown jobs, without stubs or the summary line.

## Repository mirrors

Mirror configuration, sync rules and the `lu repos` / `lu sync` commands are in [config.md](config.md) and the [client guide](client-guide.md#command-map); route details are in [api.md](api.md).

## Troubleshooting

For symptom-first checks, log and artifact problems, and HTTP error codes, see [troubleshooting.md](troubleshooting.md). Limits and route details are in [api.md](api.md), config keys and validation rules in [config.md](config.md), and the threat model in [security.md](security.md).
