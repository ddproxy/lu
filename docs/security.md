# Security

This page describes what Lu protects, what it trusts, and the controls and trust boundaries in
place. It matches the code in this tree. The controller, agent (`lu agent`) and operator CLI are
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
| Repository credentials | controller host, `[[controller.repos]].credential_file` | They authorise forge fetches into the controller's bare mirrors and must not reach an agent or appear in a log or error. |

## Threat model

The controller treats everything arriving over the network as hostile: bearer headers, ids,
node names, JSON bodies, raw log chunks, artifact bytes, lengths, offsets and query strings.
Requests are authenticated before their bodies are parsed, bodies and logs are size-capped,
unknown `JobSpec` fields are ignored for forward compatibility while other bodies reject unknown
fields, ids and names are validated, and artifact file names are chosen by the controller rather
than supplied by the agent. The controller never connects back to an
agent, so there is no inbound path to an agent from the controller or from another agent.

The main adversaries considered are:

- someone on the network between an operator or agent and the controller;
- someone who has stolen an agent or operator token;
- a compromised or malicious agent host or harness;
- a malicious operator with a valid token;
- a local unprivileged user on the controller or agent host;
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
  current epoch. It cannot use operator routes.

Missing, malformed and unknown tokens are `401 unauthorized` with `WWW-Authenticate: Bearer`.
A valid token used on the other scope's route is also `401`, so a probe cannot distinguish scope
failures. A valid agent token that names a different node, or an agent write for another node's
job, is `403 forbidden`.

Token hashes must be unique across agents and operators, so one token can never hold both
scopes. An agent name is also the node identity; two agents cannot share a name or a token hash.

Failed token checks are throttled per source address: ten failures in sixty seconds block that
address for the rest of the window and return `429 rate_limited` with `Retry-After`. IPv6
addresses are counted by `/64`, and an IPv4-mapped IPv6 address is counted as IPv4. A
successful request does not reset the count.

For token generation, hash configuration, private-file rules and rotation, see
[Tokens and TLS](tokens-and-tls.md#token-files-and-hashes). The controller stores only SHA-256
hashes, so the value hashed into `token_sha256` is the same byte sequence the client later sends
as the bearer token; the same private-file rules cover `reiki_token_file` when that optional
health check is configured.

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

Lease expiry is a safety mechanism governed by the lease epoch and node binding. If an agent is
slow, paused or partitioned for `lease_secs`, the job becomes `recovering`; after
`recovery_grace_secs` it is requeued or failed. A late completion from the old lease arrives with
a stale epoch and changes nothing. On controller restart, `leased` and `running` jobs enter the
same recovery path so each job reaches a terminal state or a retry.

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
under `<artifacts_dir>/../mirrors`, mode `0700`; fetching uses `https` only, no terminal prompts,
no system or global config, no hooks and no inherited askpass or ssh command, with a 5-minute
timeout and a size cap. A repository credential is read at fetch time, passed through a
short-lived private askpass helper, and scrubbed from any stderr before it becomes an error.

Staging invokes `git` directly with prompts disabled, a restricted transport allowlist
(`file`, `https`, `ssh`), and system config skipped. The harness is launched directly in its own
process group with an inherited environment of `PATH`, `HOME` and `LANG`, stdin closed, and
capped output. Timeouts and idle-output detection bound a harness that never finishes. Those
controls apply at the process and filesystem level. A harness needing a stronger boundary runs
inside a container or VM through `harness_wrapper`.

The controller uses the agent's report as the record of what its harness did. An agent reads the
job's task and repository content and produces the diff, log and report, so operators open those
files as untrusted input and place secrets only on trusted nodes.

## The harness runs unprivileged

The contributed units use a separate `lu` service account and an unprivileged `lu-harness`
account created by `contrib/lu.sysusers`:

- `lu` runs the controller or agent and owns the token file, mode `0600`;
- `lu-harness` runs the coding-agent process and cannot read the token, the controller config
  or the TLS key;
- the shared staging root is group-writable by `lu-harness`, while each job directory is
  created mode `0700`.

The units apply `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`,
`PrivateDevices`, restricted address families, an empty capability set, a system-call filter
and `LimitNOFILE`. They are `Type=simple`; shutdown drains in-flight connections for a fixed 10
seconds.

**Separating the harness.** With `[agent] workspace_group` set, the working tree is shared with that group and `.git` stays with the agent. An agent that runs as one account can drive a harness that works as another, such as an opencode server in a container, while the harness stays separate from the agent's token and `.git`.

**Without it:** the harness runs as the agent's own user. The harness gets a minimal
environment and never receives the token as an argument or variable, but it can read any file
the agent can, including the token file. Run the agent as a dedicated account that holds no other
secrets, and treat a job's harness as able to act with that account's rights. Use
`harness_wrapper` to start the harness in another account when that boundary is needed.

## Credentials stay on the leaf

Provider credentials (API keys and similar) belong to the harness on the agent/leaf host. The
controller stores the provider name and model in the job spec. In the agent, the provider
credential is set for the harness process through `OPENCODE_SERVER_PASSWORD`. The agent clears
the child environment and adds only `PATH`, `HOME`, `LANG`, the explicit `CommandSpec::env`
entries, and that credential.

A task, repository file, command line, model response or harness log can contain a secret, and
that text is stored in `run.log`, `diff.patch`, `report.json` or SQLite. Logs and artifacts are
mode `0600`, directories `0700`, and the database is created `0600`, but backups and downloads
inherit their sensitivity. Treat job storage and backups as sensitive as the repositories and
credentials of the jobs that ran there.

## Denial of service and resource limits

The controller caps JSON bodies at 1 MiB, a log chunk at 64 KiB plus framing, a diff at 10 MiB,
a report at 1 MiB, and total job log storage at 50 MiB. It limits concurrent connections
globally (`max_connections`) and per source address (`max_connections_per_addr`), times out
header reads and requests, and drains for a fixed 10 seconds on shutdown. `lu-supervise` enforces
its own run, idle and output limits on the harness.

These are per-request and per-connection limits. Disk use follows the number of jobs an operator
submits and the artifacts an agent uploads within the per-file caps. Filesystem quotas, a
dedicated volume and monitoring bound that growth. The login throttle works per source address
alongside network access control.

## Reporting

Report vulnerabilities as described in [SECURITY.md](../SECURITY.md). A useful report includes
synthetic tokens, test keys and redacted customer data. Any credential that may have leaked is
rotated before the report is filed.
