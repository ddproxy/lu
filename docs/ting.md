# `ting` reference

`ting` is the node agent. It also has a small set of self-service commands that use the node's own
config and agent token, so an operator token is not needed to drain a node for maintenance. The
HTTP side of these commands is documented in [api.md](api.md#agent); the upgrade sequence is in
[Updating a node](../README.md#updating-a-node).

## Synopsis

```text
ting [OPTIONS] [COMMAND]
```

With no subcommand, `ting` runs the agent loop. With `mcp`, it serves the credential-free scoped
MCP protocol over stdio for a job harness ([mcp.md](mcp.md#scoped-mcp-ting-mcp)). The other
subcommands talk to the controller using the node's token.

| Option | Meaning |
| --- | --- |
| `--config <PATH>` | TOML config file. Global; defaults to `/etc/ting/ting.toml`. An explicit path overrides it for every command. |

The config must have an `[agent]` table with `name`, `controller_url`, `token_file`, `work_dir` and
`harness`; `ca_file` is optional. `token_file` is read with the same private-file rules as the agent
loop (a regular file, owned by the agent account, mode `0600` or tighter). The token is sent as a
bearer credential and is never printed by any command. Configuration keys are in
[config.md](config.md#agent); the installed layout is in [install.md](install.md#agent).

## Commands

| Command | Meaning |
| --- | --- |
| `ting drain` | `POST /v1/agent/drain`. Stop assigning new jobs to this node. Idempotent; a running job continues and no new one is assigned. An operator-owned drain is left as it is. Prints nothing on success. |
| `ting undrain` | `POST /v1/agent/undrain`. Clear a drain this node started. Idempotent when not drained; an operator-owned drain is refused by the controller with a conflict error and stays in place. Prints nothing on success. |
| `ting status [--json]` | `GET /v1/agent/self`. Without `--json`, prints one line: `NAME: version=… draining=… current_job=… last_heartbeat_age_secs=… online=…` (`-` for a missing version or job). With `--json`, prints one compact JSON object as described below. |
| `ting wait-idle --timeout <DURATION>` | Poll `GET /v1/agent/self` once a second until the node is drained and has no current job. Exits `0` when idle. `--timeout` is required; the value is a positive integer in seconds, or an integer followed by `s`, `m` or `h` (for example `30`, `45s`, `2m`, `1h`). Exits `3` on timeout with `ting: timed out waiting for the node to become idle` on stderr. |
| `ting mcp` | Serve the scoped harness MCP protocol over stdio. Does not read the controller config or token. |
| `ting --help`, `ting <command> --help` | Print clap help and exit `0`. |

The node name is never accepted as an argument: all four self-service routes bind it to the token.
Unexpected query strings and body fields are refused with a `400 bad_request` from the controller.

## Stable `status --json` shape

The object has exactly these fields, in this order. `version` and `current_job` are `null` when
unknown; `last_heartbeat_age_secs` is a whole number; `draining` and `online` are booleans.

```json
{"name":"node-a","version":"0.5.1","draining":true,"current_job":null,"last_heartbeat_age_secs":2,"online":true}
```

No token, config path, filesystem path or other node's data appears in the output. The same shape
is the controller's `AgentSelfView`; see [api.md](api.md#agentselfview).

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success, including `wait-idle` reaching a drained and idle node. |
| `1` | Runtime or controller error: bad token file contents at request time, connection failure, HTTP error, timeout of a single HTTP request, or a failed encode/decode. |
| `2` | Config or usage error: missing/unreadable config, missing `[agent]` section, unreadable/empty token file, invalid controller URL, invalid `--timeout`, or a clap usage error. |
| `3` | `wait-idle` reached its `--timeout` before the node was both drained and idle. |

Errors print one line on stderr prefixed `ting: `. For example, a missing config reports
`ting: cannot read config <path>: …`; no token value is included in any error.

## Example: drain, wait, update, undrain

Run these on the node, with the service's own config and token available. A failed update leaves
the node drained so it does not take work in a half-updated state; clear it only after the health
check passes.

```sh
ting drain --config /etc/ting/ting.toml
ting wait-idle --timeout 10m --config /etc/ting/ting.toml
# stop the old service, replace the binary, start the new service
ting status --json --config /etc/ting/ting.toml   # check version, online, draining
ting undrain --config /etc/ting/ting.toml
```

From the operator host, `lu nodes --json` shows `drain_origin` alongside `draining`, so an operator
can see whether a drain belongs to the agent or to the operator. Only the operator routes can clear
an operator-owned drain.
