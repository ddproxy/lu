# Lu with Reiki

Lu reads Reiki health to learn which providers a node can use. The agent on the node asks its local
Reiki for health and sends the result with each heartbeat. The controller uses that result for
dispatch and cordons. Lu does not route harness traffic through Reiki, choose keys, or store Reiki
credentials.

> **Unsupported:** A node with no Reiki feed reports no providers. A job that names a provider
> cannot run there, even if the harness could reach that provider by another route.

## Harness setup

Reiki is configured on the agent side. Put both keys in the same `[agent]` table; one without the
other is a config error.

```toml
[agent]
# ... name, controller_url, token_file, work_dir, harness ...
reiki_health_url = "http://127.0.0.1:4100/v1/health"
reiki_token_file = "/etc/ting/reiki.token"

[[agent.provider_models]]
provider = "anthropic"
model = "claude-sonnet-4-5"

[[agent.provider_models]]
provider = "openai"
model = "gpt-5"
```

| Key | Rule |
| --- | --- |
| `reiki_health_url` | Full loopback `http` or `https` URL with a host. No credentials, query or fragment. Requires `reiki_token_file`. |
| `reiki_token_file` | Regular private file, owned by the agent user, mode `0600` or tighter, no symlink or FIFO. The agent reads up to 4 KiB at start and uses one trimmed line as the bearer token. |
| `[[agent.provider_models]]` | Up to 64 ordered `{ provider, model }` pairs. A pair is published only while Reiki reports that provider `ok`. `provider = "any"` jobs use these pairs in order. |

`lu check` validates the keys but does not open the token file. `ting` reads the token at start;
a bad token file stops the agent.

## Health reading

The agent sends `GET /v1/health` with the bearer token every 10 seconds. Heartbeats reuse the last
answer.

| Item | Behaviour |
| --- | --- |
| Timeout | 3 seconds. |
| Redirects | Refused. |
| Reply cap | 256 KiB. A larger reply is ignored. |
| Forwarded document | Included in the heartbeat only when it serialises to 48 KiB or less. |
| Providers | The first 64 valid `upstreams[]` entries. Invalid entries are skipped. |
| Failed reads | Three in a row reset health to empty: no providers and no forwarded document. |

Each `upstreams[]` entry becomes one provider status. The first matching rule wins:

| Reiki fields | Lu state | Usable for dispatch? |
| --- | --- | --- |
| `healthy_keys > 0` | `ok` | Yes. |
| `healthy_keys` is 0, `cooling_keys > 0` | `cooling` | No. `soonest_cooldown_seconds` is kept when present. |
| `healthy_keys` and `cooling_keys` are 0, `disabled_keys > 0` | `disabled` | No. |
| None of the above | `unknown` | No. |

An example health document, with the resulting states:

```json
{"instance":"node-a","upstreams":[
  {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0},
  {"name":"openrouter","healthy_keys":0,"cooling_keys":1,"disabled_keys":0,"soonest_cooldown_seconds":90},
  {"name":"forgejo","healthy_keys":0,"cooling_keys":0,"disabled_keys":1},
  {"name":"empty","healthy_keys":0,"cooling_keys":0,"disabled_keys":0}
]}
```

`zen` is `ok`, `openrouter` is `cooling` with 90 seconds left, `forgejo` is `disabled`, and `empty`
is `unknown`. Only `zen` is eligible for provider jobs.

## Dispatch

The heartbeat carries the provider list, the opaque Reiki document, and the healthy
`provider_models` pairs.

| Job spec | Placement rule |
| --- | --- |
| No `provider` | Runs on any eligible node. |
| `--provider openai` | Needs a node reporting `openai` as `ok`. |
| `--provider any` | Needs a healthy published `{provider, model}` pair. Pairs are tried in config order. |
| Primary plus `--fallback` | Choices are tried in order; the first healthy, uncordoned pair on an eligible node is assigned. |

A queued `any` job with no healthy pair stays `queued` with the waiting reason
`no node has a healthy provider for provider "any"`. Labels, taints, drains and one-job-per-agent
still apply; see [dispatch](dispatch.md).

## All keys cool

A running job whose provider stops being usable is stopped by the agent, not by a harness marker.

| Step | What happens |
| --- | --- |
| 1 | The run watcher checks every 5 seconds, but acts only on a new health reading. The feed itself refreshes every 10 seconds. |
| 2 | Two fresh readings in a row that show the assigned provider `cooling` or `disabled` stop the harness. |
| 3 | The job completes `Failed(Provider)`. The summary is `Reiki reports provider <name> has no usable key; the soonest returns in <secs>s`, or the same text without the time when Reiki gives none. |
| 4 | `retry_after_secs` is the soonest return plus 60 seconds. |
| 5 | The job requeues while `attempt <= 100`; `max_retries` does not apply. The assigned `(node, provider)` pair is cordoned. |
| 6 | The cordon lasts for `retry_after_secs`, clamped to 5 seconds–24 hours. Without that estimate it lasts `provider_cooldown_secs`, default 1800 seconds. |

A Reiki `503` whose body names `gateway_store_unavailable` or `per-stream counters` is different:
it is a node-side gateway fault, not a provider key outage. It stops the running job as
`Failed(node)` with `failure_reason: "reiki_gateway_503"`, excludes that node from the job and
requeues within `[controller] node_attempts`; it creates no provider cordon. An unnamed `503` is
only a failed health read and does not classify the job. See
[Node infrastructure failures](lifecycle.md#node-infrastructure-failures).

A Reiki-derived cordon lifts at expiry or after two consecutive healthy provider heartbeats. A
non-healthy reading resets that streak. A controller restart clears all cordons through boot
recovery; Reiki health and new failures set them again. An operator can clear one early with
`lu uncordon <node> [<provider>]`. See
[Provider cordons and node drains](operations.md#provider-cordons-and-node-drains).

> **Fallback, not Reiki:** For a job with no provider, or a node without a Reiki feed, the agent
> also watches harness output. If the newest line is a provider-unavailable marker and the harness
> then produces no output for 120 seconds, the job completes `Failed(Provider)`. Later output
> disarms the watch. This path does not use Reiki health.

## Observing

| Command | What to read |
| --- | --- |
| `lu show <id>` | `waiting` for a queued job; `failure` and `summary` after a provider stop. |
| `lu nodes --json` | `providers`, `provider_models`, `cordon_details` and the opaque `reiki` document for each node. |
| `lu node <name>` | Latest heartbeat stats, including labels published by probes. |
| `lu logs <id>` | Harness output; Reiki-derived stops may have no provider marker in it. |

Route and field details are in [api.md](api.md). Config limits are in [config.md](config.md).
