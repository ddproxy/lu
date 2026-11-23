# Migrating an agent host to Ting

Ting is the new name for the node agent. The controller (`lu serve`), operator CLI (`lu`) and the HTTP API are unchanged. This note is for an operator with an existing `lu agent` host. The rename touches the agent's binary, config, token, state directory and service account; it does **not** change the config schema, the `[agent]` table name, wire routes, node views, heartbeat `agent_version` field or the store schema baseline.

## What changed

| Old | New |
| --- | --- |
| `lu agent` subcommand / `lu-agent` binary | `ting` binary (run the agent loop) |
| `lu-agent mcp` | `ting mcp` (scoped harness MCP; `serverInfo.name` is `ting`) |
| `contrib/lu-agent.service`, unit `lu-agent.service` | `contrib/ting.service`, unit `ting.service` |
| `/etc/lu/agent.toml` | `/etc/ting/ting.toml` (the `[agent]` table is unchanged) |
| `/etc/lu/agent.token` | `/etc/ting/ting.token` |
| `/var/lib/lu/work`, `StateDirectory=lu` for the agent | `/var/lib/ting/work`, `StateDirectory=ting` |
| service account `lu` for the agent | service account `ting` |
| optional agent log under `/var/lib/lu` | `/var/lib/ting/ting.log` |
| crate `lu-agent` | crate `tuishou-ting` |

The controller keeps its own `/etc/lu/lu.toml`, `/etc/lu/operator.token`, `/etc/lu/tls/`, `/var/lib/lu/` and `lu` account. The harness helper account `lu-harness` and its home `/var/lib/lu-harness` are unchanged; `ting.service` keeps `SupplementaryGroups=lu-harness` and `ReadWritePaths=/var/lib/ting /var/lib/lu-harness`.

## What did not change

- **Environment variables.** Controller and CLI settings stay `LU_URL`, `LU_TOKEN_FILE`, `LU_CA_FILE`, and so on. `LU_JOB_SOCKET` still names the per-job harness socket. There were no `LU_AGENT_*` variables to rename.
- **Metrics.** Controller `/metrics` series keep their `lu_*` prefixes, including the node views. There were no `lu_agent_*` series to rename.
- **Config keys.** Unknown keys remain errors; no legacy aliases or compatibility shims were added.
- **Credentials.** Token files are still private files (`0600` or tighter) owned by the account that reads them. Tokens are never logged or stored in the database.
- **Store schema.** There is still a single baseline (currently version 6); this rename does not bump a schema version or require a migration of the controller database.
- **Release archives.** Names stay `lu-<tag>-<target>.tar.gz` and now contain both `lu` and `ting`. Repository links moved from `ddproxy/lu` to `ddproxy/tuishou-lu`.

## Steps

1. Install the new release; `make install` places both `/usr/local/bin/lu` and `/usr/local/bin/ting`. Install `contrib/lu.sysusers` and `contrib/lu.tmpfiles` again and run `systemd-sysusers` and `systemd-tmpfiles --create` so the `ting` account and `/etc/ting`, `/var/lib/ting` directories exist.
2. Stop the old agent and disable the old unit: `systemctl disable --now lu-agent.service`. Drain the node first if you do not want it to take jobs while it is down (`lu drain <node>`).
3. Copy your current agent config to `/etc/ting/ting.toml`. Keep the `[agent]` table; update `token_file` to `/etc/ting/ting.token` and `work_dir` to `/var/lib/ting/work`. Any agent-owned Reiki token or opencode server password should likewise move under `/etc/ting` and be owned by `ting:ting`. Validate with `lu check --config /etc/ting/ting.toml` (this opens no secrets).
4. Move the plaintext agent token to `/etc/ting/ting.token`, `chown ting:ting` and `chmod 0600` it. The controller's stored `token_sha256` does not change, so the token value stays the same.
5. Move any existing job state from the old work directory to `/var/lib/ting/work` if you need in-progress staging preserved; normally a fresh directory is fine. Keep it mode `0700` and owned by `ting`.
6. Enable the new unit: `systemctl daemon-reload` then `systemctl enable --now ting.service`. Confirm with `lu nodes` that the node heartbeats again; `agent_version` now shows the Ting package version.
7. Update harness configuration from `["lu-agent","mcp"]` to `["ting","mcp"]` for scoped jobs. If an out-of-tree Ansible role manages the agent, update its variables and handler unit as described in the final release note.
8. Remove the obsolete `lu-agent.service` unit file and the old config/token paths once the node is healthy. There is no automatic fallback from `ting` to `lu agent`.

For a foreground check without systemd, run `ting --config /etc/ting/ting.toml`. See [install.md](install.md) for the full deployment layout and [scope.md](scope.md) for the harness-facing MCP server.
