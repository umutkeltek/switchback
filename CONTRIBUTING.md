# Contributing to Switchback

Thanks for your interest. Switchback is a focused, invariant-driven codebase —
a few rules keep it that way.

## Read `AGENTS.md` first

[`AGENTS.md`](AGENTS.md) is the source of truth for architecture, the golden-rule
invariants, conventions, and the recipes for adding a provider or a wire protocol.
Read it before opening a PR. The short version:

- **The core never sees provider wire formats.** `sb-core` is provider-agnostic;
  OpenAI/Anthropic/etc. JSON lives in `sb-protocols` and the adapters.
- **Every request emits an explainable `RouteDecision`.**
- **Secrets are leases and are never logged.** Metadata-only logging.
- **Streaming-first, one path.** Adapters emit `AiStreamEvent`; non-stream =
  collect that stream.
- **Don't widen the provider surface faster than you harden the seams.**

## Development

```bash
cargo build                  # whole workspace
cargo test                   # all crates
cargo clippy --workspace --all-targets   # must be clean (CI denies warnings)
cargo fmt --all              # before committing (CI checks --check)
```

Run the gateway locally with the zero-setup config (no API keys needed):

```bash
cargo run -p sb-server -- serve --config config/quickstart.yaml
curl -s localhost:8765/v1/chat/completions -H 'content-type: application/json' \
  -d '{"model":"mock/echo","messages":[{"role":"user","content":"hi"}]}'
```

Optional features pull heavyweight deps and are off by default:
`--features wasm` (Wasmtime plugin sandbox), `--features otel` (OpenTelemetry).

## Verification is mandatory

Claims need tool evidence. Before saying a change works, it must be **green**
(`cargo build && cargo test`) and, for anything on the request path, smoke-tested
with `curl` (stream + non-stream) — not "should work". A behavior change to
streaming/tool-calls requires a streamed-fixture test.

## Pull requests

- Keep PRs small and focused; one concern per PR.
- Use **conventional commits** (`feat:`, `fix:`, `refactor:`, `test:`, `docs:`,
  `chore:`). Small, focused commits.
- Add tests for new behavior; keep `cargo fmt`/`clippy` clean.
- Don't add a crate, a provider, or a feature beyond the v1 scope in `AGENTS.md`
  without opening an issue to discuss first.
- Update docs (`README.md` / `AGENTS.md` / the example config) when you change
  behavior or add a surface.

## Scope guardrails

Switchback does network-path selection (proxy/egress) only. It will **not** accept
TLS/JA3 fingerprint mimicry, official-client impersonation, anti-bot evasion,
free-tier pooling, or subscription-bypass code. Legitimate multi-account, egress,
and routing are all in scope.

## License of contributions

By contributing, you agree your contributions are licensed under the
[Elastic License 2.0](LICENSE), the same license as the project.

## Reporting bugs / security issues

Functional bugs: open an issue (templates provided). Security vulnerabilities:
**do not** open a public issue — see [`SECURITY.md`](SECURITY.md).
