# Quickstart: one shell, ten minutes

This brings up a controller, an agent and the operator CLI on one machine, submits a job and
fetches its artifacts. It uses one shell: the controller and agent run in the background and are
stopped at the end.

Ten minutes covers the commands below. Building `lu`, installing `opencode` and setting up a
working provider are not in the budget. Prerequisites before starting the timer:

- `lu` on `PATH`, or `target/release/lu` from `make build`;
- `git`, `curl`, `sha256sum`, `base64` and a POSIX shell;
- `opencode` on `PATH`, configured with a provider that can complete a small task;
- a local git repository with a commit on `main`.

Replace `/home/you/src/demo` with your repository path. Plain HTTP is accepted only on loopback;
a non-loopback `listen` needs a `[tls]` table. See [Tokens and TLS](tokens-and-tls.md).

## Time budget

| Step | Allow |
| --- | --- |
| Tokens and config | 2 min |
| Controller and agent ready | 2 min |
| Submit and wait | 4 min |
| Fetch and verify | 2 min |

## 1. Tokens and config

`umask 077` keeps the token files private. The controller stores only their SHA-256 hashes, so
keep the plaintext files.

```sh
set -euo pipefail
umask 077
LU="${LU:-$PWD/target/release/lu}"
if [ ! -x "$LU" ]; then LU=lu; fi   # use your PATH build when make build was not run here
DEMO="$HOME/lu-demo"
REPO="/home/you/src/demo"

mkdir -p "$DEMO"
printf '%s' "$(head -c 32 /dev/urandom | base64)" > "$DEMO/agent.token"
printf '%s' "$(head -c 32 /dev/urandom | base64)" > "$DEMO/operator.token"
AGENT_HASH=$(sha256sum "$DEMO/agent.token" | cut -d' ' -f1)
OPERATOR_HASH=$(sha256sum "$DEMO/operator.token" | cut -d' ' -f1)

cat > "$DEMO/lu.toml" <<EOF
[controller]
listen = "127.0.0.1:7600"
db = "$DEMO/lu.db"
artifacts_dir = "$DEMO/artifacts"

[[controller.agents]]
name = "node-a"
token_sha256 = "$AGENT_HASH"

[[controller.operators]]
name = "ops"
token_sha256 = "$OPERATOR_HASH"

[agent]
name = "node-a"
controller_url = "http://127.0.0.1:7600"
token_file = "$DEMO/agent.token"
work_dir = "$DEMO/work"
harness = "opencode"

[log]
level = "info"
format = "text"
stderr = true
EOF

"$LU" check --config "$DEMO/lu.toml"
```

Expected `lu check` output; the database line appears only after the first start:

```text
controller: listen 127.0.0.1:7600, 1 agent(s), 1 operator(s), tls off
agent: node-a -> http://127.0.0.1:7600, work dir /home/you/lu-demo/work
checks: config values
```

The agent creates `work_dir` recursively with mode `0700` at first start if it is missing. An
existing path must be a real directory, not a symlink; staging rejects `/` and bare top-level
directories.

## 2. Start the controller and agent

The health route needs no token. `lu nodes` does, so export the operator settings before running
it.

```sh
"$LU" serve --config "$DEMO/lu.toml" > "$DEMO/controller.log" 2>&1 &
CONTROLLER=$!

for i in $(seq 1 30); do
  curl -fsS http://127.0.0.1:7600/v1/health > /dev/null 2>&1 && break
  sleep 1
done
curl -fsS http://127.0.0.1:7600/v1/health

export LU_URL=http://127.0.0.1:7600
export LU_TOKEN_FILE="$DEMO/operator.token"

"$LU" agent --config "$DEMO/lu.toml" > "$DEMO/agent.log" 2>&1 &
AGENT=$!
trap 'kill "$AGENT" "$CONTROLLER" 2>/dev/null || true' EXIT

for i in $(seq 1 30); do
  "$LU" nodes | grep -q node-a && break
  sleep 1
done
"$LU" nodes
```

Expected health response and node row; times and version vary:

```text
{"db":"ok","ok":true,"version":"0.3.2"}
NAME    VERSION  HARNESS  LABELS  LAST_SEEN             DRAINED
node-a  0.3.2    idle     -       2026-11-21 14:22:00Z  no
```

If the node row does not appear, read `$DEMO/agent.log` and `$DEMO/controller.log`.

## 3. Submit and wait

`--wait` prints the id on stdout and the final state on stderr. The redirect captures the id for
the fetch step.

```sh
"$LU" submit --repo "$REPO" --ref main \
  --task "Run the repository's tests. Do not edit tracked files." \
  --timeout 300 --wait > "$DEMO/job-id"
JOB=$(cat "$DEMO/job-id")

"$LU" show "$JOB"
```

Expected submission output and the first rows of `lu show`:

```text
01M7Z7FMB95WRHQ790HK90EYM2   <- stdout, 26-character job id
state: done                  <- stderr
id                 01M7Z7FMB95WRHQ790HK90EYM2
state              done
node               node-a
assigned_provider  -
fallbacks          -
tolerates          -
attempt            1
epoch              1
created_at         2026-11-21 14:17:07Z
started_at         2026-11-21 14:17:07Z
finished_at        2026-11-21 14:17:07Z
failure            -
waiting            -
summary            <harness summary>
outcome_note       -
diff_bytes         <size of stored diff.patch>
```

`lu submit --wait` exits `0` for `done` and `3` for `failed` or `cancelled`. A connection, token
or config error exits `1`; a bad flag exits `2`.

## 4. Fetch and verify

```sh
mkdir -p "$DEMO/results"
"$LU" fetch "$JOB" --out "$DEMO/results"
for f in diff.patch run.log report.json; do test -f "$DEMO/results/$f"; done
ls -l "$DEMO/results"
```

Expected `lu fetch` output, in this order:

```text
saved /home/you/lu-demo/results/diff.patch
saved /home/you/lu-demo/results/run.log
saved /home/you/lu-demo/results/report.json
```

The files are written with mode `0600`. A missing artifact prints `skipped <name>: not found` on
stderr; `lu fetch` exits `0` if at least one artifact was saved and `1` if none were. It also
fetches artifacts of failed jobs, including a partial diff.

## 5. Stop the demo

```sh
kill "$AGENT" "$CONTROLLER" 2>/dev/null || true
wait "$AGENT" 2>/dev/null || true
wait "$CONTROLLER" 2>/dev/null || true
```

The controller stops accepting connections on `SIGTERM` and drains in-flight requests for up to
10 seconds.

## Exit codes

| Command | `0` | `1` | `2` | `3` |
| --- | --- | --- | --- | --- |
| `lu check` | Config is valid | Config or database error | — | — |
| `lu submit --wait` | Job `done` | API, connection or token error | Usage error | Job `failed` or `cancelled` |
| `lu logs --follow` | Job `done` | API, connection or token error | Usage error | Job `failed` or `cancelled` |
| `lu fetch` | At least one artifact saved | No artifacts, write or API error | Usage error | — |

## Limits of this demo

- The harness runs as your user and can read your agent token. See
  [What Lu does not protect against](limits.md).
- The task needs a working provider. A provider failure requeues while the attempt budget allows,
  then fails the job; `lu show` reports `failure provider`. See
  [Running the opencode harness](opencode-harness.md).
- There is no config reload. Restart `lu serve` after editing config or token hashes, and restart
  `lu agent` after editing its config.
