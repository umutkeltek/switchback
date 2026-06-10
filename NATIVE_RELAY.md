# Native Codex / Claude Code Relay Plan

Product-level requirements and promotion gates live in `NATIVE_RELAY_SPEC.md`.
This file is the shorter implementation plan and status note.

Switchback currently supports two different native-client outcomes:

- **Client-native ingress:** Codex and Claude Code can point at Switchback and
  keep their expected OpenAI Responses / Anthropic Messages client surfaces.
- **Native token-source adapter:** Switchback can lease access tokens from the
  local Codex or Claude Code auth stores and attach them as bearer credentials
  where an upstream accepts that contract.

That is not the same thing as first-party subscription-native upstream relay.
The relay track below exists to keep that distinction explicit. Local shell
lanes and machine-specific setup are separate from the product relay contract.

## Non-Negotiable Boundary

Do not route a native relay provider until native wire fixtures exist and the
adapter is implemented against those fixtures. `claude_code_native_relay` has a
first non-stream relay adapter covered by a sanitized Claude Code fixture.
`codex_native_relay` has a first HTTP Responses relay adapter covered by a
sanitized Codex native-auth fixture. Full Codex WebSocket conformance is still
tracked below.

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
   - `codex_native_relay` (first HTTP Responses relay slice implemented)
   - `claude_code_native_relay` (first non-stream relay slice implemented)
   - These are separate from `openai_compatible` and `anthropic` providers so
     public API bearer adapters cannot masquerade as subscription relay.

4. **Relay adapters**
   - Add codecs/signers/transports only after fixtures exist.
   - Claude Code currently reuses the Anthropic Messages codec shape with
     bearer native OAuth and the captured `x-anthropic-billing-header`.
   - Codex currently uses the ChatGPT Codex backend Responses endpoint with
     bearer native OAuth plus `chatgpt-account-id`.
   - Codex WebSocket transport still needs distinct fixture capture before
     enablement.
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

## OpenCode client

OpenCode talks the OpenAI **Chat Completions** wire via `@ai-sdk/openai-compatible`,
so it does not need a native-relay provider or a bespoke profile — it points at
Switchback's `/v1` and is decoded by the existing `openai` ingress. A drop-in
config is in `config/opencode.example.json` (set `baseURL` to your bind, copy to
`~/.config/opencode/opencode.json`). Routing/relay/tracing then apply exactly as
for any other client; tool calls ride the Chat Completions tool-call frames,
which the decoder already handles.

## Capability conformance — HTTP Responses slice

The Responses native-relay slice (Codex) now decodes and re-renders the full
agentic surface, verified live against the ChatGPT Codex backend except where
noted:

- **tool calls / tool results** — `function_call` output items,
  `function_call_arguments` deltas, and multi-turn `function_call_output`
  replay are fixture-backed for Codex HTTP Responses
  (`codex/tool_call_and_tool_result.json`).
- **reasoning** — `reasoning_summary_text` deltas → a reasoning item ahead of the
  answer (live).
- **vision input** — native-relay targets advertise `vision_in` so screenshots
  route (live).
- **generated images / citations** — `image_generation_call` + `output_text.
  annotation.added` (unit-proven; a coding backend rarely emits these).
- **server tools** — `web_search` / `code_interpreter` / `file_search` lifecycle
  (unit-proven).
- **expired native token** — proactive JWT-`exp` guard → actionable lease error.

Still open before `--client all` conformance flips green: Codex **WebSocket
transport** capture, Codex model-list/token-count/client-abort/refresh-failure
fixtures, and live Claude Code fixtures (blocked on the Keychain token source
until `claude setup-token`).
