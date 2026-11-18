# Security

This page describes what Lu protects, what it trusts, and where the remaining risks are. It
matches the code in this tree. The controller and operator CLI are implemented; the agent
runtime is not, so the agent and harness parts describe the intended deployment and the HTTP
contract, not a runnable 0.1.0 agent. See [operations.md](operations.md) for the same status.

## Assets

| Asset | Where it lives | Why it matters |
| --- | --- | --- |
| Agent and operator tokens | token files on the host that uses them | A token is a bearer credential; possession is authorisation. |
| Token hashes | controller config (`token_sha256`) | They are not secrets, but changing one revokes a token. Protect the config so an attacker cannot add a hash. |
| TLS private key | controller host, `[tls] key` | Anyone with it can impersonate the controller. |
| SQLite database | controller host, `[controller] db` | Job specs and tasks, state, leases, epochs, idempotency keys, node state. |
| Job logs and artifacts | controller host, `artifacts_dir/<job id>/` | Diffs, run logs and reports can contain source code, credentials and model output. |
| Provider credentials | leaf/agent host in 0.1.0 | They authorise paid model calls and must not reach the controller or another tenant. |

## Threat model

The controller treats everything arriving over the network as hostile: bearer headers, ids,
node names, JSON bodies, raw log chunks, artifact bytes, lengths, offsets and query strings.
Requests are authenticated before their bodies are parsed, bodies and logs are size-capped,
JSON rejects unknown fields, ids and names are validated, and artifact file names are chosen by
the controller rather than supplied by the agent. The controller never connects back to an
agent, so there is no inbound path to an agent from the controller or from another agent.

The main adversaries considered are:

- someone on the network between an operator or agent and the controller;
- someone who has stolen an agent or operator token;
- a compromised or malicious agent host or harness;
- a malicious operator with a valid token;
- a local unprivileged user on the controller or agent host;
- a hostile repository, task text, diff or log that tries to exploit parsing, path handling or
  terminal output.

Out of scope for 0.1.0: a compromised controller host, a malicious kernel or systemd, side
channels in the model provider, and cryptographic attestation of what a harness actually did.

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
A valid token used on the other scope's route does not authenticate. In this tree that is also
reported as `401`; `docs/api.md` specifies `403` for a wrong scope, which is a code/contract
mismatch listed in the task report. A valid agent token that names a different node, or an agent
write for another node's job, is `403 forbidden`.

Token hashes must be unique across agents and operators, so one token can never hold both
scopes. An agent name is also the node identity; two agents cannot share a name or a token hash.

Failed token checks are throttled per source address: ten failures in sixty seconds block that
address for the rest of the window and return `429 rate_limited` with `Retry-After`. IPv6
addresses are counted by `/64`, and an IPv4-mapped IPv6 address is counted as IPv4. A
successful request does not reset the count.

Recommended token handling:

```sh
umask 077
printf '%s' "$(head -c 32 /dev/urandom | base64)" > /etc/lu/agent.token
chown lu:lu /etc/lu/agent.token
chmod 600 /etc/lu/agent.token
sha256sum /etc/lu/agent.token
```

The value printed by `sha256sum` goes in `token_sha256`. The token file is read once with
`O_NOFOLLOW` and `O_NONBLOCK`, must be a regular file owned by the effective user, must have no
group or other permission bits, and is capped at 64 KiB. The same rules apply to
`reiki_token_file` when that optional health check is configured.

## TLS and transport

Plain HTTP is accepted only when `[controller].listen` is a loopback address. Any non-loopback
listener must have a `[tls]` table; `lu serve` enforces that. The listener uses TLS 1.2 or 1.3
with safe defaults, HTTP/1.1 only, and no client certificates: the bearer token is the only
client authentication.

The TLS key is treated as a secret file by `lu serve`: regular file, owned by the effective user,
no group/other bits, no symlink, no FIFO. The certificate is public. Both are loaded at startup;
there is no reload, so restart after replacing them. `lu check`, `lu backup` and `lu restore` do
not read these files.

Operators and agents verify the controller certificate against the system trust roots. The
optional `[agent].ca_file` adds up to 1 MiB of PEM certificates to those roots; it does not
replace them and it is not a pinning mechanism. Use a certificate that matches the host name in
`controller_url`. A self-signed certificate belongs in `ca_file`, not in a disabled verification
mode; the client has no such mode.

## Epoch fencing and node binding

Each assignment increments the job's epoch and creates a lease. Every agent write (`start`,
`renew`, log append, artifact upload, complete) names the epoch it believes it holds. A write
for an older epoch is `409 stale_epoch` and changes nothing. This prevents a restarted,
duplicated or delayed agent from finishing a job that has since been reassigned.

An agent token is bound to one node, and the controller checks that the job's `node` is that
node before accepting a write. A token from node-a therefore cannot complete, log or upload for
a job on node-b, even if it guesses the job id and epoch.

Lease expiry is a safety mechanism, not an authentication failure. If an agent is slow, paused
or partitioned for `lease_secs`, the job becomes `recovering`; after
`recovery_grace_secs` it is requeued or failed. A late completion from the old lease is
discarded as stale. On controller restart, `leased` and `running` jobs enter the same recovery
path so a job cannot stay wedged forever.

## What the agent does and does not trust

The controller is trusted to assign work and to define the job spec and limits, but the agent
still validates the spec and treats every field as untrusted input. It should not assume that a
repository URL, ref, task, provider name or model string is safe merely because the controller
accepted it.

The agent does **not** trust:

- the repository or ref it checks out, or any file, hook, script or build instruction in it;
- the task text or model output, which may contain hostile terminal escape sequences, paths,
  URLs or instructions;
- the harness process, which may hang, fork, write unbounded output or try to read files it
  should not;
- log or artifact content returned by the controller, which is printed or written for the
  operator.

Staging invokes `git` directly, not through a shell, with terminal prompts disabled, no system
config and a restricted transport allowlist (`file`, `https`, `ssh`). The harness is launched
directly in its own process group with an empty inherited environment except `PATH`, `HOME` and
`LANG`, no stdin, and capped output. Timeouts and idle-output detection stop a harness that
never finishes. Those controls limit accidental damage; they are not a VM or container boundary.
A harness that needs stronger isolation should run inside one.

The controller trusts an agent to report what its harness did, but cannot prove it. A malicious
agent can fabricate a diff, log or report for a job assigned to it, and can read the job's task
and repository content. Do not send secrets to a node you do not trust, and treat artifacts as
untrusted input when you open them.

## The harness runs unprivileged

The contributed units use a separate `lu` service account and an unprivileged `lu-harness`
account created by `contrib/lu.sysusers`. The intended split is:

- `lu` runs the controller or agent and owns the token file, mode `0600`;
- `lu-harness` runs the coding-agent process and cannot read the token, the controller config
  or the TLS key;
- the shared staging root is group-writable by `lu-harness`, while each job directory is
  created mode `0700`.

The units apply `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`,
`PrivateDevices`, restricted address families, an empty capability set, a system-call filter
and `LimitNOFILE`. They are `Type=simple`; there is no `sd_notify` support.

**Separating the harness.** With `[agent] workspace_group` set, the working tree is shared with that group and never `.git`. An agent that runs as one account can then drive a harness that works as another, such as an opencode server in a container, and the harness can neither read the agent's token nor change `.git`.

**Without it:** the agent runs the harness as its own user. The harness gets a minimal
environment and never receives the token as an argument or variable, but it can read any file
the agent can, including the token file. Run the agent as a dedicated account that holds no other
secrets, and treat a job's harness as able to act with that account's rights. Separating the
harness into `lu-harness` is planned.

## Credentials stay on the leaf in 0.1.0

Provider credentials (API keys and similar) belong to the harness on the agent/leaf host. The
controller stores only the provider name and model in the job spec, and has no provider
credential store. In the intended agent design the runtime passes the minimum needed environment
to the harness and never puts those values in a heartbeat, log, artifact or completion.

Moving provider credentials to the controller later would be a security-relevant design change:
it would concentrate valuable secrets in one host, require encryption at rest and a way to
deliver short-lived credentials to the correct node and job, and require audit logging that does
not copy the secret into job output. Do not assume that model exists in 0.1.0.

There is no redaction pass in 0.1.0. A task, repository file, command line, model response or
harness log can contain a secret, and that text is stored in `run.log`, `diff.patch`,
`report.json` or SQLite. Logs and artifacts are mode `0600`, directories `0700`, and the
database is created `0600`, but backups and downloads inherit their sensitivity. Treat job
storage and backups as sensitive as the repositories and credentials of the jobs that ran there.

## Denial of service and resource limits

The controller caps JSON bodies at 1 MiB, a log chunk at 64 KiB plus framing, a diff at 10 MiB,
a report at 1 MiB, and total job log storage at 50 MiB. It limits concurrent connections
globally (`max_connections`) and per source address (`max_connections_per_addr`), times out
header reads and requests, and drains for a fixed 10 seconds on shutdown. `lu-supervise` enforces
its own run, idle and output limits on the harness.

These are per-request and per-connection limits, not quotas. A valid operator can create jobs
until the disk fills, and a valid agent can upload up to the artifact caps for each job. Use
filesystem quotas, a dedicated volume and monitoring if Lu stores work from untrusted users.
The login throttle is per source address and is not a substitute for network access control.

## Reporting

Report vulnerabilities as described in [SECURITY.md](../SECURITY.md). Do not include real
tokens, private keys or customer data in a report; rotate anything that may have leaked.
