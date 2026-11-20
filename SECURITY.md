# Security policy

## Reporting a vulnerability

Email **security@example.net**. This is a placeholder address: replace it with the team's real
security contact before publishing a release. If the project is hosted on a forge with private
vulnerability reporting, that channel is preferable to email.

Please do not open a public issue for a suspected vulnerability. Include the affected version,
the configuration and platform, reproduction steps, and the impact you believe it has. Do not
send real bearer tokens, TLS private keys or job contents; redact them. If a token or key may
have leaked, rotate it as well as reporting it.

We aim to acknowledge a report within three working days and to agree a disclosure date after a
fix is available. This is a small project without a paid security team, so these are targets,
not contractual guarantees.

## Supported versions

| Version | Supported |
| --- | --- |
| 0.2.x | yes |
| 0.1.x | yes |
| earlier | no |

These are pre-1.0 releases. Security fixes may be issued as a new patch release rather than
backported to older patch releases. Always run the newest supported release you can.

## Scope

In scope:

- the controller's HTTP(S) listener, authentication and authorization, request parsing, body
  and connection limits, TLS setup and shutdown behaviour;
- bearer-token handling, token hashing, scope checks, node binding, lease and epoch fencing;
- SQLite state handling, artifact paths and permissions, and log/artifact size limits;
- the operator CLI's token-file checks, output handling and `lu fetch` file writes;
- the supervised harness process boundary (process group, environment, timeouts, output caps);
- the contributed systemd units and sysusers/tmpfiles files where they weaken the protections
  described in [docs/security.md](docs/security.md).

Out of scope:

- vulnerabilities in a coding-agent harness, model provider or git itself, unless Lu's handling
  makes them worse;
- a compromised controller or agent host with the service account's privileges;
- denial of service by an operator or agent that already holds a valid token, beyond the caps
  documented in [docs/security.md](docs/security.md);
- missing features listed as known gaps, unless the gap itself creates a security problem.

The full threat model, token rules, TLS requirements and residual risks are in
[docs/security.md](docs/security.md).
