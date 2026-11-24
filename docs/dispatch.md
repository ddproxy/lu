# Choosing where a job runs

A job runs on a node that is idle, not draining and able to serve what the job needs. Two optional fields on a job narrow that choice: labels describe what the node can do, and provider choices describe which model the harness may use. A node must satisfy the labels first, then it must be able to serve one of the provider choices.

## Priority and groups

`--priority N` sets an integer from 0 (lowest) to 9 (highest); the default is 5. Dispatch orders queued jobs by starvation class, then effective priority, then `dispatch_seq` and `eligible_since` ascending. Effective priority starts at `spec.priority` and gains up to two points as the job waits; once a job is older than `[schedule] starvation_secs` (default 3600) it outranks every static priority. `dispatch_seq` is persisted submission order, not wall-clock id order. See [Time-aware scheduling](scheduling.md#admission-deadlines-and-queue-order).

`--group NAME` marks a job as part of a concurrency group. Names are 1-64 characters from `A-Z a-z 0-9 . _ -`. A controller can cap a group with `[[controller.groups]]` or set a runtime cap with `PUT /v1/groups/{name}/limit`; jobs in uncapped groups are unlimited. Runtime caps stay in controller memory and return to configured values on restart. Jobs count toward the cap while `leased`, `running`, `recovering` or `needs_input`. A queued job blocked by a full group stays queued and reports `group limit (<name> <running>/<max>)` in `waiting_reason`. Cancellation and failure release the slot.

Both fields are optional; omitting them keeps the previous dispatch behaviour. The provider, label and taint rules described below still decide node eligibility; priority orders queued jobs and a group cap blocks dispatch, neither bypasses node eligibility.

## Labels and providers

To pin work to nodes that advertise every named label, repeat `--require`:

```sh
lu submit --repo /path/to/repository --ref main --task "run the PHP suite" --require php8.3 --require gpu
```

Labels are opaque to Lu: an agent advertises them, a job requires them, and dispatch compares them exactly. A submit with a label no online node advertises still succeeds and prints a warning; a queued job that cannot be placed shows why in `lu show` and `lu top`.

To let a job run on another provider or model when its first choice is unavailable, repeat `--fallback`. The primary `--provider`/`--model` stays the first choice, and each fallback is tried in order after it:

```sh
lu submit --repo /path/to/repository --ref main --task "run the test suite" \
  --provider peer-a --model big --fallback peer-b:small --fallback peer-c
```

Dispatch first applies the label rule, then gives the job to the first choice the node can serve (provider healthy and not cordoned). A fallback with no model reuses the primary model; a fallback that names a model uses it. The job view records both `assigned_provider` and `assigned_model`, and `lu show` prints the fallback list and `assigned_provider`; `lu ls` is unchanged. A job with no primary provider ignores its fallback list. An older agent that does not read the assignment fields runs the primary choice.

## Provider from the node

Use `--provider any` to choose from the node's `[[agent.provider_models]]` pairs, tried in agent configuration order. The job names no model or fallback; either with `--provider any` is a validation error. Each agent publishes a pair with its heartbeat when Reiki reports that provider healthy. Dispatch skips cordoned providers and gives the job to the first healthy pair on an otherwise eligible node. The job records that pair as `assigned_provider`/`assigned_model`, so the agent runs that provider's model. With no healthy pair on any eligible node the job stays `queued` and `lu show` reports `no node has a healthy provider for provider "any"`.

For a controller on a non-loopback address, add a `[tls]` table with `cert` and `key`; plain HTTP is accepted only on loopback. See [config.md](config.md).

## Cordons

A cordon is a `(node, provider)` block. Dispatch skips that pair on that node, so fallbacks, other nodes and other providers remain available. A job with no provider is not affected.

- **Reiki-derived**: the agent reports two consecutive unhealthy Reiki readings and completes the job as `Failed(Provider)` with `retry_after_secs`. The cordon has source `reiki`, records Reiki's estimated return time in its reason, and lifts after two consecutive healthy provider heartbeats. Its API `until` is null because the end time is not fixed.
- **Failure-derived**: any other retryable provider failure uses `provider_cooldown_secs`. The cordon has source `failure`, a reason naming the provider and failure summary, and a fixed wall-clock expiry.

`lu nodes` shows cordoned providers in the `CORDONS` column as `!provider`; `lu top` marks the provider chip with `!` and a `~` suffix for Reiki-derived cordons, then lists both kinds with their reason and remaining time or lift count. `lu node <name>` prints the same details with the age of each cordon. An operator can clear a cordon early with `lu uncordon <node> [<provider>]`, which calls `POST /v1/nodes/{name}/uncordon`; omitting the provider clears every cordon on that node. Boot still clears all stored cordons, so a controller restart never blocks dispatch with stale deadlines.

## Taints and tolerations

A node can refuse work unless a job opts in. An agent lists up to 8 opaque taints in `[agent].taints`; a job names up to 8 with `--tolerate`. Dispatch gives a job only to a node whose every taint the job tolerates, so a node reserved for one kind of work never receives anything else. A taint is compared exactly and never interpreted. `lu nodes` shows taints first with a `!` prefix.

```sh
lu submit --repo /path/to/repository --ref main --task "build the homelab image" --tolerate homelab
```

## Detecting labels

An agent can publish labels from commands it runs itself: up to 16 `[[agent.detect]]` entries run an argv command at start and every `every_secs`. Exit status 0 publishes the entry's label; any other exit withdraws it. Probes run off the heartbeat path with a wall timeout and a minimal environment, and `lu nodes` marks a published label as `(probed)`. Config keys are in [config.md](config.md).

See [Concepts](concepts.md) for the vocabulary and [Job lifecycle](lifecycle.md) for what happens when no node qualifies.
