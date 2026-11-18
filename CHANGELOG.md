# Changelog

All notable changes are recorded here. The format follows Keep a Changelog, and the project
uses semantic versioning once it reaches 1.0.

## [Unreleased]

### Added

- Node labels and job requirements. Agents advertise up to 32 opaque labels from `[agent].labels` and `[agent].label_files` (re-read before each heartbeat); jobs require up to 8 with `JobSpec.requires` and dispatch gives a job only to a node that has every required label. `lu submit --require` and `lu retry --require`/`--no-require` set them, `lu nodes` shows a `LABELS` column, and `lu show`/`lu top` explain a stalled queued job as `no node with label <label>`. Submitting an unknown label succeeds with a warning. Metrics add `lu_node_labels_info{node,label}`. No schema change: labels live in the stored heartbeat.

## [0.1.14] - 2026-11-18

### Fixed

- A provider failure cordon created from Reiki's `retry_after_secs` now clears after two consecutive healthy provider heartbeats, so a stale outage estimate no longer leaves jobs queued for hours. Non-healthy readings reset the streak; output-marker cordons still wait out their timer. Store schema version is 3; older databases upgrade with their existing cordons unchanged.
- `lu check`, `lu backup` and `lu restore` read only `[controller] db` and no longer check or load TLS keys and token files, so they work when run as root against a service account's secrets. `lu serve` still validates the whole config and its secret files.

### Added

- `lu submit --after-key <key>` and `lu retry --after-key <key>` resolve a job's idempotency key to a dependency id before submitting. They are repeatable, combinable with `--after`, de-duplicated, and count towards the 16-dependency limit; an unknown key is a usage error (exit 2).
- `GET /v1/jobs?key=<key>` returns the newest job submitted with that key in the usual list envelope (zero or one job, `next: null`); an empty, over-long or malformed key gets `400 bad_request`. If legacy data holds several jobs with one key, the newest wins.

### Changed

- Schema 3 indexes the spec's idempotency key for the new lookup. Older databases gain the index on open.

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
