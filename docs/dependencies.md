# Dependencies

A job can wait for earlier jobs and, by default, build on their diffs. Dependencies are declared
at submission and cannot be changed later; `lu retry` can replace the whole list when it
resubmits a finished job.

```mermaid
graph TD
    build["build<br/>key: build"] --> test["test<br/>after-key: build"]
    build --> lint["lint<br/>after-key: build"]
    test --> ship["ship<br/>after-key: test, lint"]
    lint --> ship
```

## Declaring dependencies

```sh
lu submit --key build --repo /home/you/src/demo --ref main --task "build"

lu submit --key test --after-key build \
  --repo /home/you/src/demo --ref main --task "test"

lu submit --key ship --after-key test --after-key lint \
  --repo /home/you/src/demo --ref main --task "ship"
```

- `--after <job-id>` and `--after-key <key>` are repeatable and can be combined. A key resolves
  to the newest job submitted with it. IDs found through keys are appended only if not already
  present.
- A spec may name at most 16 dependencies. Duplicate IDs, unknown IDs and dead dependencies are
  rejected. An unknown `--after-key` is a CLI usage error (exit 2); an unknown ID from the API is
  `422 unknown dependency <id>`.
- A dependency that is already `failed` or `cancelled` is rejected with
  `422 dependency <id> already failed or was cancelled`. It is not possible to submit a job that
  starts in a state from which recovery depends on a dead predecessor.
- `lu retry <id> --after ... --after-key ...` replaces the dependency list on the new job. An old
  job must be terminal first.

## Waiting and promotion

A new job is `queued` when every dependency is `done`, otherwise `blocked`. `lu ls` shows the
number of unfinished dependencies in its `AFTER` column (`-` for none); `lu show` prints one
`after  <id>  <state>` line per dependency, marking those still awaited with `(waiting)`.

When a job completes `done`, the controller moves each blocked direct dependent whose entire
dependency set is now `done` to `queued`. When a job finishes `failed` or `cancelled`, the
controller recursively fails its blocked dependents with `failure_class: "dependency"` and a
summary of the form `dependency <id> <state>`. Those failures cascade in turn; no agent runs and
no dependency patch is applied for such a dependent.

## Stacking dependency diffs

By default the agent stages the dependent's base tree, then fetches each dependency's
`diff.patch` in `after` order and applies it to that tree. Empty or missing diffs are skipped.
The harness then runs on the combined tree, and the dependent's exported diff includes the
applied dependency changes. The list of applied dependency ids is recorded in `report.json` under
`applied_dependencies`.

If a dependency patch cannot be applied, the job finishes `failed` with
`failure_class: "conflict"` and a summary such as:

```text
dependency <id> could not be applied; conflicts: src/main.rs, tests/main.rs
```

An unreadable or otherwise invalid patch is also a conflict; its detail is appended to the
summary instead of a path list. Patches applied before the failing one remain in the exported
diff and are listed in `applied_dependencies`.

Pass `--no-stack` to set `stack_dependencies: false`. The job still waits for its dependencies,
but the harness runs on the dependent's own base tree alone and no dependency diffs are applied.

## Viewing the graph

`lu graph [job]` draws components that contain at least one unfinished job, newest component
first. Nodes are ordered newest first within a component and placed left to right by dependency
depth; edges point from a prerequisite to the job that names it. A job id selects that job's
component even if every job in it is finished. Display is capped at 6 components and 40 jobs,
with a note when anything is hidden.

A node's title is the first non-empty line of its task, with leading `#` heading marks and a
`Task:` or `task:` prefix removed. Colour is used only when stdout is a terminal and `NO_COLOR`
is unset. `lu graph --json` prints:

```json
{"nodes":[{"id":"...","state":"queued","title":"...","after":["..."]}]}
```

`lu top` and the TUI dashboard also expose the graph panel; press `g` to toggle it there.
