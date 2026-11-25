# Submitting work

Lu accepts one job at a time from the CLI, MCP tools or the HTTP API, and whole sets of jobs from
batch files. Signed intake normally proposes drafts for an operator to approve; see
[Webhook intake](intake.md).

## Batch submissions

`lu submit --batch <path>` turns a directory of task files or a Markdown checklist into one job per
task. Every job gets a deterministic idempotency key, all input is validated before the first
request, and a dry run prints the plan without connecting to a controller or reading an operator
token. Batch inputs only populate fields that `JobSpec` already has; the store schema is unchanged.
With `--chain`, each job depends on all earlier jobs and inherits their staged base; see
[One base per chain](dependencies.md#one-base-per-chain).

### Inputs

When `<path>` is a directory, every regular, non-hidden file whose extension is not `.toml` becomes
one task. Files are read in name order; the file's stem is the slug. A file named `build.md` looks
for `build.toml` beside it as its sidecar. Symbolic links in the batch root, task files or sidecars
are refused, other directories are ignored, and each task is capped at 256 KiB.

When `<path>` is a file, it is parsed as a Markdown checklist. Each top-level unchecked `- [ ]`
item becomes a job; checked items and fenced code blocks are skipped. More-indented lines under an
item become part of its task text, so the nested item below is context, not a third job:

```markdown
# Release

- [ ] Build the artifacts
  Use the release profile.
- [ ] Publish the artifacts
  - [ ] Smoke test the mirror
- [x] Already announced
```

A sibling TOML file with the same stem (`release.toml` for `release.md`) applies to every job.
Checklists are capped at 1 MiB.

### Keys, defaults and sidecars

`--key-prefix` is required. Each key is `<prefix>-<slug>`, where the slug is a lowercase,
hyphenated form of the file stem or item text. A rerun with the same prefix targets the same jobs;
a repeated slug inside one batch is an error. Keys longer than the controller's 128-byte limit are
refused before submission.

`--defaults <file.toml>` supplies fields common to the batch. Directory-mode sidecars override
them for one task; a checklist's sibling sidecar applies to all its jobs. Unknown keys are errors,
and TOML files are capped at 64 KiB.

```toml
# defaults.toml
repo = "/srv/app"
ref = "main"
provider = "peer-a"
model = "small"
priority = 3
group = "release"
timeout = 120
idle_timeout = 60
workspace = "keep"
require = ["linux"]
after_keys = ["prepare"]

[expect]
files = ["REVIEW.md"]
changed = ["src/**"]
mode = "fail"
```

Defaults accept `repo` and `ref` plus these shared fields; sidecars accept only the shared fields:

| Field | Meaning |
| --- | --- |
| `provider`, `model` | Provider and model names, as on `lu submit`. |
| `priority` | `0` to `9`; higher dispatches first. |
| `group` | A `1` to `64` character concurrency group. |
| `timeout`, `idle_timeout` | `1` to `86400` seconds; an idle timeout is omitted when absent. |
| `expect` | `files`, `changed`, `forbid_changed`, `min_diff_bytes`, `max_diff_bytes`, `mode`. |
| `workspace` | `ephemeral`, `keep_on_failure` or `keep`. |
| `require` | Node labels; an empty array clears a default list. |
| `after_keys` | Idempotency keys of existing jobs that must finish first. |

`repo` and `ref` may come from `--repo`/`--ref` or the defaults file, never from a task file or a
directory-mode sidecar.

### Dependencies and parallelism

`--chain` makes each job depend on all earlier jobs in input order and is additive with other
prerequisites. An `after:` hint may appear on a checklist item or an indented line beneath it; a
directory-mode task can carry `after_keys` in its sidecar. Hints are comma- or whitespace-separated
and may name a slug, a full key or an external idempotency key:

```markdown
- [ ] Build
- [ ] Test
  after: build, ci-base
```

Names matching an earlier job in the batch resolve to that job's id; other keys are looked up on
the controller. A current, later, unknown or over-16-dependency reference is an error.

`--max-parallel N` sets a runtime cap of `1` to `64` concurrent jobs. All jobs must share one group
or have none; the cap group name defaults to the slugified key prefix. The CLI calls
`PUT /v1/groups/{name}/limit` before submitting. The cap lives in controller memory and survives a
restart only if it is also in `[[controller.groups]]`; see [Dispatch](dispatch.md#priority-and-groups).

### Dry run and failures

```sh
lu submit --batch ./tasks \
  --key-prefix release-1 \
  --defaults ./defaults.toml \
  --chain --max-parallel 2 \
  --dry-run
```

`--dry-run` prints keys, chain order, group, sources and task byte sizes, or one JSON object with
global `--json`. It performs the same local validation but sends no request and reads no token.
All input errors are collected before submission: missing `repo`/`ref`, an invalid prefix, duplicate
or underivable slugs, empty or oversized tasks, bad TOML or keys, bad values, invalid hints and
group-cap conflicts. Usage errors exit `2` and submit nothing. A controller failure partway through
prints the submitted rows and exits `1`; the submitted jobs keep their keys, so a rerun completes
the remaining work. Tokens never appear in plan or result output.

`lu_submit_batch` uses the same planner from the MCP host's filesystem; its arguments and limits
are in [CLI and MCP clients](cli.md#batches).

## Drafts

A draft is a stored job that an operator has not approved. Intake creates drafts by default, and an
operator can submit one deliberately. Drafts are visible and editable but never dispatched: no
lease, no agent poll event and no harness run until approval. See [Webhook intake](intake.md) for
how external events become drafts and [Job Lifecycle](lifecycle.md#drafts) for the state machine.

### Job fields

| Field | Meaning |
| --- | --- |
| `source.repo` | Repository `https` URL. |
| `source.ref` | Branch, tag, commit, or `refs/pull/<n>/head` / `refs/pull/<n>/merge`. |
| `source.base` | Optional base commit id. |
| `task` | Operator-authored instruction text; intake writes it from `task_template`, not raw payload fields by default. |
| `context` | Optional external text, capped at 32 KiB with a truncation marker. |
| `origin`, `origin_ref` | Optional provenance label and length-capped link back to the submitter. |
| `untrusted_context` | Marks `context` as external data. |
| `draft` | When true, the job is stored as `draft` and is never dispatched. |

Task text is operator-authored; context is external data. The agent stages `context` in
`.lu/inputs/context.md` under an **UNTRUSTED EXTERNAL TEXT** header so the harness treats it as
data. An intake rule can merge payload text into `task` only with `unsafe_inline = true`, which is
dangerous and off by default. Wire shapes are in [HTTP API](api.md#jobspec).

### Approving and rejecting

| Command | Effect |
| --- | --- |
| `lu drafts [--limit <n>]` | List unapproved drafts, oldest first; expired drafts are counted but not listed. |
| `lu show <draft>` | Show spec, origin, context metadata and dependencies. |
| `lu edit <draft>` | Edit task text, provider, model, priority, source ref and publish settings. |
| `lu approve <draft> [--edit]` | Move to `queued`, or `blocked` when dependencies are unmet; `--edit` edits first in one action. |
| `lu reject <draft> [--reason <text>]` | Cancel; the reason becomes a safe audit code, not free job text. |

```sh
lu drafts
lu show 01ARZ3NDEKTSV4RRFFQ69G5FAV
lu edit 01ARZ3NDEKTSV4RRFFQ69G5FAV --task-file task.txt
lu approve 01ARZ3NDEKTSV4RRFFQ69G5FAV
lu reject 01ARZ3NDEKTSV4RRFFQ69G5FBV --reason "duplicate"
```

Approval and rejection require a draft and an operator token; the source that created a draft cannot
call either endpoint. `lu edit` also accepts a `queued` or `blocked` job, but not an `expired` one.
The API routes are `GET /v1/drafts`, `PUT /v1/drafts/{id}`, `POST /v1/drafts/{id}/approve` and
`POST /v1/drafts/{id}/reject` in [HTTP API](api.md#operator). `lu_drafts` is an operator-only MCP
read tool; job-scoped agent paths cannot see drafts ([CLI and MCP clients](cli.md#mcp-tools)).

### Limits, expiry and observability

| Key | Default | Meaning |
| --- | --- | --- |
| `[drafts] ttl` | `7d` | Drafts still unapproved after this interval become terminal `expired`. |
| `[drafts] max_drafts` | `10000` | Total cap; per-origin caps apply in addition. |
| `[drafts] max_drafts_per_source` | `1000` | Per-origin cap; must not exceed `max_drafts`. |

When space is needed, the oldest drafts are expired first. If both caps still bind, new drafts are
refused with `429 limit_exceeded` and the intake route counts the outcome as `invalid`. Full config
rules are in [Configuration](config.md#drafts).

`lu top` shows the draft count and the oldest draft age. `/metrics` exports `lu_drafts{state}` and
`lu_intake_total{source,result}`; the `jobs` object in `GET /v1/status` includes `draft`, `expired`
and `deadline_missed` ([Monitoring](monitoring.md#series)).

A draft is not a permit to run code. Approval is the trust decision: the approving operator owns
the task text and settings. Until then, payload-derived text stays in `context` and is staged as
data. The boundary for a sender who can post an event is in
[Security](security.md#intake-and-drafts).
