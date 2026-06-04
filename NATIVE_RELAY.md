# Native Codex / Claude Code Relay Plan

Switchback currently supports two different native-client outcomes:

- **Client-native ingress:** Codex and Claude Code can point at Switchback and
  keep their expected OpenAI Responses / Anthropic Messages client surfaces.
- **Native token-source adapter:** Switchback can lease access tokens from the
  local Codex or Claude Code auth stores and attach them as bearer credentials
  where an upstream accepts that contract.

That is not the same thing as first-party subscription-native upstream relay.
The relay track below exists to keep that distinction explicit.

## Non-Negotiable Boundary

Do not route a native relay provider until native wire fixtures exist and the
adapter is implemented against those fixtures. `claude_code_native_relay` has a
first non-stream relay adapter covered by a sanitized Claude Code fixture.
`codex_native_relay` still parses as intent but must fail closed before serving.

## Implementation Sequence

1. **Protocol audit**
   - Detect local Codex and Claude Code installations.
   - Inspect native auth stores by shape only; never print token values.
   - Capture the first-party upstream URLs, auth headers, request bodies,
     stream framing, model listing, token counting, error bodies, and refresh
     behavior in sanitized fixtures.
   - Fixture coverage is tracked in
     `crates/sb-protocols/tests/fixtures/native-relay/manifest.json`.
   - Raw debug/HAR/log material must be passed through
     `switchback setup native-relay capture --from-file ... --out-file ...`
     before it is committed as a fixture.

2. **Auth-store contract**
   - Decide whether Switchback reads only the native stores or also refreshes
     and persists rotated tokens.
   - Any write-back must be atomic, redacted in logs, and opt-in.

3. **Typed relay providers**
   - `codex_native_relay`
   - `claude_code_native_relay` (first non-stream relay slice implemented)
   - These are separate from `openai_compatible` and `anthropic` providers so
     public API bearer adapters cannot masquerade as subscription relay.

4. **Relay adapters**
   - Add codecs/signers/transports only after fixtures exist.
   - Claude Code currently reuses the Anthropic Messages codec shape with
     bearer native OAuth and the captured `x-anthropic-billing-header`.
   - Codex still needs distinct wire capture before adapter enablement.
   - Keep provider wire JSON out of `sb-core`; translate at protocol/adapter
     edges into the canonical IR.

5. **Conformance suite**
   - Non-stream request.
   - Stream request through first byte and finish.
   - Tool call / tool result if supported by the native client.
   - Model list.
   - Token count / count_tokens.
   - Expired token / refresh failure.
   - Client abort before and after first streamed byte.

6. **UX**
   - Dashboard and CLI must show three separate states: mock smoke path, native
     token-source adapter, and first-party native relay.
- Setup packs may install token-source adapters today; relay packs only ship
  after relay conformance passes.
