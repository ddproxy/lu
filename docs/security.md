# Security

This page describes what Lu protects, what it trusts, and the controls and trust boundaries in
place. It matches the code in this tree. The controller, agent (`ting`) and operator CLI are
implemented. The agent and harness sections cover the deployment and HTTP contract used by this
tree. See [operations.md](operations.md) for deployment details.

## Assets

| Asset | Where it lives | Why it matters |
| --- | --- | --- |
| Agent and operator tokens | token files on the host that uses them | A token is a bearer credential; possession is authorisation. |
| Token hashes | controller config (`token_sha256`) | They are not secrets, but changing one revokes a token. Protect the config so an attacker cannot add a hash. |
| TLS private key | controller host, `[tls] key` | Anyone with it can impersonate the controller. |
| SQLite database | controller host, `[controller] db` | Job specs and tasks, state, leases, epochs, idempotency keys, node state. |
| Job logs and artifacts | controller host, `artifacts_dir/<job id>/` | Diffs, run logs and reports can contain source code, credentials and model output. |
| Provider credentials | leaf/agent host | They authorise paid model calls and must not reach the controller or another tenant. |
| Repository credentials | controller host, `[[controller.repos]].credential_file` and `[mirror.credentials]` | They authorise fetches into the controller's bare mirrors and must not reach an agent or appear in a log, artifact, `lu show` or error. |
| Publish tokens | controller host, `[publish.remotes.<name>].token_file` | They authorise writes to the target repository. Only the controller reads one, for one push, and it must never reach an agent, artifact, `lu show`, audit event or error. |
| Intake signing keys | controller host, `[[intake.sources]].secret_file` | They authenticate intake events; anyone holding one can submit events for that source, so they are mode `0600` secrets. |
| Drafts and external context | controller store and job spec | Draft task text is operator-authored; `context` is external data that must never be promoted to instructions. |

## Threat model

The controller treats everything arriving over the network as hostile: bearer headers, ids,
node names, JSON bodies, raw log chunks, artifact bytes, lengths, offsets and query strings.
Requests are authenticated before their bodies are parsed, bodies and logs are size-capped,
request bodies reject unknown fields, ids and names are validated, and artifact file names are
chosen by the controller rather than supplied by the agent. The controller never connects back to an
agent, so there is no inbound path to an agent from the controller or from another agent.

The main adversaries considered are:

- someone on the network between an operator or agent and the controller;
- someone who has stolen an agent or operator token;
- a compromised or malicious agent host or harness;
- a malicious operator with a valid token;
- a local unprivileged user on the controller or agent host;
- a stranger who can post a comment or other event carrying data that reaches an intake source;
- a hostile repository, task text, diff or log that tries to exploit parsing, path handling or
  terminal output.

The model provider, host kernel and systemd sit outside Lu's control. The controls here cover the
network path, token scopes, agent writes and harness execution.

## Tokens, scopes and hashes

The controller stores only SHA-256 hashes of tokens. A presented token is hashed and compared
with every configured hash using a constant-time comparison. The token itself is never sent to
the controller in a job or stored in SQLite. Because the hash is unsalted, tokens must be
generated randomly with enough entropy; the quick start uses 32 random bytes. Do not reuse a
human password as a token.

There are two scopes:

- **Operator**: may submit, list, show, cancel and read logs/artifacts for any job, list nodes,
  and drain or undrain any node. An operator token is not bound to a node.
- **Agent**: bound to exactly one node name at config time. Its heartbeat must name the same
  node, and agent writes are accepted only for jobs currently assigned to that node and for the
  current epoch. It cannot use operator routes. It can read its own node state and drain or
  undrain only that node through the agent self-service routes; no node name is accepted from the
  caller. There is no separate controller-role token: the controller service has no agent token, so
  an operator token or an anonymous caller is `401` on those routes.

Missing, malformed and unknown tokens are `401 unauthorized` with `WWW-Authenticate: Bearer`.
A valid token used on the other scope's route is also `401`, so a probe cannot distinguish scope
failures. A valid agent token that names a different node, or an agent write for another node's
job, is `403 forbidden`.

Operator credentials never come from `--config`. The operator CLI reads only `LU_URL` with
`LU_TOKEN_FILE` (optional `LU_CA_FILE`) or an operator file with `url` and `token_file` (optional
`ca_file`); when none are present it exits non-zero and names those settings and files. There is
no fallback to `[agent].controller_url` or `[agent].token_file`, so an agent token is never
attached to an operator request. If one were presented, the controller would refuse it with the
same wrong-scope `401` as any other agent call on an operator route.

Token hashes must be unique across agents and operators, so one token can never hold both
scopes. An agent name is also the node identity; two agents cannot share a name or a token hash.

### Throttling

Failed token checks are throttled per source address and scope. Ten wrong-token failures in
sixty seconds block that address for the rest of the window and return `429 rate_limited`
with `Retry-After`; the block is checked before the token, so it also refuses otherwise valid
requests from that address. Requests with no token are counted in a separate bucket with a
limit of 100 in the same window. They never consume the wrong-token budget, so a polling
client that lost its token cannot lock out other clients sharing the address; a full
tokenless bucket throttles only further tokenless requests. IPv6 addresses are counted by
`/64`, and an IPv4-mapped IPv6 address is counted as IPv4. A successful request does not reset
either count.

For token generation, hash configuration, private-file rules and rotation, see
[Tokens and TLS](tokens-and-tls.md#token-files-and-hashes). The controller stores only SHA-256
hashes, so the value hashed into `token_sha256` is the same byte sequence the client later sends
as the bearer token. `provider_gateway_token_file`, when the optional provider health check is configured, uses
those rules: a regular file owned by the agent account, mode `0600` or tighter with no group or
other bits, no symlink or FIFO, opened with `O_NOFOLLOW | O_NONBLOCK`. It holds one token line,
read at most 4 KiB when used and 64 KiB when the config loads.

## TLS and transport

The trust boundary is the listener: plain HTTP is confined to loopback, any non-loopback listener
requires `[tls]`, and the bearer token is the only client credential. Certificate verification
stays enabled; `ca_file` adds roots rather than replacing them. Modes, file rules and rotation are
in [Tokens and TLS](tokens-and-tls.md#tls-modes).

## Epoch fencing and node binding

Each assignment increments the job's epoch and creates a lease. Every agent write (`start`,
`renew`, log append, artifact upload, complete) names the epoch it believes it holds. A write
for an older epoch is `409 stale_epoch` and changes nothing. This prevents a restarted,
duplicated or delayed agent from finishing a job that has since been reassigned.

An agent token is bound to one node, and the controller checks that the job's `node` is that
node before accepting a write. A token from node-a therefore cannot complete, log or upload for
a job on node-b, even if it guesses the job id and epoch.

The same binding applies to the self-service routes: `GET /v1/agent/self`, `POST /v1/agent/drain`
and `POST /v1/agent/undrain` all derive the node from the bearer token alone. Query strings and
body fields are refused, so agent-a cannot read or change agent-b's drain state. Drain ownership
is explicit: `nodes.draining` distinguishes operator and agent drains, `agent/undrain` returns
`409 conflict` for an operator drain and never clears it, and an agent drain never replaces an
operator drain. Only operator routes can clear an operator drain. These calls are audited with the
agent node as actor (`agent_self`, and `drain` with `source: "agent"`).

Lease expiry is a safety mechanism governed by the lease epoch and node binding. If an agent is
slow, paused or partitioned for `lease_secs`, the job becomes `recovering`; after
`recovery_grace_secs` it is requeued or failed. A late completion from the old lease arrives with
a stale epoch and changes nothing. Artifact uploads check the epoch before reading the body, when
recording the artifact, and again immediately before the atomic file rename, so a lease transfer
during the upload returns `409 stale_epoch` without overwriting the newer attempt's file. On
controller restart, `leased` and `running` jobs enter the same recovery path so each job reaches a
terminal state or a retry; a recovery error stops startup before any request is served.

## What the agent treats as untrusted

The controller assigns work and defines the job spec and limits. The agent validates the spec and
treats every field as untrusted input, including a repository URL, ref, task, provider name or
model string accepted by the controller.

The agent treats these inputs as untrusted:

- the repository or ref it checks out, or any file, hook, script or build instruction in it;
- the task text or model output, which may contain hostile terminal escape sequences, paths,
  URLs or instructions;
- the harness process, which may hang, fork, write unbounded output or try to read files it
  should not;
- log or artifact content returned by the controller, which is printed or written for the
  operator.

For a configured repository the controller mirrors the forge itself and pins the job to a
commit, so an agent no longer needs forge access or its credential. A mirror is a bare repository
under `<artifacts_dir>/../mirrors`, mode `0700`; fetching uses `https` (`file://` is allowed only by the test-only `allow_file_urls` escape hatch), no terminal prompts,
no system or global config, no hooks and no inherited askpass or ssh command, with a 5-minute
timeout and a size cap. A repository credential is read at fetch time, passed through a
short-lived private askpass helper, and scrubbed from any stderr before it becomes an error. A restart clears helpers left by a crash.

Job submissions cannot put userinfo in `source.repo`: the validator refuses it with the same fixed error used for mirror URLs. Rows written before that rule still exist, so every job view strips userinfo, query and fragment from the repo before encoding it; the stored value itself is unchanged for the agent's fetch. Audit data fields apply the same stripping to URL-shaped text, and no credential text is accepted into an audit field.

Every staging `git` invocation, including three-way applies, runs with prompts disabled, a
restricted transport allowlist (`file`, `https`, `ssh`), system and global config skipped, hooks
disabled, and inherited config, askpass, proxy and repository-location environment scrubbed. The
harness is launched directly in its own
process group with a cleared environment rebuilt from `PATH`, `HOME`, `LANG`, the selected
profile's explicit `env` and `secret_env` values, stdin closed unless the profile selects task
stdin, and capped output. Timeouts and idle-output detection bound a harness that never finishes.
Those controls apply at the process and filesystem level. A harness needing a stronger boundary
runs through a profile `wrapper` that starts a separate runner; the wrapper is direct argv, never a
shell. See [Harness profiles](config.md#harness-profiles) for the validated fields.

The controller uses the agent's report as the record of what its harness did. An agent reads the
job's task and repository content and produces the diff, log and report, so operators open those
files as untrusted input and place secrets only on trusted nodes.

## Intake and drafts

Intake is off unless `[intake]` is configured. The route `POST /v1/intake/<source>` takes no
operator or agent bearer token: the source HMAC-SHA256 key is its only credential. A missing,
malformed or wrong signature all return the same `401`, so a sender cannot probe which check failed. Signatures are checked before bodies are
capped or parsed, payload text is never written to logs or audit entries, and successful requests
return only a job id and state. See [intake.md](intake.md) for the configuration and rules.

A stranger who can post a comment or event that reaches a configured source can:

- send a correctly signed event and have it evaluated against that source's rules;
- influence values copied by `context_from` into the job's external `context`, bounded by a 32 KiB
  cap with a truncation marker;
- influence `task` only when the rule sets `unsafe_inline = true` and the pointer is listed in that
  rule's `task_vars`, with each value length-capped and stripped of control characters;
- cause an event to be dropped (`204`) by failing `match` or `allow`, or counted as `duplicate`,
  `rate_limited` (`429`) or `invalid` (`400`/`429` for a full draft cap), or create a draft (or,
  with `auto_approve`, a queued job) when the rules permit it.

That sender cannot:

- dispatch a job by default, because `auto_approve` defaults to false and the outcome is a `draft`,
  which is stored and visible but never leased or run; rejection cancels it and TTL expiry ends it
  in `expired`, a terminal state. An operator may deliberately enable `auto_approve` for a trusted
  rule, which is an operator decision, not a sender capability;
- bypass a configured `allow` condition: a failed allowlist drops and counts the event and never
  creates even a draft;
- inject arbitrary payload text into `task` while `unsafe_inline = false` (the default). Payload
  fields go only into `context`, which the agent stages as `.lu/inputs/context.md` under an
  **UNTRUSTED EXTERNAL TEXT** header, with the harness prompt instructing it to treat the file as
  data, not instructions. `unsafe_inline = true` removes that separation and is dangerous;
- read drafts or another job's task text through job-scoped agent paths: `lu_drafts` is an
  operator-only read tool, and `/v1/scope/*` and `ting mcp` cannot see drafts;
- see or use signing keys, repository credentials or operator tokens in logs, artifacts,
  `lu show`, audit events or errors. Mirror fetches use restricted exact refspecs, never `+refs/*`,
  pin the resolved commit on the job, and intake refuses fork heads unless its rule sets
  `allow_forks = true`.

Plain HTTP intake is confined to loopback. With `[intake]` enabled, startup refuses a listener
that has neither TLS nor a loopback bind unless `allow_plain_http = true`; use that flag only
behind a reverse proxy or tunnel that terminates TLS and keeps the direct listener off untrusted
networks. Per-source rate limits, bounded dedupe and `[drafts]` caps make a hostile sender noisy but
not able to exhaust other sources or displace existing drafts. Outcomes are counted in
`lu_intake_total{source,result}` and `lu_drafts{state}`; see
[monitoring.md](monitoring.md#series) and [Submitting work](submitting.md#drafts).

## Publishing

Publishing is an explicit operator action on a finished job. The controller never gives a push token
to an agent or a harness, and no payload text can call the publish route. A job has no publish
target unless an explicit spec remote, a configured redirect or the job's own configured source
remote selects one; every other source remote is denied. The token file is owner-only, read at push
time with an 8 KiB cap, and scrubbed from git errors before an operator sees them. Tests walk the
controller's temporary state after a push and grep every file except the token file so a credential
cannot leak into a record, view, artifact, log or audit event. See [Publishing](publish.md) for the
workflow and [Configuration](config.md#publish) for the settings.

A source-branch push is the highest-risk path because it can move a branch other people use. Lu
requires all of these before attempting one:

- the selected remote opts in with `allow_source_branch_push = true`;
- the job spec sets `publish.target = "source_branch"`;
- the origin allowance `publish.allow_source_branch` is true in the operator's job spec or intake
  rule, never in webhook payload text;
- an operator submitted the job, or explicitly approved the draft with `lu approve`.

The branch name comes from the controller mirror and `source.ref`, not from the task, context or
payload. A pull-request head must map to exactly one mirrored branch in the same repository; a
fork-only or ambiguous head is refused. `main`, `master`, `work` and `release*` are protected by
default, with operator patterns added through `protected_patterns`. Gates run before the push, and
`on_gate_failure = "push_labelled"` has no effect on a source-branch push. The push is one exact,
non-force refspec, so it cannot delete a ref, push a tag, touch another branch or replace a
non-fast-forward commit. A compare-and-swap preflight checks the advertised tip against the job's
pinned base commit; on a miss Lu either falls back to `snapshot/<key>` or records
`no_snapshot_prefix` without touching the source branch.

**This is defence in depth, not a trust boundary on its own.** The CAS preflight is not an atomic
remote lease: a writer that moves the branch between the check and the push is outside Lu's control.
Use a dedicated machine account whose token has contents write only on the target repository, and
enforce branch protection or rulesets at the forge for protected names, required review and direct
pushes. Source-branch publishing is not recommended without those layers. Every attempt writes a
`publish` audit event when auditing is on and increments `lu_publish_total{target,result}`; neither
carries a credential or payload text.

## Keeping the harness unprivileged

The contributed units use a separate `lu` service account and create an unprivileged
`lu-harness` account in `contrib/lu.sysusers`. The units alone do not switch the harness to
that account: the agent launches it as its own user unless a separate runner is configured.

- `lu` runs the controller or agent and owns the token file, mode `0600`;
- `lu-harness` can run the coding-agent process through a separate-account runner configured as a
  profile `wrapper` with `[agent] workspace_group`; only then is it kept away from the token,
  the controller config and the TLS key;
- the shared staging root can be group-writable by `lu-harness`, while each job directory is
  created mode `0700`.

The units apply `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`,
`PrivateDevices`, restricted address families, an empty capability set, a system-call filter
and `LimitNOFILE`. They are `Type=simple`; shutdown drains in-flight connections for a fixed 10
seconds.

**Separating the harness.** With `[agent] workspace_group` set, the working tree is shared with that group and `.git` stays with the agent. An agent that runs as one account can drive a harness that works as another, such as an opencode server in a container, while the harness stays separate from the agent's token and `.git`.

**Without it:** the harness runs as the agent's own user. The harness gets a minimal
environment and never receives the token as an argument or variable, but it can read any file
the agent can, including the token file. Run the agent as a dedicated account that holds no other
secrets, and treat a job's harness as able to act with that account's rights. Use a profile
`wrapper` to start the harness in another account when that boundary is needed.

## Credentials stay on the leaf

Provider credentials (API keys and similar) belong to the harness on the agent/leaf host. The
controller stores the provider name and model in the job spec. A selected profile may name private
files in `secret_env`; each is read at job start (at most 4 KiB, trailing CR/LF trimmed) and passed
under the configured variable name. Those values are harness credentials, not a controller
credential channel. The agent clears the child environment and adds only `PATH`, `HOME`, `LANG`,
the profile's explicit `env` values and the `secret_env` values. Names may not be controller
credential names or start with `LU_`.

A task, repository file, command line, model response or harness log can contain a secret, and
that text is stored in `run.log`, `diff.patch`, `report.json` or SQLite. Logs and artifacts are
mode `0600`, directories `0700`, and the database is created `0600`, but backups and downloads
inherit their sensitivity. Treat job storage and backups as sensitive as the repositories and
credentials of the jobs that ran there. Controller responses send `Cache-Control: no-store` and
`X-Content-Type-Options: nosniff`; log and artifact downloads also send
`Content-Disposition: attachment` so a browser saves them instead of embedding them in a page.

## Artifact scrubbing

The controller can redact secret-like bytes before writing `report.json` and `run.log`. Scrubbing is on by default. Set `[artifacts] scrub = false` to store uploads unchanged. Operator patterns can be added with `[[artifacts.extra_rules]]`; see [config.md](config.md#artifacts).

Built-in rules use generic names. They do not name token issuers.

| Rule | What it matches |
| --- | --- |
| `private_key` | A PEM private-key block, including its body. Certificates are left alone. |
| `bearer_token` | The value after `Authorization: Bearer ` when it is at least 12 characters long. |
| `token_prefix` | Long values shaped like `ghp_`, `github_pat_`, `sk-` or `AKIA` tokens. |
| `jwt` | Three base64url segments with JSON-looking first two segments. |
| `assignment` | `password=`, `token=`, `secret=` and similar assignments with a value of at least 12 characters. Quoted JSON keys are included. |

Every match becomes `[scrubbed:<rule>]`. The controller adds the number of matches to the job's `scrubbed_hits` field. A changed `diff.patch` also sets `patch_scrubbed`.

Patch handling is controlled by `[artifacts] scrub_patch` (default `false`). When `scrub_patch` is `false`, the controller stores `diff.patch` byte for byte as uploaded and sets `patch_scrubbed` to `false`. It inspects the patch without changing it: only added lines (`+`, not `+++`) are scanned with high-confidence rules (`private_key`, `jwt`, `bearer_token`, `token_prefix`; not generic `assignment`). Hits are recorded in `patch_secret_hits` as a count and rule names, never matched text. The job view shows the field, `lu show` renders it, and the controller writes an audit event with no secret text. Publishing and `lu collect` refuse a patch with `patch_secret_hits.count > 0` unless the operator passes the same existing opt-in flag (`[publish] allow_scrubbed = true` or `--allow-scrubbed`).

When `scrub_patch` is `true`, patches are scrubbed one line at a time. Hunk headers are never changed. Diff markers and line terminators are kept, and no replacement adds or removes a line. Private-key bodies are tracked across lines without merging them. A scrubbed patch can still fail to apply because its contents changed. `lu collect` refuses a scrubbed input unless `--allow-scrubbed` is given.

Log uploads report the stored length in the `X-Lu-Log-Length` response header, because scrubbing changes the byte count. Agents use that value for the next chunk offset.

Scrubbing is a heuristic, not a guarantee. It only finds shapes it knows, and an operator rule can be too broad or too narrow. Keep provider credentials on the leaf host. Treat stored artifacts, logs, job specs and backups as sensitive data even when `scrubbed_hits` is zero and `patch_secret_hits.count` is zero.

## Denial of service and resource limits

The controller caps JSON bodies at 1 MiB, a log chunk at 64 KiB plus framing, a diff at 10 MiB,
a report at 1 MiB, and total job log storage at 50 MiB. Before applying an input diff, the agent
also bounds each `GIT binary patch` side to 256 MiB of inflated data and refuses malformed or
size-mismatched binary patch data regardless of the conflict policy. It limits concurrent connections
globally (`max_connections`) and per source address (`max_connections_per_addr`), times out
header reads and requests, closes a connection that is quiet or stuck writing for 120 seconds, and
drains for a fixed 10 seconds on shutdown. The unauthenticated health check shares one 5-second,
single-flight DB probe instead of querying SQLite on every request. Mirror syncs are limited to one
at a time and six per operator per minute; base packs to four at a time and thirty per agent per
minute. Both return `429 rate_limited` on contention. Artifact and log downloads are streamed under
the same per-file caps instead of being read whole into memory. The agent self-service routes
(`/v1/agent/self`, `/v1/agent/drain`, `/v1/agent/undrain`) add no new quota: they use the same
auth throttle, shared JSON/body caps and error envelope as every other agent route. `lu-supervise`
enforces its own run, idle and output limits on the harness.

Intake adds per-source body caps, rolling-minute rate limits, a bounded dedupe window and `[drafts]`
caps; every drop is counted and payload text is not logged, so an unsigned or abusive sender cannot
turn the route into a job-submission or log-growth channel. See
[intake.md](intake.md#deduplication-and-rate-limiting).

These are per-request, per-connection and per-principal limits. Disk use follows the number of jobs an operator
submits and the artifacts an agent uploads within the per-file caps. Filesystem quotas, a
dedicated volume and monitoring bound that growth. The login throttle works per source address
alongside network access control.

## What Lu does not protect against

Lu protects the controller boundary: bearer tokens with two scopes, SHA-256 hashes, TLS on
non-loopback listeners, epoch fencing, private file modes and capped request bodies. It does not
provide the boundaries below.

| Limit | What the code does | What follows | Do this |
| --- | --- | --- | --- |
| Harness runs as the agent user | `ting` launches the harness as its own OS user; there is no `setuid`, namespace, container or VM switch in the launcher. | A harness can open every file that account can, including the agent token and any controller TLS key readable by it. | Run the agent as a dedicated account; use a profile `wrapper` with `workspace_group` for tree access. |
| Token file is readable by that user | A token file must be a regular file owned by the reader, mode `0600` or tighter, with no symlink or FIFO. | Mode `0600` protects against other accounts, not against the agent user or a harness running as that user. | Treat the agent account as holding the token; rotate on suspicion, see [Tokens and TLS](tokens-and-tls.md#rotation). |
| Artifacts and backups carry source and secrets | `diff.patch`, `run.log` and `report.json` report what the harness printed; `lu backup` copies SQLite as ordinary files. | Downloads and backups are as sensitive as every repository and credential the jobs used. | Store them with the same care as the live artifact directory; see [Operations](operations.md#backup-and-restore). |
| Provider keys live on the node | The controller stores provider and model names, not keys. Profile `secret_env` values are harness-side values, not a controller credential channel. | A node, its harness or its logs can expose keys; the controller cannot audit or revoke them. | Keep provider credentials on dedicated nodes and out of task text. |
| No per-job isolation | One agent runs one job at a time in a process group with run, idle and output limits; `workspace_group` is group sharing, not a separate user or filesystem. | Jobs on a node share the agent account, environment and files. | Use a profile `wrapper` that starts a separate runner when jobs must not share a trust domain; see [Configuration](config.md#harness-profiles). |
| Cordons are dispatch hints | A provider cordon blocks one `(node, provider)` pair; a job with no provider is not blocked. | A cordon is not access control or proof that a provider is safe. | Use labels and taints for placement and read `lu nodes --json`; see [Dispatch](dispatch.md#cordons). |
| No general live reload | Config, token hashes, TLS material and the agent token are read at start; profile `secret_env` files are read at each job start; `SIGHUP` is not handled. Only `POST /v1/schedule/reload` swaps in a schedule. | Other file changes, including a profile change or a mid-job agent change, take effect after a restart. | Restart the affected process; see [Time-aware scheduling](scheduling.md#inspecting-scheduling). |
| Runtime group caps are memory-only | `PUT /v1/groups/{name}/limit` changes the active controller-process cap; `[[controller.groups]]` is durable. | A runtime cap returns to its configured value, or disappears, on restart. | Put a required cap in config as well; see [Dispatch](dispatch.md#priority-and-groups). |

Fail-open cases leave a job waiting rather than failing: an unmatched label, an untolerated taint or
no healthy pair for `--provider any` appears as a `waiting` reason in `lu show`. A drain or a cordon
on a named provider can leave a job `queued` without a reason. A failing `[[agent.detect]]` probe
removes its label without stopping the agent. Cordons and drains are not access control.

## Audit record

With `[audit] path` set, the controller appends a small JSON Lines record of lifecycle events to
that file. Without the table no file is opened and no audit work is performed; see
[Configuration](config.md#audit) and [Monitoring](monitoring.md#series).

```json
{"seq":4,"time":"2026-11-21T12:34:56.789Z","event":"started","job":"01ARZ3NDEKTSV4RRFFQ69G5FAV","node":"node-a","actor":"node-a","data":{"attempt":1,"epoch":9},"prev":"9a…","hash":"b3…"}
```

- `seq` increases by exactly one; `time` is controller-generated UTC with millisecond precision.
- `event` is one of `submitted`, `leased`, `started`, `finished`, `failed`, `retried`, `cancelled`,
  `node_excluded`, `cordon_set`, `cordon_cleared`, `drain`, `agent_self`, `collect_applied`,
  `scope_action`, `intake`, `draft_approved`, `draft_rejected`, `draft_expired`, `publish`,
  `job_edited`, `group_limit`, `workspace_clean`, `repo_sync`, `mutation_dry_run`,
  `mutation_replay` and `mutation_precondition_failed`. Operator mutations record the operator and
  role; the `mutation_*` events also carry `data.principal`, `data.role`, `data.operation` and
  `data.outcome`.
- `job`, `node` and `actor` identify the change; `actor` is the operator token name or the node
  name.
- `data` is an allow-list of scalar fields such as `attempt`, `epoch`, `outcome`, `class`, `reason`,
  `excluded_node`, `provider`, `model`, `source`, `state`, `target`, `result`, `remote` and counts.
  It never contains task text, prompts, artifact content or tokens. `node_excluded` records a newly
  excluded node in `data.excluded_node`; a completion adds a fixed node-failure `reason`, while a
  lease-recovery timeout uses actor `maintenance` and no reason.
- `prev` is the hex SHA-256 of the previous line's bytes (64 zeroes for the first line); `hash` is
  the hex SHA-256 of this line's canonical bytes with `hash` omitted.

`collect_applied` records the request at submission, not proof of a later apply. `cordon_set` covers
provider failures and `cordon_cleared` an explicit uncordon; store expiry has no separate line.
`publish` records each outcome with a configured remote name, never a URL or credential.
Agent self-drain records `drain` with `source: "agent"`. The writer appends in mode `0600` where
supported, calls `sync_data` per line, and resumes the verified chain on restart.

`lu audit verify <path>` checks shape, sequence, `prev` links and hashes, reporting the first break
or `ok: N record(s), head <hash>`. With a trusted head recorded outside the file, this proves that
no later edit, insertion or deletion inside the file goes unnoticed: `verify --head <hash>` also
detects a truncated file. `lu audit tail <path> [-l <lines>]` prints the last records (default 20)
and `lu audit head <path>` prints the head hash; `verify --json` prints machine-readable output.

The chain is unkeyed, so someone who can rewrite the whole file can recompute it; it does not prove
an event happened, is not a trusted timestamp, and is not a transaction with job state. Anchor the
head in a place an attacker cannot rewrite and ship lines to an append-only log host. Write errors
and a full bounded channel increment `lu_audit_write_errors_total` (also `audit_write_errors` in
`lu status` and `lu show`) and log an error; no job fails because of one. A startup open failure
stops `lu serve`.

## Reporting

Report vulnerabilities as described in [SECURITY.md](../SECURITY.md). A useful report includes
synthetic tokens, test keys and redacted customer data. Any credential that may have leaked is
rotated before the report is filed.
