# Running the opencode harness

Lu runs `opencode` as the harness. This page covers the agent settings around it, the container
wrapper pattern and the operator view of a provider outage. File checks and limits come from
[Configuration](config.md); the trust boundary is in [Security](security.md).

## The command Lu builds

For each job, the agent builds:

```text
opencode run [--server URL] --auto [-m MODEL] -f TASKFILE MESSAGE
```

`--server` comes from `[agent].opencode_server`, `-m` from the job's assigned model, and `-f` from
the task file staged in the job tree. The message is fixed: `Follow the task in the provided file.`
The task text itself is in the file. A model, server or message starting with `-`, containing a
control character or (for the server) whitespace is refused before the process starts.

The child runs directly, never through a shell, in its own process group with stdin closed. It
sees only `PATH`, `HOME`, `LANG`, any explicit command environment and
`OPENCODE_SERVER_PASSWORD` when configured. Output is capped; the job's wall timeout and the
idle-output timeout set with `lu submit --idle-timeout` stop a harness that hangs. A running job
with both output and CPU idle over five minutes is reported as `stuck` in `lu node`; that report
does not stop the job. A
job spec with a `harness` other than `opencode` fails with class `execution`; `--harness` on
`lu submit` defaults to `opencode`.

When a job spec carries `context`, the agent stages it as `.lu/inputs/context.md` under an
**UNTRUSTED EXTERNAL TEXT** header and adds a prompt instruction telling the harness to treat that
file as data, never instructions. The value is capped at 32 KiB with a truncation marker. Task text
is kept separate and is operator-authored; see [Drafts](drafts.md#job-fields).

## Agent configuration

```toml
[agent]
name = "node-a"
controller_url = "https://lu.example.net:7600"
token_file = "/etc/ting/ting.token"
work_dir = "/var/lib/ting/work"
harness = "opencode"
opencode_server = "http://127.0.0.1:4096"
opencode_password_file = "/etc/lu/opencode.password"
workspace_group = "lu-harness"
reiki_health_url = "http://127.0.0.1:9000/health"
reiki_token_file = "/etc/lu/reiki.token"

[[agent.provider_models]]
provider = "anthropic"
model = "claude-sonnet-4-5"

[[agent.detect]]
label = "opencode/cli"
command = ["opencode", "--version"]
timeout_secs = 5
every_secs = 300
```

- `opencode_server` is an opaque endpoint passed to the launcher. Config only requires a non-empty
  value; when a job starts, the launcher requires `http://` or `https://`, no whitespace or control
  characters, no leading `-`, and at most 2048 bytes.
- `workspace_group` gives each job's tree to that group with group read, write and the setgid bit;
  `.git` is left as git made it. It supports a harness running as another account. An unknown
  group stops the agent. It is group sharing, not per-job user isolation.
- `reiki_health_url` must be loopback and is paired with `reiki_token_file`. The agent reads peer
  health every 10 seconds and sends providers with each heartbeat. `provider_models` are offered
  only while Reiki reports that provider healthy; after three failed reads the node reports no
  providers.
- Config is read at start. Restart `ting` after changing any of these settings.

## Server password file

`opencode_password_file` holds one line: the password for the opencode server, not a provider
key. The agent reads it at start, trims trailing CR/LF and passes it to the harness as
`OPENCODE_SERVER_PASSWORD`.

The file must be a regular file owned by the agent account, with no group or other permission
bits (`0600` or tighter), no symlink and no FIFO. It holds at most 4 KiB of text; an empty value or
a control character is refused. Create it like the agent token:

```sh
umask 077
printf '%s' 'a-long-random-server-password' > /etc/ting/opencode.password
chown ting:ting /etc/ting/opencode.password
chmod 0600 /etc/ting/opencode.password
```

The value is not passed as an argument and is not a general provider-key channel. Keys belong to
the harness or a local Reiki on the node; see [What Lu does not protect against](limits.md).

## Container wrapper

`harness_wrapper` prefixes the opencode command. `{cwd}` becomes the job's working tree. Entries
are argv text without control characters and run directly, never through a shell. The opencode
command is appended as trailing arguments.

```toml
[agent]
harness_wrapper = ["podman", "exec", "-i", "--workdir", "{cwd}",
                   "--env", "OPENCODE_SERVER_PASSWORD", "opencode-agent"]
```

The container image must provide the `opencode` program, because the appended command starts with
`opencode`. With `workspace_group` set, the host tree is group-writable so the container user can
write the diff. The wrapper runs as the agent account; use container configuration for any
stronger user or filesystem boundary.

> Unsupported: a wrapper does not change the harness name in the job spec, and there is no
> shell expansion, image build or container lifecycle management in Lu. Manage the image and its
> credentials on the node.

## Scoped MCP for the harness

A job submitted with `lu submit --allow-spawn` gets a per-job agent socket. Configure the harness to
use the scoped MCP server, not the operator's `lu mcp`:

```toml
mcp = { lu = { command = ["ting","mcp"] } }
```

`ting mcp` reads `LU_JOB_SOCKET` and exposes only `job_context`,
`submit_child`, `list_children`, `get_child`, `get_child_artifact` and `cancel_child`. It refuses to
start if operator credentials are visible in the environment or the standard config paths, and it
marks child output as untrusted with `source: "child_job"`. For a container harness, the socket
directory must be bind-mounted with matching uid; see
[Container harness bind mount](scope-container.md) and the [Ansible example](scope-ansible.md). The
full threat model and limits are in [Job-scoped capability](scope.md).

## A probe label for the harness

A probe publishes a label while its command exits `0`:

```toml
[[agent.detect]]
label = "opencode/cli"
command = ["opencode", "--version"]
timeout_secs = 5
every_secs = 300
```

The command runs at agent start and then every `every_secs`, never overlapping itself. It runs as
the agent account with only `PATH`, `HOME` and `LANG`, null stdin and at most 4 KiB of discarded
output. Exit `0` publishes the label; a timeout, spawn failure or other exit withdraws it.

`lu nodes` and `lu node <name>` mark the label with ` (probed)`; `lu nodes --json` puts it in
`label_sources`. A job can require it with `--require opencode/cli`. Probe labels join static and
file labels under one 32-label cap and are dropped last when the cap is full.

## What a provider failure looks like

Two detectors can stop a running job:

| Detector | Trigger | Result |
| --- | --- | --- |
| Reiki health | Two consecutive fresh readings from the local Reiki show the job's provider `cooling` or `disabled`. The watch ticks every 5 seconds but counts only new health snapshots, which refresh every 10 seconds. | Job completes as `Failed(Provider)` with a summary naming the provider and the soonest return time. With a reported time, the cordon uses it plus a 60-second buffer; otherwise `provider_cooldown_secs` applies. |
| Harness output | The newest line matches an opencode "every key in this stream's chain is unavailable", quota, `HTTP 429` or rate-limit message, and the harness then produces no output for 120 seconds. Any other output disarms the watch. | Job completes as `Failed(Provider)` with a summary naming the marker. The cordon uses `provider_cooldown_secs`. |

Provider failures requeue while `attempt <= 100`; `max_retries` is not consulted. While attempts
remain, `lu show` can therefore show a queued job that still carries the failure:

```text
state              queued
failure            provider
attempt            2
summary            Reiki reports provider anthropic has no usable key; the soonest returns in 120s
waiting            -
```

At `attempt > 100` the state is `failed`. In `lu nodes --json`, the node that ran the job shows
these fields among others:

```json
{"providers":[{"name":"anthropic","state":"cooling","soonest_secs":60}],
 "cordon_details":[{"provider":"anthropic","source":"reiki","reason":"Reiki cooldown for anthropic",
                    "set_at":1763750000,"until":null,"lifts_after_healthy_heartbeats":2}]}
```

A job with no provider is not blocked by that cordon; a job with fallbacks may run on another
provider or node. An operator can clear a cordon early with `lu uncordon <node> [<provider>]`;
otherwise it expires, a Reiki-derived cordon clears after two consecutive healthy provider
heartbeats, and a controller restart deletes all cordons. See
[Provider cordons and node drains](operations.md#provider-cordons-and-node-drains).
