# Security policy

## Reporting a vulnerability

Report vulnerabilities privately through the project's forge security channel: use the
**Report a vulnerability** button on the [Security tab](https://github.com/ddproxy/tuishou-lu/security)
or [submit a private advisory](https://github.com/ddproxy/tuishou-lu/security/advisories/new). Use that
channel instead of a public issue or an email address; it keeps the report confidential and gives
the maintainers a private thread for follow-up. If private reporting is not enabled for this
repository, contact a maintainer privately and ask for the current security address rather than
guessing one.

Include the affected version, the configuration and platform, reproduction steps, and the impact
you believe it has. Do not send real bearer tokens, TLS private keys or job contents; redact them.
If a token or key may have leaked, rotate it as well as reporting it.

We aim to acknowledge a report within three working days and give an initial assessment within
ten working days. We agree a disclosure date once a fix is available.

## Supported versions

Only the [latest release](https://github.com/ddproxy/tuishou-lu/releases/latest) receives security fixes.
Older releases are not supported: upgrade to the latest release.

| Version | Supported |
| --- | --- |
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
- the per-job harness socket, its peer-credential check and teardown, the `/v1/scope/` routes and
  the `ting mcp` credential refusal;
- SQLite state handling, artifact paths and permissions, and log/artifact size limits;
- the operator CLI's token-file checks, output handling and `lu fetch` file writes;
- the supervised harness process boundary (process group, environment, timeouts, output caps);
- the intake route's signature checks, rule evaluation, draft approval boundary and untrusted
  context handling, described in [Intake and drafts](docs/security.md#intake-and-drafts);
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
[docs/security.md](docs/security.md). The separate threat model for a compromised harness, the
limits on what it can reach through its per-job socket and the operator views of job lineage are in
[Job-scoped capability](docs/scope.md). A compromised harness gets only that socket: it never holds
controller or operator credentials, and the controller enforces the boundary. The boundary for a
stranger who can post an event to an intake source, including what drafts and untrusted context
may do, is in [Intake and drafts](docs/security.md#intake-and-drafts).
