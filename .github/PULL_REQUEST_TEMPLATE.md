<!-- Thanks for contributing! Keep PRs small and focused. See CONTRIBUTING.md. -->

## What & why

<!-- What does this change, and why? Link any related issue (Fixes #123). -->

## How it was verified

<!-- Claims need evidence. Paste the relevant output. -->

- [ ] `cargo build && cargo test` is green
- [ ] `cargo clippy --workspace --all-targets` is clean
- [ ] `cargo fmt --all --check` passes
- [ ] Request-path changes were smoke-tested with `curl` (stream + non-stream)
- [ ] Added/updated tests for new behavior
- [ ] Updated docs (`README.md` / `AGENTS.md` / example config) if behavior changed

## Invariants

- [ ] `sb-core` stays provider-agnostic (no provider wire shapes in the core IR)
- [ ] Every request still emits an explainable `RouteDecision`
- [ ] No secrets in logs/traces; metadata-only
- [ ] Did not widen the provider surface beyond the `AGENTS.md` v1 scope without discussion
