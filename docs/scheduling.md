# Time-aware scheduling

Phase-1 scheduling controls **when a queued job may start**. It is configured on the controller with
`[schedule]`, `[[calendar]]` and `[[attach]]`, and requested per job with `JobSpec.schedule`. It does
not change which repo a job uses or how the harness runs.

This is the phase-1 feature set: no cron expressions, timezone database, DST, cost weights or burst
limits. Calendar offsets are UTC or fixed offsets only.

## What is and is not in the CLI

There is no `lu schedule` command and no scheduling flags on `lu submit`, `lu retry`, `lu edit`,
`lu collect`, `lu submit --batch` or `lu mcp`. Those paths build specs with `schedule: None`. The
CLI can **inspect** the result with `lu status --json`, `lu ls` and `lu show [job] --json`, but it
cannot set or reload scheduling configuration.

Set scheduling constraints through the HTTP API: `POST /v1/jobs` for a new job, `POST /v1/jobs/{id}/edit`
for a queued or blocked job, or `POST /v1/schedule/reload` to swap the active controller schedule.
Scoped harness children inherit the parent's schedule block but cannot set or widen it.

## Job constraints

`JobSpec.schedule` is an optional object. Unknown fields are rejected. Durations accept an integer
number of seconds or a string with an `s`, `m`, `h` or `d` suffix; config tuning values are plain
integer seconds.

| Field | Wire form | Meaning |
| --- | --- | --- |
| `not_before` | Unix seconds | Earliest allowed start. Must be positive. |
| `deadline` | Unix seconds | Latest allowed start. Must be positive and not before `not_before`. |
| `on_deadline` | `expire` (default), `run`, `cancel` | What happens after the deadline passes. |
| `window` | calendar name | Soft gate: start only while this calendar is available. |
| `requires_calendar_state` | object | Soft gate on a calendar state and its usable interval. |
| `fit` | `timeout` (default), `none`, duration string or integer | How much of a state interval must remain for the run. |

`requires_calendar_state` has `calendar`, `state` (`available` or `unavailable`), `lead` and `tail`.
The usable part of a state interval is `[begin + lead, end - tail)`. A non-`none` `fit` also needs
`timeout_secs + staging_secs + finish_secs`, or the explicit `fit` duration plus that allowance,
to remain. `staging_secs` and `finish_secs` come from `[schedule]` and default to 300 and 60.

```json
{
  "schedule": {
    "not_before": 1764000000,
    "deadline": 1764003600,
    "on_deadline": "expire",
    "window": "nightly",
    "requires_calendar_state": {
      "calendar": "provider-maintenance",
      "state": "unavailable",
      "lead": "5m",
      "tail": "0s"
    },
    "fit": "timeout"
  }
}
```

Submit validates field shape and names. It does **not** search the horizon for a possible start;
`POST /v1/schedule/explain` reports whether the job can fit and why.

## Controller configuration

`[schedule]` is optional. Without it, and without calendar or attachment tables, dispatch has no
calendar gates. Unknown keys are rejected everywhere.

| `[schedule]` key | Default | Validation |
| --- | --- | --- |
| `file` | none | Absolute TOML path without `..`; the file may not name another file. |
| `horizon_secs` | `31622400` (366 days) | Positive integer. How far boundary searches look. |
| `step_budget` | `50000` | Positive integer. Evaluation steps per dispatch poll. |
| `staging_secs` | `300` | Non-negative integer. Staging allowance used by `fit`. |
| `finish_secs` | `60` | Non-negative integer. Finish allowance used by `fit`. |
| `starvation_secs` | `3600` | Positive integer. Age at which a job outranks all static priorities. |
| `calendars` | `[]` | Nested `[[schedule.calendars]]` alias. |
| `attachments` | `[]` | Nested `[[schedule.attachments]]` alias. |

An inline value overrides the same tuning key from `file`; unset keys keep the file or built-in
default. Calendars and attachments from the file, inline `schedule` table and top-level aliases are
merged in that order. Calendar names are unique; an attachment may name only defined calendars.

### Calendars

A calendar has `name`, optional `offset` (`Z`, `UTC`, `+HH:MM`, `-HH:MM`, `+HHMM`, `-HHMM` or integer
minutes within ±14:00), optional `default` (`open` or `closed`), `windows`, `ranges` (alias `dates`)
and `exceptions`. Names are 1–64 characters from `A-Z a-z 0-9 . _ -`.

| Table | Keys |
| --- | --- |
| `[[calendar.windows]]` | `id`, `kind` (`available`/`unavailable`), `from`, `to`, `days` |
| `[[calendar.ranges]]` | `id`, `kind`, `from`, `to`, `reason` |
| `[[calendar.exceptions]]` | Same fields as `ranges`; replaces base rules over its span. |

`from` and `to` in a weekly window are local `HH:MM`; `24:00` is valid only as an end. `to < from`
crosses midnight, and `from == to` is rejected. `days` accepts `*`, `mon-fri`, comma lists such as
`mon,wed,fri`, or an array of weekday names; it names the window's **local start day**. One-off
endpoints accept `YYYY-MM-DD` (midnight) or `YYYY-MM-DDTHH:MM[:SS][Z|±HH:MM]`; an explicit offset
wins over the calendar offset. `to` must be after `from`.

Intervals are half-open. When `default` is omitted, a calendar is open only if it has no `available`
weekly, range or exception rule. `unavailable` always wins over `available`.

### Attachments

`[[attach]]` has `scope`, `name` and `calendars`. Scope is `global`, `group`, `node`, `provider` or
`model`; `global` takes no `name`, all other scopes require one. Every named calendar becomes a hard
gate for matching dispatch candidates. These gates stay enforced even when `on_deadline = "run"`
ignores a job's soft gates.

### Examples

The placeholders below stand for operator-supplied token hashes. These are complete configs; the
same keys can be put in a separate `file` document with the schedule tables at top level.

A weeknight maintenance window that blocks all starts outside 22:00–06:00 local time:

```toml
[controller]
listen = "127.0.0.1:7600"
db = "/var/lib/lu/lu.db"
artifacts_dir = "/var/lib/lu/artifacts"

[[controller.agents]]
name = "node-a"
token_sha256 = "AGENT_TOKEN_HASH"

[[controller.operators]]
name = "ops"
token_sha256 = "OPERATOR_TOKEN_HASH"
schedule = true

[agent]
name = "node-a"
controller_url = "http://127.0.0.1:7600"
token_file = "/etc/ting/ting.token"
work_dir = "/var/lib/ting/work"
harness = "opencode"

[schedule]
horizon_secs = 31622400
step_budget = 50000
staging_secs = 300
finish_secs = 60
starvation_secs = 3600

[[calendar]]
name = "nightly"
offset = "+02:00"
default = "closed"

[[calendar.windows]]
id = "weeknights"
kind = "available"
from = "22:00"
to = "06:00"
days = ["mon", "tue", "wed", "thu", "fri"]

[[attach]]
scope = "global"
calendars = ["nightly"]
```

A provider-specific calendar using the nested aliases, with a one-off outage. The `except` window
is unavailable and therefore wins over the otherwise open calendar:

```toml
[controller]
listen = "127.0.0.1:7600"
db = "/var/lib/lu/lu.db"
artifacts_dir = "/var/lib/lu/artifacts"

[[controller.agents]]
name = "node-a"
token_sha256 = "AGENT_TOKEN_HASH"

[[controller.operators]]
name = "ops"
token_sha256 = "OPERATOR_TOKEN_HASH"

[agent]
name = "node-a"
controller_url = "http://127.0.0.1:7600"
token_file = "/etc/ting/ting.token"
work_dir = "/var/lib/ting/work"
harness = "opencode"

[[schedule.calendars]]
name = "provider-maintenance"
offset = "Z"
default = "open"

[[schedule.calendars.exceptions]]
id = "outage"
kind = "unavailable"
from = "2026-11-25T22:00Z"
to = "2026-11-25T23:30Z"
reason = "provider maintenance"

[[schedule.attachments]]
scope = "provider"
name = "anthropic"
calendars = ["provider-maintenance"]
```

Top-level `[[calendar]]`/`[[calendars]]` and `[[attach]]`/`[[attachments]]` are aliases; nested
`[[schedule.calendars]]`/`[[schedule.attachments]]` are accepted too. Setting both names of one
alias pair at the same level is rejected.

## Admission, deadlines and queue order

Dispatch builds one admission request per node/provider/model candidate and calls the same evaluator
used by `waiting_detail` and `explain`. A blocked candidate reports structured reasons with codes
such as `not_before`, `window`, `requires_state`, `window_too_short`, `gate_global`, `gate_group`,
`gate_node`, `gate_provider`, `gate_model`, `unknown_calendar`, `eval_budget` and `clock`.

`on_deadline = "expire"` or `"cancel"` ends a queued or blocked job as `deadline_missed` once the
deadline passes. Both actions store the summary `deadline missed` and write a `deadline_missed`
audit event. `deadline_missed` is a terminal job state counted by `lu status --json` and
`lu_jobs{state="deadline_missed"}`, but it is excluded from `StatsReport` and the node contribution
metrics. `expired` remains the draft-only TTL state.

`on_deadline = "run"` ignores only the job's soft gates (`window`, `requires_calendar_state` and
`fit`). `not_before`, global/group/node/provider/model attachments, drain and cordon stay hard.

Queued jobs are ordered by starvation class (a job at or past `starvation_secs` outranks every
static priority), effective priority (static priority plus a bounded age boost), `dispatch_seq`
ascending, then `eligible_since` ascending. A per-poll `step_budget` bounds how many candidates are
evaluated. A node drain or an active provider cordon still prevents assignment.

If no candidate fits, the job stays `queued` with a `waiting_reason` and, on single-job views,
structured `waiting_detail`. It does not fail merely because no matching calendar window exists.

## Inspecting scheduling

| Route or command | What it shows |
| --- | --- |
| `GET /v1/schedule?at=<unix-or-utc>` | `configured`, current time, next change, defaults, calendar states and attachments. With no schedule configured it returns `configured: false` and empty arrays. Any non-viewer operator token may read it; schedule routes are not in the viewer allow-list. |
| `GET /v1/schedule/calendars?from=<time>&days=<n>` | Calendar rules plus normalized `available`/`unavailable` intervals. `days` is clamped to 1–366 and defaults to 7. |
| `GET /v1/schedule/calendars/{name}` | The same detail for one calendar; unknown names return `404`. |
| `POST /v1/schedule/explain` body `{"id":"<job-id>","at":<unix>,"node":"<name>"}` | Per-candidate admission decisions, future-fit result and next change. `node` is optional; up to 64 candidates are shown and `truncated` says whether more were omitted. |
| `POST /v1/schedule/reload` | Revalidates the controller config file and atomically swaps the schedule. Requires `schedule = true` on the operator entry in `[[controller.operators]]`; a rejected reload leaves the active schedule unchanged. |
| `lu status --json` | `jobs` counts for all twelve states, including `deadline_missed`. It has no schedule-specific field. |
| `lu ls` / `lu ls --json` | Job list rows, including jobs waiting on a calendar. List views do not populate `waiting_detail`. |
| `lu show <id> --json` | Raw `GET /v1/jobs/{id}` JSON, including `spec.schedule` and structured `waiting_detail`. Plain `lu show` prints the existing key/value rows. |

`/metrics` exports `lu_jobs{state="deadline_missed"}` among the twelve states. It does **not**
export schedule-wait or reload counters; those counters exist only inside the controller process.
See [HTTP API](api.md#operator) for the full route contract and [Configuration](config.md#schedule)
for the config keys.
