# Switchback

A unified AI execution gateway — one endpoint that routes every AI call across providers, accounts, local runtimes, and (later) tools/agents, with a fallback ladder, format translation, and explainable routing. Rust data plane, clean control plane. Built to stay valuable whatever providers do next.

> **Name:** *switchback* — a road that keeps climbing by re-routing. Switching + resilience.

## Status

Pre-build. Research phase complete. The decision to make **before any code** is the *road* (see the deconstruction §13): single-user local power tool · hosted business · own-the-client. The gateway core is necessary for all three, sufficient for none.

## Docs

| Doc | What it is |
|-----|-----------|
| [`docs/9router-DECONSTRUCTION.md`](docs/9router-DECONSTRUCTION.md) | **Start here.** Comprehensive deconstruction of `decolua/9router` (the strongest proof-of-demand in this category) + a greenfield build-better blueprint, reconciled against the AI-execution-gateway research. Evidence-tagged, every claim traced to a file. |
| [`docs/9router-audit/`](docs/9router-audit/) | The 5 supporting subsystem audits behind the deconstruction — core engine, executors/auth, translator/RTK, MITM/CLI, dashboard/data. File-level detail. |

### What to steal from 9router (legitimate, portable)
Hub-and-spoke canonical IR · per-(account,model) rate locks · refresh-dedup vs token-family-revoke · config-driven error→cooldown · RTK fail-safe tool-result compression.

### What to NOT inherit
Subscription impersonation/arbitrage · plaintext secrets · blob data model (no model catalog / price ledger / credential vault / FKs / tenancy) · full-prompt logging on by default.
