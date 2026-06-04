# OPERATIONS — deploying Switchback for a team

This is the operator runbook for running the Switchback AI execution gateway as a
shared service. For architecture and invariants read `AGENTS.md`; for the config
reference read the inline comments in `config/switchback.example.yaml`.

Goal: a working team gateway in minutes.

---

## 1. Prerequisites

- Docker Engine 24+ with the Compose v2 plugin (`docker compose version`).
- The repo checked out (Compose builds the in-repo `Dockerfile`), **or** access to
  the published image `ghcr.io/umutkeltek/switchback` if you prefer not to build.
- Upstream provider API keys for whichever providers your config uses
  (OpenRouter, Anthropic, Gemini, Vertex, Bedrock). The bundled mock provider
  needs none, so you can stand the gateway up and smoke-test it with zero keys.

The container image is produced by the existing root `Dockerfile` (multi-stage:
a Rust builder runs `cargo build --release -p sb-server`, a `debian-slim` runtime
stage carries only the `switchback` binary + the `config/` registries). It
**binds nothing on its own** — the `serve` args decide the bind. The release
workflow (`.github/workflows/release.yml`) publishes a multi-arch
(`linux/amd64,linux/arm64`) image to GHCR on every `v*` tag.

---

## 2. Start it

```bash
# from the repo root
docker compose up -d            # build (first run) + start in the background
docker compose logs -f          # follow logs
docker compose ps               # state
docker compose down             # stop (keeps the state volume)
docker compose down -v          # stop AND delete the SQLite state volume
```

The gateway publishes **port 8765** on the host (`localhost:8765`). To run the
prebuilt GHCR image instead of building, comment out `build:` and uncomment the
`image:` line in `docker-compose.yml`.

By default Compose mounts `config/switchback.example.yaml` read-only at
`/etc/switchback/switchback.yaml`. That example config references real providers,
so the gateway expects their key envs — for a zero-credential first boot, point
the mount at a mock-only config instead (see §5).

### Secrets via a `.env` file

Compose reads a sibling `.env` automatically. Put provider keys there (never
commit it):

```dotenv
# .env  (gitignored)
OPENROUTER_API_KEY=sk-or-...
ANTHROPIC_API_KEY=sk-ant-...
GEMINI_API_KEY=...
# per-tenant gateway key referenced by api_keys: in the config
SWITCHBACK_ACME_KEY=sk-acme-...
```

Only the keys your active config references need to be set; the rest pass through
empty and are ignored. The env vars the example config can reference, all wired
through in `docker-compose.yml`:
`OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`,
`VERTEX_ACCESS_TOKEN`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
`AWS_SESSION_TOKEN`, `SWITCHBACK_ACME_KEY`, `SWITCHBACK_VAULT_KEY`,
`NVIDIA_PROXY_URL`.

---

## 3. The bind-safety rule (read this before exposing the gateway)

Compose runs the gateway with `--bind 0.0.0.0:8765` so the published port works.
`0.0.0.0` is a **non-loopback** bind, and Switchback **refuses to start an
unauthenticated open admin gateway on a non-loopback bind**. The check is
`validate_open_admin_bind` in `crates/sb-server/src/serve.rs`:

> a non-loopback bind requires `server.api_key` **or** `server.api_keys`
> **or** `server.allow_open_admin: true` — otherwise startup fails with
> *"refusing unauthenticated admin gateway on non-loopback bind"*.

So before `docker compose up` with the default args, the mounted config **must**
do one of:

```yaml
server:
  bind: "127.0.0.1:8765"          # (compose overrides this to 0.0.0.0 anyway)
  api_key: "sk-switchback-team"   # 1) single shared key — simplest
```

or use the multi-tenant key list (§4):

```yaml
api_keys:
  - key_env: SWITCHBACK_ACME_KEY
    tenant: acme
```

or, **only** when the gateway is already behind a trusted network boundary
(private VPC, a reverse proxy that does auth), explicitly opt out:

```yaml
server:
  allow_open_admin: true          # you accept an unauthenticated admin surface
```

When `api_key`/`api_keys` is set, **every** `/v1/*` and `/cp/v1/*` endpoint
requires the key (`Authorization: Bearer <key>`); only `/` and `/health` stay
open. There is no half-open mode — auth gates inference *and* the control plane.

> Bonus hardening: on a non-loopback bind the SSRF guard is still off by default.
> Set `server.block_private_networks: true` if upstream/proxy/token URLs should
> not be allowed to reach private or link-local addresses (e.g. cloud metadata).

---

## 4. Tenants, API keys, and budgets

Multi-tenancy is opt-in. An API key maps to a `Principal` (tenant + project) at
the edge; usage is attributed per tenant and hard limits reject **before**
upstream dispatch.

```yaml
tenants:
  - id: acme
    allowed_routes: ["default"]        # optional allow-lists
    allowed_providers: ["openrouter", "anthropic"]
    budget_usd: 100.0                  # cumulative spend cap → 402 when exceeded
    max_concurrency: 8                 # simultaneous in-flight → 429 when exceeded

api_keys:                              # authoritative key list; unknown key → 401
  - key_env: SWITCHBACK_ACME_KEY       # prefer key_env / key_hash over inline key
    tenant: acme
    project: web                       # optional attribution label
```

Notes:

- Do **not** set both `server.api_key` and `api_keys` — pick one model. `api_key`
  is a single shared key; `api_keys` is the per-tenant list.
- Prefer `key_env:` (pass the value through `.env` / the environment) or
  `key_hash:` (`sha256:<64 hex>`) over an inline `key:` — inline secrets end up in
  any durable draft/config the control plane persists.
- Budgets read the live durable rollup when the state store is enabled (§5), so
  the cap is enforced consistently across restarts and across gateway processes
  sharing one store; otherwise spend/concurrency are process-local.

---

## 5. Durable state store (SQLite) + backups

The gateway is in-memory by default. Enable the durable store so config
revisions, the audit trail, usage events, idempotency claims, and
admission/tenant-concurrency coordination survive restarts. Point it at the path
inside the named volume that Compose mounts:

```yaml
server:
  state_store:
    path: "/var/lib/switchback/state.sqlite"
    required: false   # true = fail startup (don't silently degrade to memory)
                      # and make non-streaming usage persistence + budget reads
                      # fail closed
```

Compose mounts the named volume **`switchback-state`** at `/var/lib/switchback`,
so the SQLite file lives there and persists across `docker compose down`
(only `down -v` deletes it).

### Where the volume actually lives + backup

```bash
# Find the on-disk path of the named volume:
docker volume inspect switchback_switchback-state --format '{{ .Mountpoint }}'

# Hot online backup with SQLite's safe .backup (no torn reads):
docker compose exec switchback sh -c \
  'sqlite3 /var/lib/switchback/state.sqlite ".backup /var/lib/switchback/backup.sqlite"' 2>/dev/null \
  || echo "sqlite3 not in image — use the volume-copy method below"

# Portable backup that needs no tools in the runtime image: copy the volume out
# with a throwaway helper container.
docker run --rm \
  -v switchback_switchback-state:/data:ro \
  -v "$PWD":/backup \
  busybox sh -c 'cp /data/state.sqlite /backup/state-$(date +%F).sqlite'
```

> The volume name is `switchback_switchback-state` (Compose prefixes the volume
> with the project name `switchback`). The state DB holds **metadata only**
> (revisions/audit/usage) by default; treat it as sensitive only if you enable
> durable `/cp/v1` drafts, which persist full config bodies including secrets.

---

## 6. Health, usage, and traces

The runtime image is `debian-slim` and ships **no curl/wget**, so probe from the
host (or any client on the network). When `api_key`/`api_keys` is set, pass the
key on the `/v1/*` routes; `/health` stays open.

```bash
KEY=sk-switchback-team   # whatever you set for server.api_key

# Liveness — open, no auth required:
curl -fs localhost:8765/health

# Rich health: admission headroom, account-pool health, routing readiness:
curl -s localhost:8765/v1/health        -H "Authorization: Bearer $KEY" | jq

# Native client readiness: Codex uses /v1/responses, Claude Code uses
# /v1/messages. The account checks are Switchback provider/accounts, with secret
# values redacted; local Codex/Claude auth stores are not read.
curl -s localhost:8765/v1/client-profiles \
  -H "Authorization: Bearer $KEY" | jq

# Usage + cost rollup (by tenant when multi-tenancy is on); the `durability`
# field reports memory-only / durable / degraded once a state store is attached:
curl -s localhost:8765/v1/usage         -H "Authorization: Bearer $KEY" | jq

# Per-request traces (metadata-only: route decision + every account/egress
# attempt + cost). Recent traces live in memory; server.trace_log appends JSONL;
# server.state_store persists queryable trace metadata in SQLite.
curl -s localhost:8765/v1/traces        -H "Authorization: Bearer $KEY" | jq
curl -s 'localhost:8765/v1/traces?session_id=sess-123&model=mock/echo&status=200' \
  -H "Authorization: Bearer $KEY" | jq

# Session rollups and trace lists from metadata (no prompts/responses stored):
curl -s localhost:8765/v1/sessions      -H "Authorization: Bearer $KEY" | jq
curl -s localhost:8765/v1/sessions/sess-123 \
  -H "Authorization: Bearer $KEY" | jq
curl -s localhost:8765/v1/sessions/sess-123/traces \
  -H "Authorization: Bearer $KEY" | jq

# Replay routing for a trace against the current config without executing.
# The response includes original/current decisions plus a small diff:
REQ_ID=req_...
curl -s localhost:8765/v1/traces/$REQ_ID/route-preview \
  -H "Authorization: Bearer $KEY" | jq
```

Langfuse export is the same OpenTelemetry span stream with a convenience config.
Build with the `otel` feature, set `server.langfuse.enabled: true`, and provide
`LANGFUSE_PUBLIC_KEY` / `LANGFUSE_SECRET_KEY`. The exported span fields are
metadata-only route/session/account facts; Switchback still does not store
prompt or response bodies.

End-to-end smoke (mock provider, no upstream credentials needed):

```bash
curl -s localhost:8765/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"mock/echo","messages":[{"role":"user","content":"hi"}]}'

# Codex-shaped request. Configure Codex's provider/base URL to this gateway and
# keep using Switchback's bearer key; upstream accounts are selected by routes.
curl -s localhost:8765/v1/responses \
  -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -H 'x-codex-session-id: sess-codex-1' \
  -d '{"model":"mock/echo","input":"hi from codex"}'

# Claude Code-shaped request.
curl -s localhost:8765/v1/messages \
  -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -H 'x-switchback-session-id: sess-claude-1' \
  -d '{"model":"mock/echo","messages":[{"role":"user","content":"hi from claude"}]}'
```

---

## 7. Promote to a real config

1. Copy the example and add keys (the copy is gitignored):
   `cp config/switchback.example.yaml config/switchback.yaml`
2. Set `server.api_key` or `api_keys:` (the bind-safety rule, §3), enable
   `server.state_store` (§5), and define your `providers:` / `routes:` /
   `tenants:`.
3. Point the Compose mount at it — edit `docker-compose.yml`:
   ```yaml
   volumes:
     - ./config/switchback.yaml:/etc/switchback/switchback.yaml:ro
   ```
4. Validate before shipping (the prebuilt binary or the image both expose it):
   ```bash
   docker compose run --rm switchback config validate \
     --config /etc/switchback/switchback.yaml
   ```
5. `docker compose up -d --build` to roll the new image/config.

Config changes can also be applied live without a restart via the control plane
(`POST /v1/reload`, or the `/cp/v1` draft→validate→publish lifecycle) — YAML stays
the bootstrap source of truth.
