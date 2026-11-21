# Adding a node safely

Use this sequence for a new agent host. A node enters controller state only after its first
heartbeat; `lu drain` returns `404` for an unknown node. Give the new node an onboarding taint
before it starts so ordinary jobs cannot be dispatched to it during that first window.

The examples use `node-b`, operator settings from `LU_URL` and `LU_TOKEN_FILE`, and a controller
config that already contains `[[controller.agents]]` for the node.

## 1. Register the identity and start drained

Generate a token, put its hash in the controller config and restart the controller. There is no
config reload; token hashes and TLS material are read at startup. See
[Tokens and TLS](tokens-and-tls.md#rotation).

```toml
[[controller.agents]]
name = "node-b"
token_sha256 = "<64 lowercase hex chars>"
```

On the node, write an agent config with an onboarding taint and a label the canary can require:

```toml
[agent]
name = "node-b"
controller_url = "https://lu.example.net:7600"
token_file = "/etc/lu/agent.token"
work_dir = "/var/lib/lu/work"
harness = "opencode"
labels = ["canary/node-b"]
taints = ["onboarding"]

[[agent.detect]]
label = "opencode/cli"
command = ["opencode", "--version"]

[[agent.provider_models]]
provider = "anthropic"
model = "claude-sonnet-4-5"
```

`labels` and `taints` are opaque to Lu. A tainted node takes only jobs that tolerate every taint.
A probe publishes its label while `command` exits `0`; a failure withdraws it. `provider_models`
are offered only while Reiki reports that provider healthy. Keys and validation rules are in
[Configuration](config.md).

```sh
lu check --config /etc/lu/agent.toml
systemctl enable --now lu-agent.service
```

`lu check` reads no token files and creates no directories. The agent creates `work_dir`
recursively with mode `0700` if it is missing.

Wait for a heartbeat, then drain the node:

```sh
lu nodes
lu drain node-b
lu nodes
```

Expected drain response. The controller answers `{}`, so the CLI prints a placeholder view with
the name and drain flag:

```text
name       node-b
version    -
harness    -
last_seen  -
draining   yes
```

The `DRAINED` column in `lu nodes` now reads `yes`. A drain survives a controller restart; no job
is dispatched to a drained node.

## 2. Check what the node advertises

```sh
lu nodes
lu nodes --json
lu node node-b
```

| Check | Where | What to look for |
| --- | --- | --- |
| Identity and state | `lu nodes` | `NAME`, `VERSION` (agent version, `-` if unknown), `HARNESS` (`idle`, `busy` or `-`), `LAST_SEEN`, `DRAINED`. There is no `ONLINE` column in the text table. |
| Labels and taints | `lu nodes` `LABELS` | Taints come first with `!`, so `!onboarding,canary/node-b,opencode/cli (probed)`. A long list ends with `+N`. |
| Probe labels | `lu nodes`, `lu node`, `--json` | A probed label is followed by ` (probed)`; JSON carries it in `label_sources` as `"opencode/cli": "probed"`. |
| Provider models | `lu nodes --json` | `provider_models` holds the configured pairs that Reiki currently reports healthy. `providers` holds each provider's state (`ok`, `cooling`, `disabled`, `unknown`). |
| Cordons | `lu nodes --json` | `cordons` lists provider names with an unexpired `(node, provider)` cordon. |
| Host and process stats | `lu node node-b` | `labels`, load, memory, disk for the `work_dir` filesystem and any running job tree. Stats are sampled at heartbeat time and are lost on controller restart until the next heartbeat. |

`lu node <name>` prints stats, not the full node view; an unknown node is a `404` and exits `1`.
`lu nodes --json` is the authoritative view for labels, taints, providers, provider models and
cordons. See [Reading `lu nodes`](operations.md#reading-lu-nodes).

If a probe label is missing, check the command on the node with only the agent's `PATH`, `HOME` and
`LANG`. If a provider model is missing, check Reiki's health first: an agent publishes pairs only
while that provider is healthy, and stops publishing all providers after three failed reads.

## 3. Submit a canary while the node is drained

Require the new node's label and tolerate its onboarding taint. The job stays `queued` because the
node is drained, even though the label matches.

```sh
lu submit --key canary-node-b \
  --repo /srv/repos/demo --ref main \
  --task "Run the test suite. Do not edit tracked files." \
  --require canary/node-b --tolerate onboarding --timeout 300
```

While the node is drained, `lu show` reports `state queued` with `node -`: no assignment has
happened. Do not leave the canary tolerating `onboarding` after the node is in service.

## 4. Undrain, then verify

```sh
lu undrain node-b
lu logs --follow <canary-id>
lu fetch <canary-id> --out ./canary-node-b
```

Expected `lu fetch` output when all three artifacts exist:

```text
saved ./canary-node-b/diff.patch
saved ./canary-node-b/run.log
saved ./canary-node-b/report.json
```

`lu logs --follow` exits `0` for `done` and `3` for `failed` or `cancelled`. A failed job still
stores a partial diff. Confirm `lu nodes` shows `harness idle` and `DRAINED no` before treating
the node as in service.

Now remove `taints = ["onboarding"]` from the agent config and restart `lu agent`. The next
heartbeat replaces the taint set; verify `!onboarding` is gone in `lu nodes` and in
`lu nodes --json`.

## If the canary fails

1. Keep or restore the drain: `lu drain node-b`.
2. Read `lu show <canary-id>`, `lu logs --follow <canary-id>` and `lu node node-b`.
3. Check `lu nodes --json` for `cordons` and `label_sources`. A harness crash at launch adds the
   label and taint `harness_ok=false`; a later successful run clears both.
4. Fix the host or config, restart the agent and repeat from step 2.

There is no CLI command to clear a provider cordon. It expires at its deadline, a Reiki-derived
cordon clears after two consecutive healthy provider heartbeats, and a controller restart deletes
all cordons. A drain is cleared with `lu undrain`. See
[Provider cordons and node drains](operations.md#provider-cordons-and-node-drains) and
[What Lu does not protect against](limits.md).

## Checklist

| Done | Check |
| --- | --- |
| ☐ | Controller config has the node name and token hash; controller restarted. |
| ☐ | Agent config has `name`, `controller_url`, `token_file`, `work_dir`, `harness`, labels and the onboarding taint; `lu check` passes. |
| ☐ | Node appears in `lu nodes` with the expected version, harness state, labels, probes and provider models. |
| ☐ | Node is drained before general work is allowed. |
| ☐ | Canary requires the node's label and tolerates the onboarding taint; artifacts fetched. |
| ☐ | Node undrained, taint removed, agent restarted and heartbeat verified. |
