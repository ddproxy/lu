# Changelog

All notable changes are recorded here. The format follows Keep a Changelog, and the project
uses semantic versioning once it reaches 1.0.

## [Unreleased]

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
### Added

- Agents report optional host stats (load, memory, swap, uptime) and per-job process-tree stats (PID, parent, state, CPU, RSS, age, redacted command line, open files) in each heartbeat. The combined payload is capped at 16 KiB by dropping processes and then jobs; older heartbeats without the fields are still accepted.
- The controller keeps the latest stats per node in memory, exposes `GET /v1/nodes/{name}/stats`, adds `load1`, `mem_available_kb`, `mem_total_kb` and `stuck` to the node list, and clears a node's stats when its next heartbeat omits them.
- `lu node <name>` prints the host numbers and each running job's process tree as an indented table; `--json` prints the raw stats response.
- Job stats flag `stuck` when the harness has produced no output and the job tree has used no CPU for more than 300 seconds.

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
