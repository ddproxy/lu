# Worked examples

These examples assume the operator connection from [client guide](client-guide.md). Job ids are
illustrative; output shapes are the ones the CLI and MCP server print.

## Build, test, collect

Submit a build, then a test that waits for it. Both use idempotency keys, so re-running a command
with the same spec returns the same job.

```sh
export LU_URL=https://lu.example.net:7600
export LU_TOKEN_FILE=/etc/lu/operator.token

lu submit --key build --repo /srv/app --ref main --task "build" --wait
lu submit --key test --after-key build --repo /srv/app --ref main --task "test" --wait
```

`submit` prints the 26-character id on stdout and `state: done` on stderr. Exit `0` means `done`;
exit `3` means `failed` or `cancelled`.

```text
01ARZ3NDEKTSV4RRFFQ69G5FAV
state: done
```

The test stays `blocked` until the build is `done`, then becomes `queued`. By default its agent
applies the build's `diff.patch` to its own base tree before starting.

Collect both results into one review job. `lu collect` sets `collect = both` and
`on_conflict = markers`.

```sh
lu collect build test --repo /srv/app --ref main --template test --key collect-v1
lu show <collect-id>
lu fetch <collect-id> --out results
```

`lu collect` prints a new job id. `lu fetch` prints one line per stored artifact:

```text
saved results/diff.patch
saved results/run.log
saved results/report.json
```

See [dependencies](dependencies.md).

## Retry with a provider override

`lu retry` needs a terminal job. Absent fields are inherited from the source spec; supplied fields
replace them.

```sh
lu retry 01ARZ3NDEKTSV4RRFFQ69G5FAV \
  --provider anthropic --model claude-sonnet-4-5 \
  --key retry-2 --wait
```

The new job id is printed first, then the final state. Give `--key` if the retry must be
idempotent; otherwise the retry has no key.

```text
01ARZ3NDEKTSV4RRFFQ69G5FBV
state: done
```

`--provider any` cannot be combined with `--model` or `--fallback`. Provider outages do not consume
`max_retries`; see [Reiki](reiki.md).

## Edit a blocked job

Start a dependency chain, then change the blocked job without changing its id or dependents.

```sh
lu submit --key slow-build --repo /srv/app --ref main --task "slow build"
lu submit --key review --after-key slow-build --repo /srv/app --ref main --task "review"

lu show <review-id>
lu edit <review-id> --provider openai --model gpt-5 --collect files
```

`lu show` prints the dependency rows after the job fields:

```text
after  01ARZ3NDEKTSV4RRFFQ69G5FAV  queued (waiting)
```

`lu edit` prints the id and its state after the edit:

```text
01ARZ3NDEKTSV4RRFFQ69G5FBV blocked
```

`--collect files` stages dependency artifacts under `.lu/inputs` without applying them to the base
tree. A job in another state, a cycle, or `--provider any` combined with `--model` or `--fallback`
is an error. See [api.md](api.md#operator).

## Probe a label

Add a probe to the agent config. The command runs at start and every `every_secs`; exit 0 publishes
the label.

```toml
[[agent.detect]]
label = "rust/cargo"
command = ["cargo", "--version"]
every_secs = 60
```

Submit a job that needs it:

```sh
lu submit --repo /srv/app --ref main --task "run cargo tests" --require rust/cargo
```

`lu nodes` shows the label with `(probed)`. The JSON view marks it in `label_sources`:

```json
{"labels":["rust/cargo"],"label_sources":{"rust/cargo":"probed"}}
```

Probe labels join static and file labels under the 32-label cap and are dropped last when the cap
is full. See [config.md](config.md#agentdetect).

## Cancel a job

```sh
lu cancel <job-id>
lu show <job-id>
```

`lu cancel` prints the same key/value rows as `lu show`. For `blocked`, `queued` and `recovering`
jobs the state becomes `cancelled` at once. For `leased`, `running` and `needs_input` jobs the
controller sets `cancel_requested`; the job stays in its state until the agent stops the harness and
reports back. A terminal job returns `409 conflict`. Use `lu show <id> --json` to read
`cancel_requested` directly.

## Submit through MCP

`lu mcp` reads newline-delimited JSON-RPC from stdin. An explicit `key` makes the submission
idempotent.

```json
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"lu_submit","arguments":{"repo":"/srv/app","ref":"main","task":"run the test suite","key":"build-v1"}}}
```

The text part of the reply starts with this shape; values are examples:

```text
id=01ARZ3NDEKTSV4RRFFQ69G5FAV state=queued attempt=1 node=- repo=/srv/app ref=main title=run the test suite
after= waiting_on=
```

Without `key`, the server derives `mcp-` plus 16 hex characters of the spec's SHA-256. Repeating the
same arguments targets the same job. A different spec with the same key is `409 conflict`. More than
30 submissions in a rolling minute return `isError` with the seconds to wait; that limit is not an
HTTP 429. Full tool arguments are in [mcp.md](mcp.md).
