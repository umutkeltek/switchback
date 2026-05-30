# CLAUDE.md — Switchback (Claude Code)

**Read `AGENTS.md` first — it is the source of truth for architecture, invariants, and conventions.** This file adds Claude-Code-specific guidance only.

## The one-paragraph orientation

Switchback is a local-first **AI execution gateway** in Rust: one binary that normalizes every AI call into a **canonical typed IR** (`sb-core`), routes it with an **explainable `RouteDecision`** + fallback, and streams it back in the client's format. Crate graph: `sb-core` ← {`sb-adapter`, `sb-protocols`, `sb-router`} ← `sb-adapters` ← `sb-server` (binary `switchback`). Design docs are in `docs/` (git-ignored, private).

## Hard rules (repeat of the invariants you must not break)

1. **Core is provider-agnostic.** No OpenAI/Anthropic JSON shapes in `sb-core`. Translation lives in `sb-protocols`/adapters.
2. **Explainable routing.** Every request emits a `RouteDecision`.
3. **No secrets in logs.** Metadata-only logging; `Secret`/`CredentialLease` redact in `Debug`.
4. **Streaming-first, one path.** Adapters emit `AiStreamEvent`; non-stream = collect.

## Verification is mandatory (do not claim "done" without it)

This repo follows the PAI rule: **claims need tool evidence.** Before saying a change works:

```bash
cargo build && cargo test            # must be green
cargo clippy --all-targets           # should be clean (warnings are debt)
cargo run -p sb-server -- serve --config config/switchback.example.yaml &  # then curl-smoke it
```

For any change touching the request path, run the **live mock smoke test** (stream + non-stream) from AGENTS.md. "Should work" / "looks fine" / "tests pass" without the actual output is not evidence. Reproduce HTTP behavior with `curl -i`/`curl -N`, not by reading code.

## Working style here

- Prefer small, focused edits and `feat:`/`fix:` commits. Keep the crate graph acyclic.
- When adding an adapter or protocol, follow the recipes in AGENTS.md; reuse `sb-protocols::openai` rather than re-implementing OpenAI↔canonical.
- Don't add a crate, a provider, or a feature beyond the v1 scope in AGENTS.md without asking the maintainer (Umut).
- The governing rule: **don't widen the provider surface faster than you harden the seams.**

## Quick commands

```bash
cargo run -p sb-server -- serve                  # serve (default config)
cargo run -p sb-server -- doctor                 # config/health diagnostics
cargo test -p sb-core                            # fast core tests
```
