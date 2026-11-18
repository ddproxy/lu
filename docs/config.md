# Configuration

`lu` reads one TOML file. `--config <path>` selects it; the default is `lu.toml` in the current directory. `lu check` parses the file and prints a summary plus database health, but does not check values, read secret files or load the TLS certificate. `lu serve` validates the whole file and its secret files; `lu backup` and `lu restore` read only `[controller] db`.

The file is UTF-8 and limited to 64 KiB. Unknown keys and tables are rejected rather than ignored. Every validation error names the table at fault, for example:

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
| `max_retries` | no | `1` | Integer `0` to `3`. Node and provider failures retry while `attempt <= max_retries`; `0` means no retry. Execution failures never retry. |
| `provider_cooldown_secs` | no | `1800` | Integer `60` to `86400`. How long a `(node, provider)` pair is cordoned after a retryable provider failure without `retry_after_secs`. |
| `recovery_grace_secs` | no | `30` | Integer `0` to `600`. How long a job stays `recovering` before it is requeued or failed. |
| `max_connections` | no | `256` | Integer greater than `0`. Global concurrent connection cap. |
| `max_connections_per_addr` | no | `64` | Integer `1` to `max_connections`. Concurrent connection cap per remote IP. |
| `agents` | no | `[]` | Array of `[[controller.agents]]` tables. With no agent entries, no agent token can authenticate. |
| `operators` | no | `[]` | Array of `[[controller.operators]]` tables. With no operator entries, no operator token can authenticate. |

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

## `[agent]`

The `[agent]` table describes an agent host. The operator CLI also reads `controller_url` and `token_file` from this table when `LU_URL` is not set.

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `name` | yes | — | 1 to 64 characters from `A-Z a-z 0-9 . _ -`. It must match the controller's agent entry. |
| `controller_url` | yes | — | `http` or `https` URL with a host. No credentials, query or fragment. `http` is allowed only for `localhost` or a loopback IP. A path is allowed but must not contain `.` or `..` segments. |
| `token_file` | yes | — | Non-empty path. On `load`, it must be a regular file, owned by the effective user, with no group or other permission bits, no symlink and no FIFO; at most 64 KiB. The CLI trims trailing CR/LF from the token text. |
| `ca_file` | no | — | Non-empty path when set. The client reads up to 1 MiB of PEM certificates and adds them to the system trust roots; it does not replace the system roots. Not checked by `lu check` beyond the non-empty test. |
| `work_dir` | yes | — | Absolute path; must not contain `..`. Staging also requires it to exist, be a real directory (not a symlink), and not be `/` or a bare top-level directory. |
| `harness` | yes | — | Non-empty. The configured harness name; jobs use the harness named in their spec, and 0.1.0 runs `opencode` only. |
| `opencode_server` | no | — | Non-empty when set. Opaque endpoint passed to the harness launcher. |
| `harness_wrapper` | no | `[]` | A command that wraps the harness, for example `["podman", "exec", "-i", "--workdir", "{cwd}", "--env", "OPENCODE_SERVER_PASSWORD", "opencode-agent"]`, so it runs inside a container. `{cwd}` becomes the job's working tree. The wrapper runs directly, never through a shell. |
| `opencode_password_file` | no | — | A private file (a regular file owned by the agent's account, no group or other access) whose single line is the opencode server's password. The agent reads it at start and passes it to the harness as `OPENCODE_SERVER_PASSWORD`, the only variable beyond `PATH`, `HOME` and `LANG` the harness gets. |
| `workspace_group` | no | — | A group name or number, 1 to 32 letters, digits, `_`, `-` or `.`. When set, each job's working tree is given to this group with group read and write and the setgid bit; `.git` is left as git made it. For a harness that runs as another account, such as an opencode server in a container, while the agent runs as the account that can use git. Resolved from `/etc/group` at start; an unknown group stops the agent. |
| `heartbeat_secs` | no | `15` | Integer `2` to `300`. When a `[controller]` table is also present, `heartbeat_secs * 2` must be strictly less than `lease_secs`. |
| `reiki_health_url` | no | — | Loopback `http` or `https` URL with no credentials, query or fragment. Requires `reiki_token_file`. The agent reads the local Reiki's peer health (`GET /v1/health`) every 10 seconds and sends each upstream as a provider (`ok`, `cooling` with seconds left, `disabled` or `unknown`) with every heartbeat, plus the document itself. A job that names a `provider` goes only to nodes that report it. After three failed reads the node reports no providers. |
| `reiki_token_file` | no | — | Non-empty when set; required when `reiki_health_url` is set. Checked like `token_file` on `load`. |

## `[tls]`

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `cert` | yes | — | Non-empty certificate path. `lu check` and `lu serve` load it as PEM. |
| `key` | yes | — | Non-empty key path. Loaded as PEM. On `load`, the file must be a regular file owned by the effective user, with no group or other permission bits, no symlink and no FIFO. |

A `[tls]` table makes the controller serve HTTPS. It is required when `listen` is not a loopback address. If the table is present with only `cert` or only `key`, parsing fails.

## `[log]`

| Key | Required | Default | Validation |
| --- | --- | --- | --- |
| `level` | no | `info` | One of `error`, `warn`, `info`, `debug`, `trace`. `RUST_LOG` overrides it at runtime when set. |
| `format` | no | `text` | `text` or `json`. |
| `stderr` | no | `true` | Boolean. Logging also goes to stderr when `file` is unset, even if this is `false`. |
| `file` | no | — | Non-empty path when set. Opened append-only with mode `0640`; a symlink at the path is refused. |

## Cross-table rules

- At least one of `[controller]` and `[agent]` must exist.
- Every `token_sha256` must be unique across all agents and operators.
- Agent and operator names are unique within their own arrays.
- When both tables are present, `heartbeat_secs * 2 < lease_secs`.

## Paths and secret files

- `db`, `artifacts_dir` and `work_dir` must be absolute and must not contain a `..` component. `[tls]` paths, `token_file`, `ca_file`, `reiki_token_file` and `[log] file` are not required to be absolute.
- Secret files are opened once with `O_NOFOLLOW` and `O_NONBLOCK`, then checked on the open handle. They must be regular files, owned by the effective user, with no group or other permission bits (mode `0600` or tighter). A file with mode `0640` is refused for a secret.
- The config file itself has no secret-file mode check; it normally contains only hashes and paths. Protect it anyway because it names the token and TLS key locations.

## Examples

The repository's root `lu.toml` is a loopback controller example with demo token hashes. `contrib/lu-controller.toml` and `contrib/lu-agent.toml` are deployment samples for `/etc/lu/`.

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
```

Minimal agent:

```toml
[agent]
name = "node-a"
controller_url = "https://lu.example.net:7600"
token_file = "/etc/lu/agent.token"
work_dir = "/var/lib/lu/work"
harness = "opencode"
```
