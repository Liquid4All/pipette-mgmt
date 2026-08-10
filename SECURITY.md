# Security Policy

## Supported Versions

Security fixes target the current `main` branch and the latest public release.
Older releases may receive fixes at maintainer discretion. Please confirm a
finding against current `main` where you can.

## Reporting a Vulnerability

Please do not open a public issue or pull request for a suspected
vulnerability, and please do not disclose details publicly until a fix is
available.

Report privately through either channel:

- **GitHub** — open the
  [Security tab](https://github.com/Liquid4All/pipette-mgmt/security/advisories/new)
  and choose *Report a vulnerability*. The report stays visible only to you and
  the maintainers.
- **Email** — send the report to support@liquid.ai.

Maintainers should acknowledge reports within 7 business days and coordinate
next steps with the reporter.

A useful report includes:

- what an attacker can do, and what they need in order to do it (network
  reachability, a registered client identity, an operator's storage
  credentials);
- the affected version or commit;
- steps to reproduce, ideally against a local `serve` instance;
- any log output or request/response capture that shows the behavior — with
  keys, signatures, and tokens redacted.

## Scope

This repository is the management server: the HTTP API, submission
verification, scoring orchestration, and warehouse writes. Findings in the
client harnesses belong to
[pipette-clients](https://github.com/Liquid4All/pipette-clients).

The security-relevant surfaces here are request authentication and client
identity (see [`docs/authentication.md`](docs/authentication.md)), the
submission path that accepts and stores client-supplied data, and object-store
access.

Reports we are glad to receive but do not treat as vulnerabilities in this
project:

- findings that require an operator's own object-store credentials or filesystem
  access — that trust is assumed by the deployment model;
- vulnerabilities in third-party crates, unless this project's use of the crate
  is what makes them exploitable. Report those upstream; tell us as well if a
  version bump here is the fix.
