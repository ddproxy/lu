# lu

Lu is a small job queue for coding-agent work: an operator submits a repository, ref and task to a controller, and an agent on a peer checks out the repository, runs a harness under supervision, and uploads a diff, log and report. The controller keeps jobs in SQLite and serves a JSON API over HTTPS; one `lu` binary provides the controller, the operator CLI and, with the `tui` feature, a terminal dashboard. Agents dial out to the controller and poll for work; the controller never connects back to an agent.

## Architecture

```mermaid
flowchart LR
    operator["operator<br/>lu CLI"] -->|HTTPS| controller
    agent -->|"HTTPS poll"| controller
    controller --- store[("SQLite<br/>+ artifacts")]
    agent -->|"process group"| harness
```

The controller owns job state, leases, retry decisions and node cordons. An agent has one job slot in 0.1.0; it stages a fresh checkout, supervises the harness, and reports logs and artifacts back to the controller.

**Status:** controller, agent and operator CLI work end to end. A submitted job is leased to an agent, run by a harness under supervision, and finishes with a diff, a log and a report. See *Known gaps* below.

## Install

```sh
make build
make install
```

`make install` builds `lu` with the `tui` feature and installs it to `$(PREFIX)/bin`. `PREFIX` defaults to `/usr/local` for root and `$HOME/.local` otherwise; `DESTDIR` and `PREFIX` can be overridden. When run as root, `make install` also installs the systemd units from `contrib/` to `$(PREFIX)/lib/systemd/system`; `make uninstall` removes them again.

Configuration and operational details are in [docs/config.md](docs/config.md), [docs/operations.md](docs/operations.md) and [docs/security.md](docs/security.md). The HTTP contract is [docs/api.md](docs/api.md).

## Quick start

These steps run a controller and the operator CLI on one machine. Run them as your own user, not root. Replace `/home/you` with your home directory.

1. Generate one agent token and one operator token. The controller stores only their SHA-256 hashes, so keep the token files:

   ```sh
   umask 077
   mkdir -p "$HOME/lu-demo/artifacts"
   printf '%s' "$(head -c 32 /dev/urandom | base64)" > "$HOME/lu-demo/agent.token"
   printf '%s' "$(head -c 32 /dev/urandom | base64)" > "$HOME/lu-demo/operator.token"
   chmod 600 "$HOME/lu-demo/agent.token" "$HOME/lu-demo/operator.token"
   sha256sum "$HOME/lu-demo/agent.token" "$HOME/lu-demo/operator.token"
   ```

2. Write `$HOME/lu-demo/lu.toml`. Put the two hashes from `sha256sum` in the `token_sha256` fields:

   ```toml
   [controller]
   listen = "127.0.0.1:7600"
   db = "/home/you/lu-demo/lu.db"
   artifacts_dir = "/home/you/lu-demo/artifacts"

   [[controller.agents]]
   name = "node-a"
   token_sha256 = "<sha256 of agent.token>"

   [[controller.operators]]
   name = "ops"
   token_sha256 = "<sha256 of operator.token>"

   [log]
   level = "info"
   format = "text"
   stderr = true
   ```

3. Check the config, then start the controller in the foreground:

   ```sh
   lu check --config "$HOME/lu-demo/lu.toml"
   lu serve --config "$HOME/lu-demo/lu.toml"
   ```

4. In a second shell, point the operator CLI at the controller and submit a job from a local git repository:

   ```sh
   export LU_URL=http://127.0.0.1:7600
   export LU_TOKEN_FILE="$HOME/lu-demo/operator.token"

   lu submit --repo /path/to/repository --ref main --task "run the test suite"
   lu ls
   lu show <job-id>
   lu logs <job-id>
   lu cancel <job-id>
   lu retry <job-id> # resubmit a finished job's spec as a new job
   ```

   `lu submit` prints the new job id. With no agent running the job remains `queued`; `lu logs` is empty and `lu cancel` moves a queued job to `cancelled`.

   To chain jobs, repeat `--after` with the ids that must finish first:

   ```sh
   lu submit --repo /path/to/repository --ref main --task "run the linter" --after <job-id>
   lu ls   # AFTER shows how many dependencies are still unfinished, or - for none
   lu show <job-id>   # prints each dependency and its state
   ```

   The dependent job stays `blocked` until every dependency is `done`, then becomes `queued`. By default the agent applies each dependency's diff to the dependent's base tree in `after` order before the harness starts; the dependent's own diff includes those changes. If a dependency diff cannot be merged, the job fails with `failure_class: "conflict"` and names the conflicting paths. `--no-stack` runs on the base tree alone. If a dependency ends `failed` or `cancelled`, the dependent fails with `failure_class: "dependency"` and no diff is applied.

   A chain can use idempotency keys instead of capturing ids:

   ```sh
   lu submit --key build --repo /path/to/repository --ref main --task "build"
   lu submit --key test --after-key build --repo /path/to/repository --ref main --task "test"
   lu submit --key ship --after-key build --after-key test --repo /path/to/repository --ref main --task "ship"
   ```

   `--after-key` is repeatable, can be combined with `--after <job-id>`, and drops duplicate ids. An unknown key is a usage error (exit 2); a key naming a failed or cancelled job hits the same dependency rule as its id.

   To pin work to nodes that advertise every named label, repeat `--require`:

   ```sh
   lu submit --repo /path/to/repository --ref main --task "run the PHP suite" --require php8.3 --require gpu
   ```

   Labels are opaque to Lu: an agent advertises them, a job requires them, and dispatch compares them exactly. A submit with a label no online node advertises still succeeds and prints a warning; a queued job that cannot be placed shows why in `lu show` and `lu top`.

5. Run an agent on the same machine. Write `$HOME/lu-demo/agent.toml` with an `[agent]` table (`name = "node-a"`, `controller_url = "http://127.0.0.1:7600"`, `token_file` pointing at `agent.token`, a `work_dir` under `lu-demo`, `harness = "opencode"`), then:

   ```sh
   lu agent --config "$HOME/lu-demo/agent.toml"
   ```

   In the other shell, `lu submit ... --wait` returns when the job is done, `lu logs --follow <job-id>` streams its output, and `lu fetch <job-id> --out results` saves the diff, log and report.

For a controller on a non-loopback address, add a `[tls]` table with `cert` and `key`; plain HTTP is accepted only on loopback. See [docs/config.md](docs/config.md).

## Commands

Global flags: `--config <path>` (default `lu.toml`), `--json` (print the raw controller JSON where the command supports it).

| Command | What it does |
| --- | --- |
| `lu check` | Parse and validate the config and TLS files, print a summary, and report the controller database's `quick_check`, schema, size and consistency counts when it exists. |
| `lu backup --out <file>` | Copy the live controller database to `<file>` with `VACUUM INTO`, verify it, and set mode `0600`. Existing files need `--force`. |
| `lu restore <file>` | Install a backup as the controller database after stopping the controller; the previous file is moved to `<db>.pre-restore-<unix seconds>`. |
| `lu status` | Show controller version, uptime, schema, database size, recovered jobs, job counts, oldest queued age and node counts. `--json` prints the raw response. |
| `lu mcp` | Serve Lu tools over stdio (JSON-RPC 2.0), including job submission and control. See [Use Lu from an agent](#use-lu-from-an-agent). |
| `lu serve` | Run the HTTP(S) controller. This is the default when no subcommand is given. |
| `lu top` | Live view of the nodes (version, state, last heartbeat, load, memory, provider health) and recent jobs, refreshed every two seconds. `↑`/`↓` or `j`/`k` select a row, `Tab` switches focus between nodes and jobs, `Enter` opens the last 20 lines of the selected job's log, `x` opens the process pane for a running job selected in the job list, `Esc` closes the log or process pane (or quits when none is open), `d` twice within three seconds drains or undrains the selected node, `g` toggles the dependency-graph panel, `p` pauses and `q` quits. `--once` prints one snapshot's tables with the same columns as plain text and exits; `--json` prints one `nodes`/`jobs` object instead, and cannot be combined with `--once`. Only in a build with `--features tui`; connects like the other operator commands. |
| `lu submit` | Submit a job. `--repo` and `--ref` are required; the task comes from `--task`, `--task-file`, or stdin. `--idle-timeout <secs>` (1 to 86400) stops a harness that has produced no output for that long. `--after <job-id>` and `--after-key <key>` (each repeatable, at most 16 dependencies in total) make the job wait for those dependencies; a key resolves to the newest job submitted with it. `--wait` polls until terminal. |
| `lu retry <id>` | Resubmit a finished job's spec as a new job. `--provider <name>` and `--model <name>` replace those fields; `--after <job-id>` and `--after-key <key>` (each repeatable) replace the dependency list; nothing else changes. Refuses with exit 2 while the old job is not terminal (`done`, `failed` or `cancelled`) or an `--after-key` is unknown. The new job carries no idempotency key unless `--key` is given. Supports `--wait` and `--json`. |
| `lu submit` | Submit a job. `--repo` and `--ref` are required; the task comes from `--task`, `--task-file`, or stdin. `--idle-timeout <secs>` (1 to 86400) stops a harness that has produced no output for that long. `--after <job-id>` (repeatable, at most 16) makes the job wait for those dependencies; their diffs are applied in `after` order unless `--no-stack` is given. `--wait` polls until terminal. |
| `lu retry <id>` | Resubmit a finished job's spec as a new job. `--provider <name>` and `--model <name>` replace those fields; `--after <job-id>` (repeatable) replaces the dependency list; nothing else changes. Refuses with exit 2 while the old job is not terminal (`done`, `failed` or `cancelled`). The new job carries no idempotency key unless `--key` is given. Supports `--wait` and `--json`. |
| `lu top` | Live view of the nodes (state, last heartbeat, labels on wide terminals, provider health) and recent jobs, refreshed every two seconds. Queued jobs show why they cannot be placed in the note column. `↑`/`↓` or `j`/`k` select a row, `Tab` switches focus between nodes and jobs, `Enter` opens the last 20 lines of the selected job's log, `Esc` closes the log pane (or quits when it is closed), `d` twice within three seconds drains or undrains the selected node, `g` toggles the dependency-graph panel, `p` pauses and `q` quits. `--once` prints one snapshot's tables as plain text and exits; `--json` prints one `nodes`/`jobs` object instead, and cannot be combined with `--once`. Only in a build with `--features tui`; connects like the other operator commands. |
| `lu submit` | Submit a job. `--repo` and `--ref` are required; the task comes from `--task`, `--task-file`, or stdin. `--idle-timeout <secs>` (1 to 86400) stops a harness that has produced no output for that long. `--require <label>` (repeatable, at most 8) routes the job only to nodes advertising every label; unknown labels are accepted with a warning. `--after <job-id>` and `--after-key <key>` (each repeatable, at most 16 dependencies in total) make the job wait for those dependencies; a key resolves to the newest job submitted with it. `--wait` polls until terminal. |
| `lu retry <id>` | Resubmit a finished job's spec as a new job. `--provider <name>` and `--model <name>` replace those fields; `--after <job-id>` and `--after-key <key>` (each repeatable) replace the dependency list; `--require <label>` (repeatable) replaces the label requirements and `--no-require` clears them. Nothing else changes. Refuses with exit 2 while the old job is not terminal (`done`, `failed` or `cancelled`) or an `--after-key` is unknown. The new job carries no idempotency key unless `--key` is given. Supports `--wait` and `--json`. |
| `lu ls` | List jobs, newest first. Filters: `--state`, `--limit`. `AFTER` counts dependencies still unfinished. |
| `lu show <id>` | Show one job, including each dependency and its state. |
| `lu graph [job]` | Draw the dependency graph of unfinished jobs, newest component first. With a job id, show only its component. `--json` prints a `nodes` array instead; colour is used only when stdout is a terminal and `NO_COLOR` is unset. |
| `lu logs <id>` | Print a job's stored log. `--follow` keeps reading until the job is terminal. |
| `lu fetch <id>` | Save `diff.patch`, `run.log` and `report.json` into `--out` (default `.`). Missing artifacts are skipped; existing files need `--force`. |
| `lu cancel <id>` | Ask the controller to cancel a job. |
| `lu repos` | List the configured repository mirrors with their last sync time and branch-head counts. `--json` prints the raw response. |
| `lu sync <name>` | Fetch one configured repository mirror and print its last sync time and branch heads. `--json` prints the raw response. |
| `lu nodes` | List known nodes, including the agent version each last reported. |
| `lu nodes` | List known nodes, including the agent version each last reported and its labels. |
| `lu drain <node>` | Stop assigning new jobs to a node. |
| `lu undrain <node>` | Allow a drained node to take jobs again. |

Operator connection settings come from `LU_URL`, `LU_TOKEN_FILE` and optional `LU_CA_FILE`, or from `~/.config/lu/operator.toml`, then `/etc/lu/operator.toml` (`url`, `token_file`, optional `ca_file`; a leading `~/` is expanded) when `--config` names no file, or from the `[agent]` table in `--config`. The token file must be private (mode `0600` or tighter).

### Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success, or `submit --wait` / `logs --follow` ended in `done`. |
| `1` | Any error: bad config, connection or HTTP failure, no artifacts available, bad token file. |
| `2` | Command-line usage error (from clap), an unknown `--after-key`, or `lu retry` given a job that is not terminal. |
| `3` | `submit --wait` or `logs --follow` ended in `failed` or `cancelled`. |

## Use Lu from an agent

`lu mcp` is an MCP server over newline-delimited JSON-RPC on stdio. Register the command with a runtime that supports stdio servers, for example:

```sh
claude mcp add lu -- lu mcp
```

The server uses the operator connection settings above. A connection failure at startup is reported by each tool call as an `isError` result, so an agent can see why the controller is unavailable.

Read tools: `lu_status`, `lu_jobs`, `lu_job`, `lu_logs`, `lu_nodes`, `lu_graph`.

Write tools: `lu_submit`, `lu_submit_many`, `lu_result`, `lu_cancel`, `lu_retry`, `lu_drain`, `lu_undrain`.

- `lu_submit` takes `repo`, `ref` and inline `task` text, plus optional `provider`, `model`, `after`, `after_keys`, `timeout_secs`, `idle_timeout_secs` and `key`. When `key` is absent the server derives `mcp-` followed by 16 hex characters of the spec's SHA-256, so repeating the same submission is idempotent.
- `lu_submit_many` submits up to 12 named jobs in order. A job's `after_names` may refer only to earlier jobs in the same call; duplicate, unknown, forward or cyclic names are rejected before any request is sent. A failure partway through reports which jobs were submitted and which were not.
- `lu_result` returns a finished job's summary, report, diff stats and a page of the diff. It defaults to 24 KiB and accepts `offset` and `max_bytes` (at most 32 KiB); follow `next_offset` until `done` is true.
- Submissions (`lu_submit`, `lu_submit_many` and `lu_retry`) are limited to 30 per rolling minute per server process. Over the limit, the call returns `isError` with the number of seconds to wait.
- `lu_cancel`, `lu_drain` and `lu_undrain` are marked destructive; all write tools report `readOnlyHint: false`.

## Known gaps

- The agent runs the harness as its own user; see `docs/security.md`.

## Crates

| Crate | Purpose |
| --- | --- |
| `lu-config` | validated TOML config |
| `lu-auth` | hashed bearer tokens, constant-time match, per-address failure throttle |
| `lu-serve` | HTTP(S) listener with connection caps, request ids and drain on shutdown |
| `lu-tui` | terminal view (feature `tui`, off by default) |
| `lu-proto` | wire types: job spec, states, events, limits, output sanitising |
| `lu-store` | the controller's SQLite store: jobs, leases and fencing epochs, retries, idempotency, cordons |
| `lu-supervise` | harness supervision in a process group, with timeouts and capped output |
| `lu-staging` | per-job checkout, diff export, artifact caps, safe cleanup |
| `app` | the `lu` binary |

MIT. See `LICENSE`.
