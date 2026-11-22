# Batch submissions

`lu submit --batch <path>` turns a directory of task files or a Markdown checklist into one job per
task. It is meant for repeating a known set of jobs safely: every job gets a deterministic
idempotency key, the whole input is validated before the first request, and a dry run prints the
plan without connecting to a controller or reading an operator token.

The schema version stays at `2`. Batch inputs only populate fields that `JobSpec` already has.

## Inputs

### Directory

When `<path>` is a directory, every regular, non-hidden file whose extension is not `.toml`
becomes one task. Files are read in name order; the file's stem becomes the job slug. A file named
`build.md` uses `build` as its slug hint and looks for `build.toml` beside it as its sidecar.

Symbolic links in the batch root, task files or sidecars are refused. Directories other than the
root are ignored. Each task is capped at 256 KiB.

### Checklist

When `<path>` is a file, it is parsed as a Markdown checklist. Every top-level unchecked `- [ ]`
item becomes a job. Checked items and lines inside fenced code blocks are skipped. More-indented
lines under an unchecked item become part of that item's task text, so this:

```markdown
# Release

- [ ] Build the artifacts
  Use the release profile.
- [ ] Publish the artifacts
  - [ ] Smoke test the mirror
- [x] Already announced
```

creates two jobs. The nested `Smoke test the mirror` line is context for `Publish the artifacts`,
not a third job. A sibling TOML file with the same stem (`release.toml` for `release.md`) is a
shared sidecar applied to every job in the checklist. Checklists are capped at 1 MiB.

## Keys and idempotency

`--key-prefix` is required with `--batch`. Each key is `<prefix>-<slug>`, where the slug is a
lowercase, hyphenated form of the file stem or checklist item text. Re-running the same batch with
the same prefix targets the same jobs; the controller returns the existing job instead of creating
a duplicate. A repeated slug inside one batch is an error, even when the task text differs.

If a key would exceed the controller's 128-byte idempotency-key limit, the batch is refused before
submission. Use a shorter prefix or shorten the item text.

## Shared defaults and sidecars

`--defaults <file.toml>` supplies fields common to the whole batch. Directory-mode sidecars
override those fields for one task; a checklist's sibling sidecar applies to all its jobs. Unknown
keys are errors in both files, and TOML files are capped at 64 KiB.

```toml
# defaults.toml
repo = "/srv/app"
ref = "main"
provider = "codex"
model = "gpt"
priority = 3
group = "release"
timeout = 120
workspace = "keep"
require = ["linux"]
after_keys = ["prepare"]

[expect]
files = ["REVIEW.md"]
changed = ["src/**"]
mode = "fail"
```

Defaults accept `repo` and `ref` plus the shared job fields below. Sidecars accept only the shared
fields:

| Field | Meaning |
| --- | --- |
| `provider`, `model` | Provider and model names, as on `lu submit`. |
| `priority` | `0` to `9`; higher dispatches first. |
| `group` | A `1` to `64` character concurrency group. |
| `timeout` | `1` to `86400` seconds. |
| `expect` | A table with `files`, `changed`, `forbid_changed`, `min_diff_bytes`, `max_diff_bytes` and `mode`. |
| `workspace` | `ephemeral`, `keep_on_failure` or `keep`. |
| `require` | Node labels. An empty array clears a default list. |
| `after_keys` | Idempotency keys of existing jobs that must finish first. |

`repo` and `ref` may come from `--repo`/`--ref` on the command line or from the defaults file. At
least one source must supply each; they are never read from a task file or sidecar in directory
mode.

## Dependencies

`--chain` stacks jobs in input order: each job depends on all earlier jobs in the batch. Chaining
is additive, so a job can also name other prerequisites.

An `after:` hint may appear on a checklist item or on an indented line beneath it. Directory-mode
tasks can carry `after_keys` in their sidecar. Hints are comma- or whitespace-separated and may use
a slug, a full key, or an external idempotency key:

```markdown
- [ ] Build
- [ ] Test
  after: build, ci-base
```

Names that match an earlier job in the same batch resolve to that job's id. Other keys are looked
up on the controller before submission. A hint that names the current or a later job, an unknown
external key, or more than 16 dependencies is an error. Checked items and fenced blocks never
produce jobs or hints.

## Parallelism cap

`--max-parallel N` sets a runtime cap of `N` concurrently running jobs (`1` to `64`). All jobs
must share one group or have none. When none is set, the cap group name is the slugified key
prefix. The CLI calls `PUT /v1/groups/{name}/limit` with `{"max_running": N}` before submitting;
the cap is stored by the controller and shown by `lu_group_running{group}` and
`lu_group_limit{group}`. The cap is process-local to that controller and survives a restart only
if it also appears in `[[controller.groups]]` configuration. See [Configuration](config.md#controllergroups) and [HTTP API](api.md#operator).

## Dry run

```sh
lu submit --batch ./tasks \
  --key-prefix release-1 \
  --defaults ./defaults.toml \
  --chain --max-parallel 2 \
  --dry-run
```

`--dry-run` prints the keys, chain order, group, sources and task byte sizes. With global
`--json`, it prints one object:

```json
{
  "dry_run": true,
  "chain": true,
  "max_parallel": 2,
  "group": "release-1",
  "jobs": [
    {"key": "release-1-build", "slug": "build", "group": "release", "after": [], "bytes": 24, "source": "tasks/build.md"}
  ]
}
```

The dry run performs no validation request, does not read the operator token and does not contact
the controller. It still validates all local inputs, so a bad batch fails before any plan is
printed.

## Validation and failures

All input errors are collected and reported together before submission: missing `repo`/`ref`, an
invalid prefix, duplicate or underivable slugs, empty or oversize tasks, malformed TOML, unknown
TOML keys, bad field values, invalid hints and group-cap conflicts. Usage errors exit `2` and
submit nothing.

After validation, external keys and the group cap are resolved. If that preflight fails, no job is
submitted. A controller failure partway through stops the submission, prints the completed rows
and exits `1`; the already-submitted jobs keep their keys, so re-running the batch finishes the
remaining work without duplicating them.

`--json` prints `{"jobs":[...],"submitted_count":N,"not_submitted":[]}` for a successful run. A
partial failure prints the completed rows and a separate error line on stderr with the remaining
labels. Tokens never appear in plan or result output.

## MCP tool

`lu_submit_batch` uses the same planner and submission helpers as the CLI. Its arguments are
`path` and `key_prefix` (required), plus optional `repo`, `ref`, `defaults`, `chain`,
`max_parallel` and `dry_run`. Paths are on the MCP host's filesystem. A dry run returns the plan
as `structuredContent` without needing a controller connection; otherwise the tool shares the
30-submissions-per-minute budget with `lu_submit`, `lu_submit_many` and `lu_retry`. See
[MCP server](mcp.md#batches).

```json
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"lu_submit_batch","arguments":{
  "path":"/srv/app/tasks/release.md",
  "key_prefix":"release-1",
  "defaults":"/srv/app/tasks/defaults.toml",
  "chain":true
}}}
```

`lu_submit_many` remains available for callers that want to send inline specs instead of reading
files; the two tools share resolution, chaining and partial-failure reporting, not a single
tool name.
