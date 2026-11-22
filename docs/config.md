# Configuration

`lu` reads one TOML file. `--config <path>` selects it; the default is `lu.toml` in the current directory. `lu check` parses and validates the values, prints a summary, database health and the checks it ran, but does not read secret files, load the TLS certificate or bind a port. `lu serve` and `ting` validate the whole file and check its secret files; `lu backup` and `lu restore` read only `[controller] db`.

The file is UTF-8 and limited to 64 KiB. `lu check`, `lu serve` and `ting` reject unknown keys and tables rather than ignoring them; `lu backup` and `lu restore` read only `[controller] db` and do not validate unrelated keys. Every validation error names the table at fault, for example:

```text
[controller]: listen 0.0.0.0:7600 is not a loopback address, so [tls] is required
[[controller.agents]] #1 (node-a): token_sha256 must be 64 lowercase hex characters
[agent]: heartbeat_secs * 2 (30) must be less than controller lease_secs (30)
```

At least one of `[controller]` and `[agent]` must be present. A file may contain both, which is useful for a single-machine setup and for `lu check`.

## `[controller]`

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `listen` | yes | — | Parsed as a socket address, such as `127.0.0.1:7600` or `[::1]:7600`. Host names are not accepted here. A non-loopback address requires a `[tls]` table. |
| `db` | yes | — | Absolute path; must not contain `..`. The parent directory must exist at startup. A new database file is created mode `0600`; the store enables WAL, foreign keys, a 5 s busy timeout and `synchronous=NORMAL`. |
| `artifacts_dir` | yes | — | Absolute path; must not contain `..`. Created at startup if missing; mode is set to `0700` where supported. Job logs and artifacts live under `<artifacts_dir>/<job id>/`. |
| `lease_secs` | no | `60` | Integer `5` to `3600`. A lease is extended by this many seconds. Maintenance runs every `max(1, lease_secs / 3)` seconds. |
| `max_retries` | no | `1` | Integer `0` to `3`. Node failures retry while `attempt <= max_retries`; `0` means no retry. Provider failures use a separate 100-attempt limit. Execution failures never retry. |
| `provider_cooldown_secs` | no | `1800` | Integer `60` to `86400`. How long a `(node, provider)` pair is cordoned after a retryable provider failure without `retry_after_secs`. |
| `recovery_grace_secs` | no | `30` | Integer `0` to `600`. How long a job stays `recovering` before it is requeued or failed. |
| `blocked_dependent_ttl` | no | `24h` | Duration `1s` to `365d`. How long a dependent blocked by a failed or cancelled predecessor waits for a retry before failing with `failure_class: "dependency"`. |
| `max_connections` | no | `256` | Integer greater than `0`. Global concurrent connection cap. |
| `max_connections_per_addr` | no | `64` | Integer `1` to `max_connections`. Concurrent connection cap per remote IP. |
| `local_repo_roots` | no | `[]` | Absolute local paths, without `..`, that the controller may read job bases from. Each must exist as a directory at startup. With no roots, only configured repository mirrors can serve `GET /v1/agent/jobs/{id}/base`. |
| `agents` | no | `[]` | Array of `[[controller.agents]]` tables. With no agent entries, no agent token can authenticate. |
| `operators` | no | `[]` | Array of `[[controller.operators]]` tables. With no operator entries, no operator token can authenticate. |
| `repos` | no | `[]` | Array of `[[controller.repos]]` tables. A repository listed here is mirrored by the controller and jobs for its `url` are pinned to a resolved commit. A repository not listed behaves as before. |
| `groups` | no | `[]` | Array of `[[controller.groups]]` tables. Each caps the number of concurrent jobs naming the same `--group`. Unnamed groups and jobs without a group are unlimited. |

### `[[controller.agents]]`

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `name` | yes | — | 1 to 64 characters from `A-Z a-z 0-9 . _ -`; unique among agents. It must also be a valid node name, and the agent token is bound to it. |
| `token_sha256` | yes | — | Exactly 64 lowercase hexadecimal characters. Unique across all agents and operators. |

### `[[controller.operators]]`

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `name` | yes | — | 1 to 64 characters from `A-Z a-z 0-9 . _ -`; unique among operators. |
| `token_sha256` | yes | — | Exactly 64 lowercase hexadecimal characters. Unique across all operators and agents. |

A token must not work in both scopes. Reusing a hash across the two arrays is refused.

### `[[controller.repos]]`

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `url` | yes | — | `https` URL with a host and no userinfo (no `user:password@`). `http` is refused. A `file://` URL is accepted only with `allow_file_urls = true`. Jobs whose `repo` equals this string are pinned. |
| `name` | yes | — | 1 to 64 characters from `A-Z a-z 0-9 . _ -`; unique among repositories. It also names the mirror directory. |
| `credential_file` | no | — | Non-empty path when set. A private file holding `username:password` or a single token line, checked like `token_file` on `load` (regular file, owner-only, no symlink or FIFO, at most 64 KiB). The controller reads it only while fetching, when it is capped at 8 KiB. The value never enters the git command line, a log or an error. |
| `refs` | no | `["*"]` | Non-empty glob allow-list of branches and tags, using `*` and `?`. A ref outside the list resolves as unknown. |
| `allow_file_urls` | no | `false` | Test-only. Allows `file://` repository URLs; do not enable it in production. |

Each mirror is a bare repository at `<artifacts_dir>/../mirrors/<name>.git`; the `mirrors` directory is created mode `0700`. A sync fetches the named ref with restricted exact refspecs and is limited to 5 minutes and 10 GiB. The controller fetches with `git` using only the configured transport, no terminal prompts, no system or global config, no hooks and no inherited askpass or command-transport variables. `credential_file` is supplied through a short-lived private askpass helper that is removed after the fetch.

A job submitted for a configured repository is pinned to the commit its `source.ref` names. The
mirror fetches that named ref with restricted exact refspecs, never `+refs/*`, so a ref such as
`refs/pull/<n>/head` or `refs/pull/<n>/merge` can be requested without publishing every ref in the
remote. Intake also refuses a fork head unless its rule sets `allow_forks = true`. The controller syncs first
when the mirror has never been synced, is older than 60 seconds, or does not know the ref. An
unknown ref is `422 unprocessable` and names the ref. The resolved commit is stored on the job;
refspecs, credentials and tokens never appear in logs, artifacts, `lu show` or the audit record.
Jobs for repositories not in this list are unchanged.

### `[[controller.groups]]`

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `name` | yes | — | 1 to 64 characters from `A-Z a-z 0-9 . _ -`; unique among groups. Matches a job's `--group` exactly. |
| `max_running` | yes | — | Integer `1` to `64`. At most this many jobs in the group may be `leased`, `running`, `recovering` or `needs_input`. |

The cap is applied inside the dispatch transaction, so concurrent polls cannot exceed it. A queued job waiting on a full group reports `group limit (<name> <running>/<max>)`. An operator can change a cap at runtime with `PUT /v1/groups/{name}/limit`, including for a group with no configured entry. Runtime caps live in controller memory and return to the configured values on restart. Metrics expose `lu_group_running{group}` and `lu_group_limit{group}` for each configured or runtime-capped group.

## `[mirror]`

Controller-side settings for named git sources. Jobs carry a `source` object with `repo`, `ref` and
optional `base`; the mirror resolves the ref to a commit that is pinned on the job. The only key on
`[mirror]` is `credentials`; fork acceptance is an intake rule decision, not a mirror setting.

### `[mirror.credentials]`

Private repository credentials live only in the controller config, never in a job spec, agent
config, log, artifact, `lu show` output or audit record. Each entry in `repos` names a configured
mirror and a token file.

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `repos` | no | `[]` | Array of `[[mirror.credentials.repos]]` entries. |

### `[[mirror.credentials.repos]]`

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `name` | yes | — | A `[[controller.repos]]` name, its URL, or a normalized repo id (`host/owner/repo`); 1 to 512 characters from `A-Z a-z 0-9 . _ - / :`. A configured entry takes precedence over that repo's `credential_file`. |
| `token_file` | yes | — | Non-empty path, checked like other secret files: regular file, owned by the effective user, mode `0600` or tighter, no symlink or FIFO. The token is passed to git through a private helper and scrubbed from errors. |

```toml
[mirror.credentials]
repos = [{ name = "app", token_file = "/etc/lu/app.token" }]
```

## `[agent]`

The `[agent]` table describes an agent host. Operator commands never read it: they use `LU_URL`/`LU_TOKEN_FILE` or an operator file.

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `name` | yes | — | 1 to 64 characters from `A-Z a-z 0-9 . _ -`. It must match the controller's agent entry. |
| `controller_url` | yes | — | `http` or `https` URL with a host. No credentials, query or fragment. `http` is allowed only for `localhost` or a loopback IP. A path is allowed; the client rejects `.` or `..` path segments. |
| `token_file` | yes | — | Non-empty path. On `load`, it must be a regular file, owned by the effective user, with no group or other permission bits, no symlink and no FIFO; at most 64 KiB. The CLI trims trailing CR/LF from the token text. |
| `ca_file` | no | — | Non-empty path when set. On `load`, the client reads up to 1 MiB of PEM certificates and adds them to the system trust roots; it does not replace the system roots. `lu check` does not read it. |
| `work_dir` | yes | — | Absolute path; must not contain `..`. The agent creates it recursively with mode `0700` at start when it is missing. An existing path must be a real directory, not a symlink; staging rejects `/` and bare top-level directories such as `/srv`. It is also the root for the job workspaces described in [workspaces.md](workspaces.md). |
| `harness` | yes | — | Non-empty. The agent runs `opencode` job specs only; a spec naming another harness fails with `execution`. A non-`opencode` configured value is ignored with a warning. |
| `base_source` | no | `auto` | `clone`, `controller` or `auto`. For a job with `base_commit`, `controller` fetches a pack from the controller and fails the job on `404`; `auto` falls back to cloning the spec's `repo` on `404`; `clone` never asks. Local bases come from paths under `local_repo_roots`; roots and candidates are canonicalised so symlinks cannot escape. |
| `opencode_server` | no | — | Config requires a non-empty value. When a job starts, the opencode launcher also requires `http://` or `https://`, no whitespace or control characters, no leading `-`, and at most 2048 bytes. |
| `workspace_group` | no | — | A group name or number, 1 to 32 letters, digits, `_`, `-` or `.`. Each job's working tree is given to this group with group read, write and the setgid bit; `.git` is left as git made it. It supports a harness that runs as another account while the agent runs as the account that can use git. Resolved from `/etc/group` at start; an unknown group stops the agent. |
| `harness_uid` | no | — | Numeric uid of the account the harness runs as. When set, the agent chowns the per-job socket root and each private socket directory/socket to this account and checks `SO_PEERCRED` against it. Unset means the harness shares the agent's uid. The agent needs `CAP_CHOWN` (or root) to use a different uid. |
| `harness_gid` | no | `workspace_group` or `harness_uid` | Numeric gid for the per-job socket files and directories. Requires `harness_uid`. |
| `opencode_password_file` | no | — | A private file (a regular file owned by the agent's account, no group or other access) whose single line is the opencode server's password. The agent reads up to 4 KiB at start, trims trailing CR/LF, rejects control characters and passes it to the harness as `OPENCODE_SERVER_PASSWORD`. It is checked at agent start, not by config validation. |
| `harness_wrapper` | no | `[]` | A command that wraps the harness, for example `["podman", "exec", "-i", "--workdir", "{cwd}", "--env", "OPENCODE_SERVER_PASSWORD", "opencode-agent"]`, so it runs inside a container. `{cwd}` becomes the job's working tree. Entries must be non-empty text without control characters. The wrapper runs directly, never through a shell. |
| `heartbeat_secs` | no | `15` | Integer `2` to `300`. When a `[controller]` table is also present, `heartbeat_secs * 2` must be strictly less than `lease_secs`. |
| `on_stuck` | no | `none` | One of `none`, `cancel`, `retry`. Default response for jobs whose spec omits `on_stuck`; `none` leaves stuck jobs running. |
| `stuck_after_secs` | no | `1800` | Integer `60` to `86400`. Default continuous stuck interval for jobs whose spec omits `stuck_after_secs`. |
| `reiki_health_url` | no | — | Loopback `http` or `https` URL with no credentials, query or fragment. Requires `reiki_token_file`. The agent reads the local Reiki's peer health (`GET /v1/health`) every 10 seconds and sends each upstream as a provider (`ok`, `cooling` with seconds left, `disabled` or `unknown`) with every heartbeat, plus the document itself. A job that names a `provider` goes only to nodes that report it. After three failed reads the node reports no providers. |
| `reiki_token_file` | no | — | Non-empty when set; required when `reiki_health_url` is set, and rejected when set alone. Checked like `token_file` on `load`; the agent reads at most 4 KiB. |
| `provider_models` | no | `[]` | Up to 64 `[[agent.provider_models]]` pairs, in preference order. Each provider is 1 to 64 characters from `A-Z a-z 0-9 . _ -`; each model is 1 to 128 bytes; provider names are unique. See below. |
| `labels` | no | `[]` | At most 32 labels. Each is 1 to 64 characters from `A-Z a-z 0-9 . _ - /`, with at most one `=`. Lu never interprets them; a job's `requires` must match every label exactly. |
| `label_files` | no | `[]` | At most 8 absolute paths without `..`. Each file is read again before every heartbeat; a missing or unreadable file contributes nothing. Each file is capped at 4 KiB, must be UTF-8, and holds one label per line; blank lines and lines starting with `#` are ignored. Invalid labels are dropped with one warning per label. File labels are merged with `labels`, then sorted and de-duplicated. |
| `detect` | no | `[]` | Up to 16 `[[agent.detect]]` entries; labels must be valid and unique. See below. |
| `taints` | no | `[]` | At most 8 taints. Each is a label with the same syntax as `labels`. A tainted node takes only jobs that tolerate every taint. Sent with every heartbeat and replaced on change. |
| `housekeeping` | no | — | Optional `[agent.housekeeping]` table. Without it the agent removes nothing. |

An explicit job spec field overrides both stuck defaults. Stuck uses the same output/CPU
observation as `lu node`: both idle counters must exceed 300 seconds, and that observation must
stay continuous for `stuck_after_secs` before the agent acts. See
[operations.md](operations.md#stuck-jobs).

Labels let one controller route work to particular hosts without a new job field for every
machine property. For example:

```toml
[agent]
# ...
labels = ["gpu", "php8.3", "region=eu"]
label_files = ["/etc/lu/labels.d/installed-software"]
taints = ["homelab"]
```

### `[[agent.provider_models]]`

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `provider` | yes | — | 1 to 64 characters from `A-Z a-z 0-9 . _ -`; unique among pairs. |
| `model` | yes | — | 1 to 128 bytes. |

Pairs are published with every heartbeat only while Reiki reports the named provider healthy. Dispatch tries them in configuration order for jobs submitted with `--provider any`. No schema change: pairs live in the stored heartbeat.

```toml
[[agent.provider_models]]
provider = "anthropic"
model = "claude-sonnet-4-5"

[[agent.provider_models]]
provider = "openai"
model = "gpt-5"
```

### `[[agent.detect]]`

Each entry runs a probe that publishes its label while the command exits 0.

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `label` | yes | — | A valid label, unique across all probe entries. |
| `command` | yes | — | argv array of 1 to 8 entries: an absolute program path without `..`, or a bare name resolved against the agent's own `PATH`. Arguments must be non-empty text without control characters. No shell. |
| `timeout_secs` | no | `5` | Integer `1` to `30`. The process group is terminated, then killed, on timeout. |
| `every_secs` | no | `300` | Integer at least `30`. |

Probes start with the agent, then run at `every_secs`. A run never overlaps itself; heartbeats
use the last result and never wait. The child runs as the agent account with only `PATH`, `HOME`
and `LANG` inherited, null stdin, at most 4 KiB of discarded output, and its own process group.
Exit 0 publishes the label; any other result or a spawn failure removes it, with one log line on
change. Probe labels join static and file labels under the same 32-label cap; probe labels are
dropped last when the cap is full and are marked `probed` in the node's `label_sources`.

```toml
[[agent.detect]]          # Rust
label = "rust/cargo"
command = ["cargo", "--version"]

[[agent.detect]]          # Go
label = "go/toolchain"
command = ["go", "version"]

[[agent.detect]]          # PHP
label = "php/cli"
command = ["php", "-v"]
```

### `[agent.housekeeping]`

Opt-in removal of old scratch directories, run by the agent at start and between jobs only.

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `scratch_globs` | no | `[]` | At most 8 absolute patterns without `..`. Only the final path component may contain `*` or `?`, so a pattern cannot match above its parent directory. |
| `min_age_hours` | no | `6` | Only a directory tree whose newest modification time is at least this many hours old is removable. |
| `min_free_percent` | no | `15` | Act only while the filesystem holding the pattern's parent has less free space than this. `0` always acts on entries old enough by age. |
| `max_remove_per_pass` | no | `10` | At least 1. The most directories removed in one pass. |

```toml
[agent.housekeeping]
scratch_globs = ["/srv/work/target-*"]
min_age_hours = 6
min_free_percent = 15
max_remove_per_pass = 10
```

A pass runs at agent start and after each finished job, never while a job is running, and at
most once every 10 minutes. It removes directories only, oldest first, and stops when the
filesystem is above `min_free_percent + 5` or `max_remove_per_pass` is reached. It never
follows a symlink, never removes a pattern's parent, and never touches the configured
`work_dir` or anything inside it. Job workspaces inside `work_dir` use the separate
`[workspace]` lifecycle below.

## `[workspace]`

Optional top-level table for the Ting agent's CI-style workspace retention and disk pressure
rules. Omitting the table keeps all documented defaults. Unknown keys are rejected; a
`[agent] workspace` key is also unknown, because this table is not nested under `[agent]`.
The workspace root is `[agent] work_dir`. See [workspaces.md](workspaces.md) for the policy
semantics and an Ansible example.

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `default` | no | `ephemeral` | Policy for jobs omitting `spec.workspace`: `ephemeral`, `keep_on_failure` or `keep`. |
| `failed_ttl` | no | `6h` | Duration a failed `keep_on_failure` workspace is kept. |
| `keep_ttl` | no | `48h` | Duration a `keep` workspace is kept. |
| `max_bytes` | no | none | Positive integer cap on total workspace bytes. Eviction starts above the cap. |
| `low_water_percent` | no | `65` | Disk use percent at or below which eviction stops. |
| `high_water_percent` | no | `80` | Disk use percent above which eviction starts. |
| `sweep_interval` | no | `60s` | How often TTLs, retries and disk pressure are checked. |

Durations accept a trailing `s`, `m`, `h` or `d`, or a bare number of seconds, and must be at
least `1s`. Water marks are integers 0 to 100 with `low_water_percent < high_water_percent`.
A zero `max_bytes` is refused.

```toml
[workspace]
default = "ephemeral"
failed_ttl = "6h"
keep_ttl = "48h"
max_bytes = 53687091200
low_water_percent = 65
high_water_percent = 80
sweep_interval = "60s"
```

## `[stack]`

Agent-side, opt-in conflict policies for dependency chains. Omit the table to keep every conflicted
path fail-closed. Unknown keys are rejected. The semantics and a worked chain are in
[dependencies.md](dependencies.md#policy-paths-for-known-safe-conflicts).

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `union_paths` | no | `[]` | At most 16 repo-relative globs of at most 256 bytes each, with `*`, `**` and `?`; no character classes, backslashes or `.`/`..` components. A conflict on a matching path keeps both sides' added lines in order and base deletions. |
| `base_paths` | no | `[]` | Same bounds as `union_paths`; no pattern may appear in both lists. A conflict on a matching path takes the base side, for generated lock files. |

A conflict on any path not covered by one of these lists fails closed with
`failure_class: "conflict"`. A covered conflict's summary names every file and hunk, records
whether the base moved since the dependency was staged, and lists the policy paths used.

A job can set `stack_union_paths` and `stack_base_paths` in its `JobSpec`. Those fields can only
narrow the agent's lists: every entry must already be in the corresponding `[stack]` list, and the
two per-job lists must not overlap. An attempt to widen or invent a policy path is refused; the
agent never grants a job a conflict policy its operator did not configure.

```toml
[stack]
union_paths = ["CHANGELOG.md", "docs/*.md"]
base_paths = ["Cargo.lock", "go.sum"]
```

## `[tls]`

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `cert` | yes | — | Non-empty certificate path. `lu serve` loads it as PEM; `lu check` does not read it. |
| `key` | yes | — | Non-empty key path. Loaded as PEM. On `load`, the file must be a regular file owned by the effective user, with no group or other permission bits, no symlink and no FIFO. |

A `[tls]` table makes the controller serve HTTPS. It is required when `listen` is not a loopback address. If the table is present with only `cert` or only `key`, parsing fails.

## `[log]`

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `level` | no | `info` | One of `error`, `warn`, `info`, `debug`, `trace`. `RUST_LOG` overrides it at runtime when set. |
| `format` | no | `text` | `text` or `json`. |
| `stderr` | no | `true` | Boolean. Logging also goes to stderr when `file` is unset, even if this is `false`. |
| `file` | no | — | Non-empty path when set. Opened append-only; created with mode `0640` when absent, while existing permissions are left unchanged. A symlink at the path is refused. |

## `[artifacts]`

Controller-side redaction of bytes before they reach the artifact directory. The table is optional. When it is absent, scrubbing is on and only the built-in rules run.

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `scrub` | no | `true` | Boolean. When `false`, the controller stores uploads unchanged. |
| `extra_rules` | no | `[]` | Up to 16 operator regexes. Each entry needs `name` and `pattern`. |

Each `[[artifacts.extra_rules]]` entry has these fields.

| Field | Validation |
| --- | --- |
| `name` | 1 to 32 bytes from `A-Z`, `a-z`, `0-9`, `_` and `-`. It becomes the `[scrubbed:<name>]` tag. Built-in names are reserved. |
| `pattern` | Non-empty, at most 512 bytes, and single-line. It must compile as an unanchored Rust regex. |

Extra rules run after the built-ins. Names must not collide with each other or with a built-in name. `lu check` compiles every pattern and reports the rule name on failure.

The built-in rule names are `private_key`, `bearer_token`, `token_prefix`, `jwt` and `assignment`. They describe value shapes, not token issuers. See [security.md](security.md#artifact-scrubbing) for the matched patterns and patch handling.
## `[audit]`

Optional append-only, hash-chained JSON Lines record of job lifecycle events written by the controller. Leave the table out to disable auditing: no file is opened or created and no audit work is done.

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `path` | yes | — | Absolute path; must not contain `..`. The controller appends to this file and creates it with mode `0600` where supported. A startup open error stops `lu serve` with a clear message. |

Events written are `submitted`, `leased`, `started`, `finished`, `failed`, `retried`, `cancelled`, `cordon_set`, `cordon_cleared`, `drain`, `collect_applied`, `scope_action`, `intake`, `draft_approved`, `draft_rejected`, `draft_expired` and `publish`. Each line has a sequence number, UTC time, event, job id (where applicable), node, actor (operator token name, node name or `job:<id>` for a scoped action), a small data object of allow-listed scalar fields, `prev` (hex SHA-256 of the previous line's bytes, 64 zeroes for the first line) and `hash` (hex SHA-256 of the line without `hash`). The data object never contains task text, prompts, artifact content or tokens. A `publish` event carries `target`, `result`, `remote` and `state`. See [Audit record](audit.md) for the guarantees and for `lu audit verify|tail|head`.

## `[publish]`

Optional controller-side settings for publishing a finished job's diff. Without the table there is no publish route capability: `lu publish` reports that publishing is not configured. Every remote defaults to no push, and an explicit operator action is always required. The full workflow, target precedence, source-branch opt-ins and forge recommendations are in [Publishing](publish.md).

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `remotes` | no | `{}` | Map of `[publish.remotes.<name>]` entries. Names follow the usual 1 to 64 character name rule and are unique. |
| `gates` | no | `{}` | Map of `[publish.gates.<name>]` entries, at most 16. |
| `committer_name` | no | `Lu Publish` | 1 to 128 printable characters without `<` or `>`. Used as the push commit author and committer. |
| `committer_email` | no | `lu-publish@localhost` | A printable address containing `@`, at most 256 bytes, without `<` or `>`. |
| `allow_scrubbed` | no | `false` | Publish a diff that artifact scrubbing changed. Off by default because the published tree differs from the reviewed patch. |
| `redirects` | no | `{}` | Map of source patterns to remote names. Patterns are bounded `host/owner/name` globs with `*`; each value must name a configured remote. |
| `protected_patterns` | no | `[]` | Extra branch-name globs that no source-branch push may update. Built-ins are `main`, `master`, `work` and `release*`. Entries are at most 256 bytes of alphanumeric characters, `.`, `_`, `-`, `:` and `*`, without `..`, a leading or trailing `/`, or any `/`. |

Redirect keys are matched against the source repository normalised to `host/owner/name` (`.git` and a trailing slash removed, a port kept with the host, and local paths under `local/`). An exact key wins; otherwise the pattern with the most literal bytes wins, with lexical order as a tie-breaker.

### `[publish.remotes.<name>]`

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `url` | yes | — | `https` URL with a host and no userinfo (no embedded credentials). `http` is refused. |
| `token_file` | yes | — | Absolute path without `..`. Checked on `lu serve` as an owner-only regular file: no symlink or FIFO, mode `0600` or tighter. Read at push time with an 8 KiB cap. |
| `branch_prefixes` | no | `["snapshot/"]` | Non-empty list of branch prefixes. Each is at most 128 bytes, ends in `/`, does not start with `/` or `refs/`, and contains no `..`, `@` or control characters. Bounds snapshot, failing-gate and fallback branches. |
| `allow_source_branch_push` | no | `false` | Dangerous opt-in. Allows a fully approved job to fast-forward its own mirror-derived source branch on this remote. |
| `max_pushes_per_hour` | yes | — | Integer `1` to `10000`. Successful pushes per remote in a rolling hour, kept in controller memory. |
| `test_file_url` | no | — | Test-only absolute `file://` path. Requires `url` to use the `shim.invalid` host; never use it in production. |

### `[publish.gates.<name>]`

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `command` | yes | — | 1 to 4096 bytes, direct argv split on whitespace, no shell. Control characters other than space and tab are refused. |
| `timeout_secs` | yes | — | Integer `1` to `3600`. Wall-clock limit for one gate run. |
| `expected_exit_code` | yes | — | Integer `0` to `255` that counts as a pass. |

A gate runs against the prepared publish tree with a minimal `PATH`, null stdin and discarded output. Its recorded result is `pass`, `fail` or `error` with an exit code only when the child reported one. See [Publishing](publish.md#source-branch-pushes) for what a gate failure does.

```toml
[publish]
protected_patterns = ["hotfix-*"]

[publish.remotes.forge]
url = "https://git.example.net/team/app.git"
token_file = "/etc/lu/publish/forge.token"
allow_source_branch_push = false
max_pushes_per_hour = 20

[publish.redirects]
"git.example.net/team/*" = "forge"
```

## `[scope]`

Controller-side limits for harness-initiated follow-up jobs. Omit the table to keep the capability off: no scoped route is reachable and scoped calls return `limit_exceeded("scope_enabled")`. An operator grants a single job the right to spawn with `lu submit --allow-spawn`; that bit is in the job spec and, together with `[agent] harness_uid`, controls whether a per-job socket is created. See [Job-scoped capability](scope.md) for the threat model and the agent socket.

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `enabled` | no | `false` | Boolean. Master switch for `/v1/scope/{route}`. A scoped call while off is a typed `limit_exceeded` naming `scope_enabled`; the agent's socket is gated by the job's `allow_spawn` bit instead. |
| `max_children_per_job` | no | `8` | Integer at least `1`. Direct children per parent. |
| `max_descendants_per_root` | no | `32` | Integer at least `max_children_per_job`. All descendants sharing one `root_job_id`. |
| `max_depth` | no | `2` | Integer `1` to `16`. Lineage depth from the root; children at the cap cannot spawn again. |
| `max_task_bytes` | no | `65536` | Integer `1024` to `262144`. Task text of one child. |
| `max_spawn_per_minute` | no | `6` | Integer at least `1`. Scoped `submit_child` calls per parent job in a rolling minute on the controller. |
| `max_artifact_read_bytes` | no | `1048576` | Integer `4096` to `10485760`. Bytes returned by one `get_child_artifact` call. |
| `max_summary_bytes` | no | `4096` | Integer `256` to `4096`. Maximum length of a child or dependency summary shown through scope tools. |

A limit breach is never a silent drop: it returns `limit_exceeded` with the limit name and increments `lu_scope_limit_exceeded_total{limit}`.

```toml
[scope]
enabled = true
max_children_per_job = 8
max_descendants_per_root = 32
max_depth = 2
max_task_bytes = 65536
max_spawn_per_minute = 6
max_artifact_read_bytes = 1048576
max_summary_bytes = 4096
```

## `[drafts]`

Optional retention and capacity rules for drafts created by [intake](intake.md) or by an operator.
A draft is stored and visible but never dispatched until approval.

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `ttl` | no | `7d` | Duration from 1s to 365d. A draft still in `draft` after this interval moves to `expired`, which is terminal. |
| `max_drafts` | no | `10000` | Positive integer total cap on stored drafts. |
| `max_drafts_per_source` | no | `1000` | Positive integer cap per `origin` label; must not exceed `max_drafts`. |

When either draft cap is full, the oldest drafts are moved to `expired` first regardless of their
TTL; expired drafts remain stored. If that does not free room under both caps, a new draft is
refused: intake answers `429 limit_exceeded` with `limit: max_drafts` and counts the event as
`invalid`. `lu drafts` shows the inventory and `lu top` shows the draft count and oldest draft age.
See [drafts.md](drafts.md#limits-and-expiry).

```toml
[drafts]
ttl = "7d"
max_drafts = 500
max_drafts_per_source = 100
```

## `[intake]`

Optional top-level table that enables the unauthenticated intake route. With no `[intake]` table,
no intake route is installed. A present table requires at least one source, and every source needs
at least one rule. Configuration details, the event format and the trust model are in
[intake.md](intake.md).

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `sources` | no | `[]` | Array of `[[intake.sources]]` entries. |

### `[[intake.sources]]`

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `name` | yes | — | 1 to 64 characters from `A-Z a-z 0-9 . _ -`; unique among sources. It is the path segment of `POST /v1/intake/<source>`. |
| `secret_file` | yes | — | Secret file holding the HMAC-SHA256 key; checked like other secret files and read with `O_NOFOLLOW`. |
| `header` | no | `X-Hub-Signature-256` | Header carrying the signature. |
| `max_body_bytes` | no | `1048576` | Raw body cap, 1 to 16777216 bytes; oversized or malformed bodies get `400 invalid delivery` and are counted as `invalid`. |
| `rate_per_minute` | no | `60` | Per-source budget, 1 to 100000; excess gets `429 rate_limited` and is counted as `rate_limited`. |
| `dedupe_header` | no | — | Optional header supplying a delivery id for the dedupe key. Duplicate deliveries are dropped as `duplicate`. |
| `dedupe_ttl_secs` | no | `86400` | How long a delivery id is remembered, 1 to 604800 seconds. |
| `allow_plain_http` | no | `false` | Escape hatch for a loopback listener with a TLS-terminating reverse proxy or tunnel. Not for a directly exposed non-loopback listener. |
| `rules` | yes | — | At least one `[[intake.sources.rules]]` entry. |

With `[intake]` configured, startup fails if neither TLS nor a loopback bind is available unless
`allow_plain_http = true`. The signature is the only credential on the intake route; no operator
or agent token is accepted there, and no payload text is logged.

### `[[intake.sources.rules]]`

Rules run in configuration order. All conditions in `match` must hold; an empty `match` matches any
body. The first matching rule creates a draft unless its `allow` conditions fail or a fork is
detected with `allow_forks = false`, in which case the event is dropped and counted and never
becomes a draft, even with `auto_approve`.

| Key | Required | Default | Meaning |
| --- | --- | --- | --- |
| `match` | no | `[]` | Conditions on JSON pointers; all must hold. Operators: `equals`, `in`, `prefix`, bounded RE2-style `regex`, `exists`. |
| `allow` | no | `[]` | Additional conditions, typically an actor allowlist. A failure drops the event. |
| `repo` | yes | — | JSON pointer or literal repository URL for `source.repo`. |
| `ref` | yes | — | JSON pointer or literal ref for `source.ref`. |
| `base` | no | — | Optional JSON pointer or literal base commit for `source.base`. |
| `task_template` | no | `""` | Operator-authored task text. Literal by default; `{{pointer}}` substitution requires `unsafe_inline = true` and a pointer in `task_vars`. |
| `task_vars` | no | `[]` | Pointers usable in `task_template` and `key_template`; values are length-capped and stripped of control characters. |
| `context_from` | no | `[]` | Pointers copied into the job's external `context`, capped at 32 KiB with a truncation marker. |
| `provider`, `model`, `priority`, `group`, `timeout` | no | — | Defaults applied to the drafted job's spec. |
| `publish` | no | — | Publish settings applied to the drafted job. |
| `key_template` | no | — | Template for the dedupe/idempotency key and the job's `origin_ref`; may substitute `task_vars`. |
| `auto_approve` | no | `false` | When true, a passing event is created `queued` (or `blocked` if dependencies are unmet) instead of `draft`. |
| `allow_forks` | no | `false` | Refuse refs whose head repository differs from the target unless enabled. |
| `unsafe_inline` | no | `false` | Dangerous. When true, `task_template` may substitute payload fields into `task`; by default payload fields go only into `context`. |

```toml
[intake]

[[intake.sources]]
name = "events"
secret_file = "/etc/lu/events.key"
max_body_bytes = 65536
rate_per_minute = 30
dedupe_header = "X-Delivery-Id"

[[intake.sources.rules]]
match = [{ pointer = "/action", equals = "opened" }]
allow = [{ pointer = "/actor/login", in = ["sam", "kim"] }]
repo = { pointer = "/repository/full_name" }
ref = { pointer = "/ref" }
task_template = "Triage the reported issue and update the docs."
context_from = ["/head/repo", "/actor"]
auto_approve = false
unsafe_inline = false
```

## Cross-table rules

- At least one of `[controller]` and `[agent]` must exist.
- Every `token_sha256` must be unique across all agents and operators.
- Agent, operator, repository and intake source names are unique within their own arrays.
- `[[mirror.credentials.repos]]` names are unique; a credential applies to a repo when its name matches the repo name, URL or normalized repo id. Every `[[intake.sources]]` has at least one rule.
- `[stack] union_paths` and `[stack] base_paths` do not overlap, and each job's `stack_union_paths`/`stack_base_paths` are subsets of the corresponding configured list.
- When both tables are present, `heartbeat_secs * 2 < lease_secs`.

## Paths and secret files

- `db`, `artifacts_dir`, `work_dir`, `[audit] path`, each `local_repo_roots` entry, each `label_files` entry and each `scratch_globs` pattern must be absolute and must not contain a `..` component.
- `[tls] cert`, `[tls] key`, `token_file`, `ca_file`, `reiki_token_file`, `credential_file`, `[mirror.credentials]` token files, `[publish.remotes.<name>]` token files, each `[[intake.sources]].secret_file`, `opencode_password_file` and `[log] file` need not be absolute. Publish token files are the exception: their own table requires an absolute path.
- Each secret read opens the path once with `O_NOFOLLOW` and `O_NONBLOCK`, then checks the open handle; a startup check and later use may open the same file separately. Secret files must be regular files, owned by the effective user, with no group or other permission bits (mode `0600` or tighter). A file with mode `0640` is refused for a secret. `lu serve` and `ting` check `[tls] key`, `token_file`, `reiki_token_file`, `credential_file`, publish token files and intake signing keys on load; `opencode_password_file` is checked when the agent starts.
- The config file itself has no secret-file mode check; it normally contains only hashes and paths. Protect it anyway because it names the token and TLS key locations.

## Examples

The repository's root `lu.toml` is a loopback controller example with demo token hashes. `contrib/lu-controller.toml` is a deployment sample for `/etc/lu/lu.toml`, and `contrib/ting.toml` is the agent sample for `/etc/ting/ting.toml`. Worked intake rules are in [intake.md](intake.md#example), and draft retention examples are in [drafts.md](drafts.md#limits-and-expiry).

Minimal controller:

```toml
[controller]
listen = "127.0.0.1:7600"
db = "/var/lib/lu/lu.db"
artifacts_dir = "/var/lib/lu/artifacts"

[[controller.agents]]
name = "node-a"
token_sha256 = "0000000000000000000000000000000000000000000000000000000000000000"

[[controller.operators]]
name = "ops"
token_sha256 = "1111111111111111111111111111111111111111111111111111111111111111"

[[controller.repos]]
name = "app"
url = "https://git.example.net/team/app.git"
credential_file = "/etc/lu/app.credential" # optional; username:password or a token
refs = ["main", "release/*"]
```

Minimal agent:

```toml
[agent]
name = "node-a"
controller_url = "https://lu.example.net:7600"
token_file = "/etc/ting/ting.token"
work_dir = "/var/lib/ting/work"
harness = "opencode"
```
