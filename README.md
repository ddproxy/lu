# lu

Lu is a small job queue for finite coding-agent work against git repositories. An operator
submits a repo, ref and task; the controller stores jobs in SQLite and hands out leases. An agent
stages a fresh checkout and runs the `opencode` harness, then uploads `diff.patch`, `run.log` and
`report.json`. One `lu` binary is the controller, the operator CLI and the agent. Agents dial out
to the controller, which never connects back to them.

## When to use it

| Situation | Lu fits when | Look elsewhere when |
| --- | --- | --- |
| One-off code changes | You want a repeatable run against a git ref and a reviewable diff | You need a shared interactive terminal |
| Chained work | Later jobs wait for earlier ones and stack their diffs | Dependencies form cycles or must run everywhere |
| Several agents | Nodes advertise labels and you want exact routing, leases and cordons | You need broadcast or long-running services |
| Automation | You want an HTTP API or MCP tools to submit and inspect jobs | You need a hosted UI or managed workers |

Build with `make build` (release binary with the `tui` feature). `make install` puts `lu` in
`$(PREFIX)/bin`, defaulting to `/usr/local` for root and `$HOME/.local` otherwise; `PREFIX` and
`DESTDIR` can be overridden. Root installs also install the `contrib/` systemd units.

## Quickstart

This runs a controller, one agent and the operator CLI on one machine. Prerequisites: `make`, a
Rust toolchain, `git`, `opencode` on `PATH` (or a `harness_wrapper` that starts it), a local git
repository, and three shells. Replace `/home/you` with your home directory and
`/home/you/src/demo` with your repository path. The commands use the binary from `make build`;
put `target/release` on `PATH` or run `make install` first.

1. Make a demo directory, then create one agent token and one operator token under it. Keep the
   plaintext token files: the controller stores only their SHA-256 hashes. The exact commands,
   file checks and ownership rules are in [Tokens and TLS](docs/tokens-and-tls.md).

   ```sh
   mkdir -p "$HOME/lu-demo"
   ```

2. Write `$HOME/lu-demo/lu.toml`, replacing the two hash placeholders with the values printed
   for the token files. The agent's `work_dir` is created with mode `0700` on first start if it
   does not exist; if it does exist, it must be a real directory, not a symlink.

   ```toml
   [controller]
   listen = "127.0.0.1:7600"
   db = "/home/you/lu-demo/lu.db"
   artifacts_dir = "/home/you/lu-demo/artifacts"

   [[controller.agents]]
   name = "node-a"
   token_sha256 = "<64 lowercase hex chars from agent.token>"

   [[controller.operators]]
   name = "ops"
   token_sha256 = "<64 lowercase hex chars from operator.token>"

   [agent]
   name = "node-a"
   controller_url = "http://127.0.0.1:7600"
   token_file = "/home/you/lu-demo/agent.token"
   work_dir = "/home/you/lu-demo/work"
   harness = "opencode"

   [log]
   level = "info"
   format = "text"
   stderr = true
   ```

3. In shell 1, check the config and start the controller:

   ```sh
   lu check --config "$HOME/lu-demo/lu.toml"
   lu serve --config "$HOME/lu-demo/lu.toml"
   ```

   `lu check` prints `controller: listen 127.0.0.1:7600, 1 agent(s), 1 operator(s), tls off`
   and `agent: node-a -> http://127.0.0.1:7600, work dir /home/you/lu-demo/work`. It prints no
   database line until the database exists.

4. In shell 2, start the agent. A successful start loads the agent token, heartbeats and polls
   the controller:

   ```sh
   lu agent --config "$HOME/lu-demo/lu.toml"
   ```

5. In shell 3, point the operator CLI at the controller and submit a job:

   ```sh
   export LU_URL=http://127.0.0.1:7600
   export LU_TOKEN_FILE="$HOME/lu-demo/operator.token"

   lu submit --repo /home/you/src/demo --ref main --task "run the test suite" --wait
   ```

   Expected output is a 26-character job id on stdout, then `state: done` on stderr. The command
   exits `0` for `done` and `3` for `failed` or `cancelled`. Without `--wait`, the id is printed
   immediately and the job stays `queued` until the agent picks it up.

6. Inspect the result:

   ```sh
   lu ls
   lu show <job-id>
   lu logs --follow <job-id>
   mkdir -p results
   lu fetch <job-id> --out results
   ```

   `lu ls` uses the header `ID STATE NODE ATTEMPT AFTER CREATED`. `lu show` prints key/value
   rows (`id`, `state`, `node`, `attempt`, `epoch`, `created_at`, `started_at`, `finished_at`,
   `failure`, `waiting`, `summary`). `lu logs --follow` streams stored output and stops when the
   job reaches a terminal state. `lu fetch` prints one `saved <path>` line per file:
   `results/diff.patch`, `results/run.log` and `results/report.json`.

7. To chain work, use idempotency keys instead of copying ids:

   ```sh
   lu submit --key build --repo /home/you/src/demo --ref main --task "build"
   lu submit --key test --after-key build --repo /home/you/src/demo --ref main --task "test"
   ```

   The second job stays `blocked` while `build` is unfinished, then becomes `queued`. By default
   its agent applies the first job's `diff.patch` to its own base tree before starting;
   `--no-stack` disables that. See [Dependencies](docs/dependencies.md).

A controller listening on a non-loopback address requires a `[tls]` table; plain HTTP is accepted
only on loopback. See [Tokens and TLS](docs/tokens-and-tls.md).

## Commands

Global flags: `--config <path>` (default `lu.toml`), `--json` (print the raw controller JSON where the command supports it).

| Command | What it does |
| --- | --- |
| `lu check` | Read the config file's structure and print a controller or agent summary. When `[controller] db` exists, it prints the database's `quick_check`, schema, size and consistency counts. It does not read TLS keys or token files. |
| `lu backup --out <file> [--force]` | Copy the live controller database into `<file>` with `VACUUM INTO`, verify it and set mode `0600`. Existing files need `--force`. |
| `lu restore <file>` | Install a backup as the controller database after stopping the controller. The previous file is moved to `<db>.pre-restore-<unix seconds>`. |
| `lu status` | Show controller version, uptime, schema version, database size, recovered jobs, job counts by state, oldest queued age and node counts. `--json` prints the raw response. |
| `lu mcp` | Serve Lu tools over stdio using newline-delimited JSON-RPC 2.0. See [Use Lu from an agent](#use-lu-from-an-agent). |
| `lu repos` | List the configured repository mirrors with their last sync time and branch heads. `--json` prints the raw response. |
| `lu sync <name>` | Fetch one configured repository mirror and print its last sync time and branch heads. `--json` prints the raw response. |
| `lu serve` | Run the HTTP(S) controller. This is the default when no subcommand is given. |
| `lu top [--once]` | Live view of the nodes and recent jobs, refreshed every two seconds. `--once` prints one snapshot as text and exits; global `--json` prints one snapshot as JSON and cannot be combined with `--once`. Only in a build with `--features tui`. |
| `lu agent` | Run the node agent loop. It needs an `[agent]` config. |
| `lu submit` | Submit a job. `--repo` and `--ref` are required. The task comes from `--task`, `--task-file` or stdin. Flags: `--harness` (default `opencode`), `--provider`, `--model`, `--timeout` (default `3600`), `--idle-timeout` (1 to 86400 seconds), `--after`, `--after-key`, `--no-stack`, `--require`, `--fallback`, `--key` and `--wait`. `--fallback provider:model` is repeatable, at most 4; a fallback with only a provider keeps the job's model. `--after` and `--after-key` are repeatable, at most 16 dependencies in total; a key resolves to the newest job submitted with it. `--require` is repeatable, at most 8 labels. `--wait` polls until the job is terminal. |
| `lu retry <id>` | Resubmit a finished job's spec as a new job. `--provider` and `--model` replace those fields; `--after` and `--after-key` replace the dependency list; `--require` replaces the label requirements and `--no-require` clears them; `--fallback` replaces the provider choices and `--no-fallbacks` clears them. `--key` gives the new job an idempotency key, and `--wait` polls until it is terminal. A nonterminal job or an unknown `--after-key` is a usage error. |
| `lu ls [--state <state>] [--limit <n>]` | List jobs, newest first. `--state` accepts the nine job states. `AFTER` counts dependencies still unfinished. |
| `lu show <id>` | Show one job, including each dependency and its state. |
| `lu graph [job]` | Draw the dependency graph of unfinished jobs, newest component first. With a job id, show only its component. `--json` prints a `nodes` array instead. |
| `lu logs <id> [--follow]` | Print a job's stored log. `--follow` reads until the job reaches a terminal state. |
| `lu fetch <id> [--out <dir>] [--force]` | Save `diff.patch`, `run.log` and `report.json` into `--out` (default `.`). Missing artifacts are skipped; existing files need `--force`. |
| `lu cancel <id>` | Ask the controller to cancel a job. |
| `lu nodes` | List known nodes, including the agent version and labels each last reported. |
| `lu node <name>` | Show the host and job stats the node last reported. |
| `lu drain <node>` | Stop assigning new jobs to a node. The node must have heartbeated at least once. |
| `lu undrain <node>` | Allow a drained node to take jobs again. The node must have heartbeated at least once. |

Operator connection settings are read in this order: `LU_URL` with `LU_TOKEN_FILE` and optional `LU_CA_FILE`; the `[agent]` table in the `--config` file when that file exists; then the first existing operator file at `$XDG_CONFIG_HOME/lu/operator.toml` (or `~/.config/lu/operator.toml`) or `/etc/lu/operator.toml`. An operator file uses `url`, `token_file` and optional `ca_file`; a leading `~/` is expanded. The token file must be private (mode `0600` or tighter).

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

## Documentation

- [Concepts](docs/concepts.md)
- [Job Lifecycle](docs/lifecycle.md)
- [Dependencies](docs/dependencies.md)
- [Tokens and TLS](docs/tokens-and-tls.md)
- [Configuration](docs/config.md)
- [HTTP API](docs/api.md)
- [Operations](docs/operations.md)
- [Security](docs/security.md)
