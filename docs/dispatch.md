# Choosing where a job runs

A job runs on a node that is idle, not draining and able to serve what the job needs. Two optional fields on a job narrow that choice: labels describe what the node can do, and provider choices describe which model the harness may use. A node must satisfy the labels first, then it must be able to serve one of the provider choices.

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

5. Run an agent on the same machine. Write `$HOME/lu-demo/agent.toml` with an `[agent]` table (`name = "node-a"`, `controller_url = "http://127.0.0.1:7600"`, `token_file` pointing at `agent.token`, a `work_dir` under `lu-demo`, `harness = "opencode"`), then:

```sh
lu agent --config "$HOME/lu-demo/agent.toml"
```

In the other shell, `lu submit ... --wait` returns when the job is done, `lu logs --follow <job-id>` streams its output, and `lu fetch <job-id> --out results` saves the diff, log and report.

For a controller on a non-loopback address, add a `[tls]` table with `cert` and `key`; plain HTTP is accepted only on loopback. See [docs/config.md](docs/config.md).

See [Concepts](concepts.md) for the vocabulary and [Job lifecycle](lifecycle.md) for what happens when no node qualifies.
