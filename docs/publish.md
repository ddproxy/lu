# Publishing

Publishing is an explicit controller-side step that turns a finished job's stored `diff.patch` into a
git commit on a configured remote. It is off until an operator configures `[publish]`, and it never
runs automatically when a job finishes. Nothing is pushed until an operator runs `lu publish <job>`
(or calls `POST /v1/jobs/{id}/publish`). Agents never see a push token and cannot call the publish
route.

The safe default is **no push**. A job gets a publish target only when one of these is true, in this
order:

1. its spec names a configured `publish.remote`;
2. a `[publish.redirects]` pattern maps the job's source remote to a configured remote;
3. the job's source remote is itself configured as a `[publish.remotes.<name>]` URL.

Any other source remote is a typed `denied` error. Redirects never broaden this rule to an
unconfigured remote: config validation rejects a redirect that names an unknown remote.

## Configuration

```toml
[publish]
committer_name = "Lu Publish"
committer_email = "lu-publish@localhost"
allow_scrubbed = false
protected_patterns = ["hotfix-*", "stable"]

[publish.remotes.forge]
url = "https://git.example.net/team/app.git"
token_file = "/etc/lu/publish/forge.token"
branch_prefixes = ["snapshot/", "review/"]
allow_source_branch_push = false
max_pushes_per_hour = 20

[publish.gates.secrets]
command = "/usr/local/bin/lu-gate-secrets --strict"
timeout_secs = 120
expected_exit_code = 0

[publish.redirects]
"git.example.net/other/*" = "forge"
"git.example.net/team/*" = "forge"
```

`url` must be HTTPS with a host and no userinfo. Embedded credentials are refused. `token_file` is
an absolute path to an owner-only regular file (mode `0600` or tighter, no symlink or FIFO, at most
8 KiB after trailing CR/LF is trimmed). The token is read once for the push and is never stored in a
job, view, artifact, log or audit event. See [Configuration](config.md#publish) for every key and
its validation rules.

`branch_prefixes` defaults to `["snapshot/"]`. It bounds snapshot branch names, the
failing-gates variant and the CAS fallback branch. It does **not** bound the mirror-derived name of
a source branch; that name is checked against the protected list instead.

`max_pushes_per_hour` counts successful pushes per remote in a rolling hour. Failed pushes release
their reservation, and the budget lives in controller memory, so it resets on restart. It is a
local backstop, not a forge-side quota.

`test_file_url` exists only for local integration tests. It requires an otherwise valid
`shim.invalid` HTTPS `url` and an absolute `file://` path; do not use it in production.

## Redirects

Source URLs are normalised to `host/owner/name` before matching: a `.git` suffix and trailing slash
are removed, userinfo and ports stay with the host, and local paths are normalised under `local/`.
Patterns use bounded `*` wildcards, for example `git.example.net/team/*`. If more than one pattern
matches, Lu chooses an exact match first, then the pattern with the most literal bytes, then lexical
order. An explicit `publish.remote` always wins over a redirect.

## Job spec

The `publish` object is part of the job spec. The CLI accepts it as JSON on
`lu edit <draft> --publish` and `lu approve <draft> --edit --publish`; operators using the HTTP API
can set it on any submitted job. `[intake].rules[].publish` applies the same object to a drafted job
and is operator-authored configuration, never payload text. Unknown fields are refused.

```json
{
  "branch": "snapshot/<key>",
  "gates": ["secrets"],
  "on_gate_failure": "hold",
  "remote": "forge",
  "target": "snapshot",
  "allow_source_branch": false
}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `branch` | `snapshot/<key>` | Template for a snapshot branch. `<key>` becomes the job's idempotency key, or its job id when no key was given. |
| `gates` | `[]` | Names of `[publish.gates.<name>]` commands to run before the push, in order. At most 16, unique. |
| `on_gate_failure` | `hold` | `hold` makes no push; `push_labelled` pushes `<branch>-failing-gates` for a snapshot when that name is inside an allowed prefix. It is ignored for source-branch pushes. |
| `remote` | — | Name of a `[publish.remotes.<name>]`. Overrides redirects and the source-remote default. |
| `target` | `snapshot` | `snapshot` creates a new branch; `source_branch` fast-forwards the branch the job came from. |
| `allow_source_branch` | `false` | Origin-side allowance for a source-branch push. A webhook payload cannot set it. |

The push commit uses `[publish] committer_name` and `committer_email` for authorship. Lu never signs
as a user. The commit message takes the first line of the task, sanitised and cut to 72 characters,
and adds a `Lu-Job: <job id>` trailer.

## Source-branch pushes

Pushing to the job's own source branch is dangerous: it can move a branch other people are working
from. It requires **all** of the following, checked again at publish time:

1. `allow_source_branch_push = true` on the selected publish remote (off by default);
2. `publish.target = "source_branch"` in the job spec;
3. `publish.allow_source_branch = true` in the job spec or the operator's intake rule;
4. operator approval — the job was submitted without an `origin` and without `draft`, or an operator
   explicitly approved the draft with `lu approve`.

A signed intake event that created a draft cannot satisfy condition 4 until an operator approves it.
`auto_approve` on a trusted intake rule makes the job queued, but it is not an operator approval for
this purpose unless `lu approve` was used. There is no `on_gate_failure` override: a failed gate on a
source-branch push ends in `gates_failed` with no push at all.

The destination branch name is resolved by the controller's mirror from `source.ref`, never from
task text, context or webhook payload:

- a branch ref must still point at the job's pinned commit;
- a `refs/pull/<n>/head` ref maps to the one mirrored local branch whose tip equals the fetched head;
- a fork-only head, an ambiguous head, a bare commit or a tag ref is refused;
- the mirror must be configured for the job's `source.repo`, so the branch is in the same repository.

Built-in protected names are `main`, `master`, `work` and `release*`. Add more with
`[publish] protected_patterns`; entries are branch-name globs without `/`. A protected or invalid
mirror-resolved branch is refused before any push.

Before pushing, Lu reads the remote's advertised tip over the same connection and requires it to
equal the job's pinned `base_commit`. This compare-and-swap (CAS) check is against the job's starting
commit, not a moving branch tip. If the branch moved, Lu does not touch it:

- when the remote allows `snapshot/`, it pushes the same commit to `snapshot/<key>` and records
  `published_snapshot_fallback`;
- otherwise it records `no_snapshot_prefix` and pushes nothing.

The CAS check is a preflight, not an atomic remote lease. A concurrent writer that moves the branch
between the check and the push is outside Lu's control; forge branch protection is required to make
that window safe.

## What a push contains

Every attempt prepares a temporary checkout from the controller mirror at the job's pinned base,
applies the stored `diff.patch`, creates one commit and runs the configured gates in that tree. Gate
commands are direct argv split on whitespace, with a minimal `PATH`, no shell, null stdin and capped
time. A missing gate name, timeout or spawn error counts as a failure.

The push uses exactly one exact refspec, `<commit>:refs/heads/<branch>`:

- no `+` prefix, so the remote enforces fast-forward; Lu never force-pushes;
- no tags, deletes, wildcards or other refs;
- no tracking ref or remote configuration is written in the prepared checkout.

If the remote rejects the update, the ref is unchanged and the publish record ends in `failed`. A
diff changed by artifact scrubbing is refused unless `[publish] allow_scrubbed = true`; enabling that
means publishing the scrubbed bytes, which may not apply cleanly and may omit redaction a reviewer
expects.

## Observability

`lu publish <job-id>` prints `<detail> <remote> <branch> (<target>)`; `--json` prints the full job
view. It requires a finished job. Precondition denials are HTTP errors (`403` for denied targets,
`429` with `Retry-After` for the hourly budget), while runtime failures are stored as publish detail
codes.

`lu show` prints `publish_target` (the effective `remote:branch`, with `(source branch)` for a
source-branch target) and `publish` (the last attempt's detail, remote, branch and target). `lu top`
adds a `publish:<detail>:<branch>` note to affected rows. The stored `PublishRecord` is part of the
jobs table's `publish_json` column in the schema 3 baseline; schema 1 and 2 databases gain the
column in place at startup.

`/metrics` exports `lu_publish_total{target,result}` since controller start. `target` is `snapshot`
or `source_branch`; `result` is one of `hold`, `gates_failed`, `published`,
`published_snapshot_fallback`, `no_snapshot_prefix`, `failed`, `denied`, `invalid`, `state`, `git` or
`rate_limited`. `gates_running` is only the initial stored detail, not a counted outcome. When `[audit] path` is set, every outcome also writes a `publish` event with
`target`, `result`, `remote` and `state` fields. Neither the metric nor the audit event carries a
token, task text or branch text from a payload.

## Token scope and forge protections

The push token needs **contents write** on the target repository. Use a dedicated machine account
whose only job is publishing, with a token scoped to that repository. Do not reuse an operator,
agent, provider or mirror-fetch token. The controller reads the token file at push time and scrubs
the value from git errors before showing them to an operator.

A forge-side rule is required: protect `main`, `master`, `work`, `release*`, your configured
protected patterns and any other branch operators should not move, and require review or a separate
approval for direct pushes. Lu's opt-ins, mirror resolution, CAS check and protected-name list are
defence in depth, not a replacement for branch protection or rulesets. Source-branch publishing is
**not recommended** without those forge layers. See [Security](security.md#publishing) for the
threat model.
