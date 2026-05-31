# Security Policy

## Reporting a vulnerability

**Please do not report security vulnerabilities through public GitHub issues.**

Report privately via GitHub's **[Security Advisories](https://github.com/umutkeltek/switchback/security/advisories/new)**
("Report a vulnerability"). Include:

- a description of the issue and its impact,
- steps to reproduce or a proof of concept,
- affected version / commit, and
- any suggested remediation.

You'll get an acknowledgement, and we'll work with you on a fix and coordinated
disclosure. Please give a reasonable window before any public disclosure.

## Supported versions

Switchback is pre-1.0; security fixes land on `main` and the latest `0.x` release.

| Version | Supported |
| ------- | --------- |
| `0.1.x` | ✅ |
| `< 0.1` | ❌ |

## Security model (what to keep in mind)

- **Deployment matters.** Bound to `127.0.0.1` it is a local-first tool. Exposed
  on a shared/team port it must have an API key set — see below.
- **Auth.** When `server.api_key` (or `api_keys`) is configured, **every** endpoint
  except `/` and `/health` requires it (config, providers, traces, usage, and the
  whole control plane), not just inference. With no key configured the gateway is
  open — only do that on a trusted local interface.
- **Secrets.** Credentials are redacting leases (`Secret` never serializes and
  redacts in `Debug`/`Display`). Logs and traces are **metadata-only** — no
  prompts, responses, or secrets. The credential vault is age-encrypted with the
  key in the OS keychain.
- **Egress.** An egress profile selects a network path only; it cannot set or
  override auth-bearing headers. Switchback does **not** do TLS/JA3 fingerprint
  spoofing or client impersonation.

## Known gaps (hosted/multi-tenant hardening, not yet implemented)

Tracked, partially built: set `server.block_private_networks: true` for hosted
mode to reject literal localhost/private/link-local provider `base_url`, proxy
URLs, and OAuth `token_url` values during validation/startup. DNS rebinding and
operator-defined allowlists are still not implemented. Still open: constant-time
API-key comparison, OAuth rotated-refresh-token persistence, and atomic vault
writes. Treat hosted multi-tenant deployment as not-yet-ready; team/local use is
the supported mode.
