# Security Policy

## Supported versions

smelt is beta software until 1.0 and currently supports only the latest
published release. Beta maturity is not encoded as a SemVer prerelease: `0.x`
versions are normal releases. Older releases do not receive security fixes.
Upgrade before reporting or verifying a fix whenever practical.

| Version | Supported |
| ------- | --------- |
| Latest release | Yes |
| Older releases | No |

## Reporting a vulnerability

Report suspected vulnerabilities through
[GitHub private vulnerability reporting](https://github.com/leonardcser/smelt/security/advisories/new).
Do not include vulnerability details, proof-of-concept code, credentials, or
other sensitive data in a public issue.

Include the affected version and platform, impact, reproduction steps, and any
suggested mitigation. You should receive an acknowledgement through the private
advisory. Public disclosure will be coordinated after a fix or mitigation is
available.

## Trust model

Lua configuration, plugins, skills, MCP servers, and trusted project `.smelt`
configuration can influence agent behavior. Lua configuration and plugins run
in-process with the same operating-system privileges as smelt. They are not
sandboxed. Review third-party code and project configuration before trusting or
loading it.

Tool permission modes reduce accidental or unintended operations, but they are
not a security boundary against malicious trusted configuration or plugins.
Model providers receive prompts and any context included in a request; choose
providers and attached data accordingly.
