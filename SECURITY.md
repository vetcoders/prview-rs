# Security Policy

## Supported versions

| Version | Supported |
| ------- | --------- |
| 0.4.x   | Yes       |

## Reporting a vulnerability

If you discover a security vulnerability in prview, please report it
responsibly. **Do not open a public issue.**

### Preferred: GitHub Security Advisories

Report via [GitHub Security Advisories](https://github.com/vetcoders/prview-rs/security/advisories/new).
This allows private discussion and coordinated disclosure.

### Alternative: Email

Contact: hello@vetcoders.io

Include:
- Description of the vulnerability
- Steps to reproduce
- Affected version(s)
- Impact assessment, if possible

## Scope

The following are in scope:

- The `prview` binary and its behavior
- Direct dependencies used at runtime
- Generated artifacts (dashboard HTML, report JSON)

The following are out of scope:

- Vulnerabilities in tools prview invokes (cargo, npm, git) -- report those
  to the respective projects
- Issues that require local code execution privileges beyond what prview
  already assumes (prview runs with the same permissions as the invoking user)

## Known advisory caveats

- `cargo audit` may currently report `RUSTSEC-2024-0436` for `paste 1.0.15`.
- In `prview-rs` this is a transitive dependency from the `loctree -> report-leptos -> leptos` stack, not a directly selected crate in this repository.
- The advisory is tracked as an informational `unmaintained` warning, is surfaced in generated review artifacts, and is not treated as a hidden pass condition.
- Current 0.2.x policy is to keep this caveat documented and visible while waiting for the upstream dependency chain to move off `paste`.

## Response timeline

- **Acknowledgment:** within 48 hours
- **Initial assessment:** within 7 days
- **Fix or mitigation:** within 30 days for confirmed vulnerabilities

## Disclosure

We follow coordinated disclosure. Once a fix is released, we will:

1. Publish a GitHub Security Advisory
2. Document the fix in the CHANGELOG
3. Credit the reporter (unless anonymity is requested)

## Bug bounty

This is an open source project and does not offer a bug bounty program.
