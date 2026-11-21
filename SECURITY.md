# Security policy

## Reporting a vulnerability

Report vulnerabilities privately through the project's forge security channel: **Security →
Report a vulnerability** (GitHub private vulnerability reporting). Use that channel instead of a
public issue or an email address; it keeps the report confidential and gives the maintainers a
private thread for follow-up. If private reporting is not enabled for this repository, contact a
maintainer privately and ask for the current security address rather than guessing one.
Report suspected vulnerabilities through GitHub private vulnerability reporting: use the
**Report a vulnerability** button on the [Security tab](https://github.com/ddproxy/lu/security)
or [submit a private advisory](https://github.com/ddproxy/lu/security/advisories/new). Do not
open a public issue.

Include the affected version, the configuration and platform, reproduction steps, and the impact
you believe it has. Do not send real bearer tokens, TLS private keys or job contents; redact them.
If a token or key may have leaked, rotate it as well as reporting it.

We aim to acknowledge a report within three working days and give an initial assessment within
ten working days. We agree a disclosure date once a fix is available.

## Supported versions

Only the [latest release](https://github.com/ddproxy/lu/releases/latest) receives security fixes.
Older releases are not supported: upgrade to the latest release.

| Version | Supported |
| --- | --- |
| 0.4.x | yes |
| 0.3.x | yes |
| 0.2.x | yes |
| 0.1.x | yes |
| earlier | no |
| Latest release | yes |
| Older releases | no |

## Security scanning

The scanning workflows, where their findings appear, and how to verify a release are described in
[Security scanning](docs/security-scanning.md).

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
