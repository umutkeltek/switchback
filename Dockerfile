# syntax=docker/dockerfile:1

# ---- build ----
FROM rust:1-slim-bookworm AS build
WORKDIR /src
# build-essential + pkg-config for bundled SQLite (sb-store); libdbus-1-dev for
# the keyring secret-service backend (sb-credentials).
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config libdbus-1-dev \
    && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build --release -p sb-server

# ---- runtime ----
FROM debian:bookworm-slim
# ca-certificates for upstream TLS; libdbus-1-3 because the binary links the
# keyring backend (only used if you configure a keychain vault).
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates libdbus-1-3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --uid 10001 switchback
WORKDIR /app
COPY --from=build /src/target/release/switchback /usr/local/bin/switchback
COPY config/ /app/config/
USER switchback
EXPOSE 8765
# Default: the credential-free mock config (smoke-testable out of the box). For
# real providers, mount your config and override the args:
#   docker run -p 8765:8765 -v $PWD/my.yaml:/app/config/my.yaml \
#     ghcr.io/umutkeltek/switchback serve --config /app/config/my.yaml --bind 0.0.0.0:8765
ENTRYPOINT ["switchback"]
CMD ["serve", "--config", "/app/config/quickstart.yaml", "--bind", "0.0.0.0:8765"]
