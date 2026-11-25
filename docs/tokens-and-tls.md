# Tokens and TLS

Lu uses bearer tokens for both scopes and TLS whenever the controller is reachable off loopback.
The controller config stores **hashes only**; clients read plaintext token files.

## Token files and hashes

Create one token for each agent and one for each operator. Keep the plaintext files: they are the
credentials, and the controller cannot recreate them from the hashes.

```sh
umask 077
install -d -m 0750 /etc/lu
install -d -m 0750 /etc/ting

printf '%s' "$(head -c 32 /dev/urandom | base64)" > /etc/ting/ting.token
printf '%s' "$(head -c 32 /dev/urandom | base64)" > /etc/lu/operator.token

# Each file must be owned by the account that reads it: the agent token by ting,
# the operator token by whoever runs the operator CLI (root in the systemd example).
chown ting:ting /etc/ting/ting.token
chown root:root /etc/lu/operator.token
chmod 600 /etc/ting/ting.token /etc/lu/operator.token

sha256sum /etc/ting/ting.token /etc/lu/operator.token
```

Use `printf '%s'`, as above, so the file has no trailing newline. Clients trim trailing CR/LF from
a token file, but `sha256sum` hashes the exact bytes on disk; a newline would make the hash and the
bearer token disagree. Each hash must be 64 lowercase hex characters.

```toml
[[controller.agents]]
name = "node-a"
token_sha256 = "<sha256 of ting.token>"

[[controller.operators]]
name = "ops"
token_sha256 = "<sha256 of operator.token>"
```

Operator entries accept four optional settings, all safe to omit:

```toml
[[controller.operators]]
name = "dashboard"
token_sha256 = "<sha256 of dashboard.token>"
role = "viewer"                    # viewer, operator (default) or admin (reserved)
expires_at = "2030-01-31T12:00:00Z" # UTC, strict form; the token is refused from this instant
restrict_fields = true             # refuse job fields that widen what a job may do
schedule = true                    # allow POST /v1/schedule/reload
```

- `viewer` is read-only. It may read `/v1/status`, `/v1/stats`, `/v1/jobs`, `/v1/jobs/{id}`,
  `/v1/nodes`, `/v1/nodes/{name}/stats`, `/v1/repos`, `/v1/health` and `/metrics`. Task text
  and context are removed from every JSON response it receives. Logs, artifacts, drafts, scope
  routes and every mutation return `403`.
- `expires_at` makes the token fail exactly like an unknown one (`401`) once the time passes. Lu
  does not reload tokens at runtime, so rotate by editing the file and restarting the controller.
- `restrict_fields` makes job creation refuse `allow_spawn`, `spawn_inherit`, `spawn_repos`,
  `stack_union_paths`, `stack_base_paths`, `publish.allow_source_branch` and
  `publish.target = "source_branch"` with `403`.
- `schedule` allows `POST /v1/schedule/reload`. Read-only schedule routes need no extra permission,
  but a `viewer` token cannot reach them. See [scheduling.md](scheduling.md#inspecting-scheduling).
- A viewer or `restrict_fields` token requires an `[audit]` path: the controller refuses to start
  without one, so changes made through a UI leave a record.

Hashes must be unique across all agents and operators, so one token cannot hold both scopes. Agent
names are also node identities. Config validation rejects malformed hashes, duplicate names and
duplicate hashes.

A token file is opened once with `O_NOFOLLOW | O_NONBLOCK` and must be a regular file owned by the
effective user, with no group or other permission bits (`0600` or tighter). Symlinks, FIFOs and
directories are refused. Agent token files are capped at 64 KiB at configuration load. Repository
credential files and provider gateway token files are capped at 64 KiB when the configuration loads, then at
8 KiB and 4 KiB respectively when read for use. Operator token files are capped at 8 KiB.
The presented bearer value is capped at 8 KiB on every request, so an agent token longer than that
is refused even though its file passes the 64 KiB load-time check. The bearer value must be visible
ASCII with no spaces or control characters.

## Authentication in practice

Clients send `Authorization: Bearer <token>`. The controller hashes the presented token and
compares it against every configured hash in constant time. Operator tokens open operator routes;
agent tokens open only that agent's node routes. A token from the other scope gets `401`, so a
probe cannot tell scope failures apart. A valid agent token used for another node's job, or a
write naming another node's job, gets `403 forbidden`.

Ten failed token checks from one address within 60 seconds block that address for the rest of the
window and return `429 rate_limited` with a retry delay. IPv6 addresses are counted by `/64`, and
IPv4-mapped IPv6 addresses count as IPv4. Successes do not reset the count. Separately, successful operator and agent requests are charged against per-principal budgets for mirror syncs and base packs; those also return `429 rate_limited` with `Retry-After` when exhausted.

## TLS modes

| Setup | Controller | Clients |
| --- | --- | --- |
| Loopback demo | `[controller].listen = "127.0.0.1:7600"` and no `[tls]` table; plain HTTP is allowed. | `http://127.0.0.1:7600`; a `ca_file`, if set, is still read but has no effect on plain HTTP. |
| Remote controller | `[controller].listen` on a non-loopback address plus a `[tls]` table with both `cert` and `key`. | `https://<host>:7600`, with hostname verification against system roots. |
| Private CA | Same as remote. | Add PEM roots with `[agent].ca_file`, `LU_CA_FILE`, or `ca_file` in an operator config. |

`cert` and `key` are both required when `[tls]` is present. The certificate chain is public; the
key is checked with the same private-file rules as a token and must be owned by the account
running `lu serve`. The listener uses TLS 1.2 or 1.3, HTTP/1.1 only, with no client certificates:
the token is the only client authentication. These files are loaded at startup, so restart the
controller after replacing them; the schedule-only reload route does not re-read TLS material.

A `ca_file` adds up to 1 MiB of PEM certificates to the system trust roots. It does not replace
them and is not a pinning mechanism. Use a certificate whose subject matches the host name in
`controller_url` or `LU_URL`. There is no option to disable verification.

`lu check` validates config values, reports `tls on` or `tls off`, names the checks it ran, and opens no token or key files or port. `lu backup` and `lu restore` read only `[controller] db`, and `lu check` uses that key for its database checks without opening secrets, so all three commands work as root against a service account's secrets. `lu serve` loads the key and certificate; `ting` loads the agent token; operator commands load the operator token from `LU_TOKEN_FILE` or an operator file. An agent token on an operator route gets `401`, so operator commands never fall back to `[agent].token_file`.

## Rotation

For a token:

1. Generate a new value and hash it as above.
2. Add or replace its `token_sha256` in the controller config. Hashes must remain unique. To
   overlap old and new clients, keep both hashes for a while; remove the old hash to revoke it.
3. Restart the controller; token config is not reloaded in place. The schedule-only reload route
   does not re-read token hashes.
4. Replace the token file on the agent or operator host. Restart `ting` so it re-reads its
   file; operator commands read the file at each invocation.
5. Verify with `lu status` (operator scope) and `lu nodes` (agent heartbeats).

For TLS:

1. Replace the certificate and key, keeping the key private and owned by the controller user.
2. Restart the controller; TLS material is read once at startup.
3. If the issuer changes, update the clients' trust roots or `ca_file`, then restart agents so
   their clients rebuild with the new roots.
4. Verify with `lu status` from an operator host and by checking that agent heartbeats resume.
