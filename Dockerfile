# syntax=docker/dockerfile:1

FROM --platform=$BUILDPLATFORM node:22-bookworm-slim AS web
WORKDIR /src
COPY package.json package-lock.json ./
COPY frontend/package.json frontend/package.json
RUN npm ci
COPY frontend frontend
RUN npm run build:web

# Compile on the build machine for the requested architecture instead of emulating it.
FROM --platform=$BUILDPLATFORM rust:1-bookworm AS api
ARG TARGETARCH
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-gnu; gcc=x86-64-linux-gnu ;; \
      arm64) target=aarch64-unknown-linux-gnu; gcc=aarch64-linux-gnu ;; \
      *) echo "Unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    echo "$target" > /rust-target; \
    if [ "$(uname -m)" != "${target%%-*}" ]; then \
      apt-get update; \
      apt-get install -y --no-install-recommends "gcc-$gcc" "libc6-dev-$TARGETARCH-cross"; \
      rm -rf /var/lib/apt/lists/*; \
    fi; \
    rustup target add "$target"
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY backend backend
COPY integrations integrations
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=investigenie-target-$TARGETARCH \
    target="$(cat /rust-target)" \
    && cargo build --locked --release --target "$target" -p investigenie-api \
    && cp "target/$target/release/investigenie-api" /investigenie-api

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --no-create-home --shell /usr/sbin/nologin investigenie \
    && mkdir /data \
    && chown 10001:10001 /data
COPY --from=api /investigenie-api /app/investigenie-api
COPY --from=web /src/frontend/dist /app/web
COPY LICENSE /app/LICENSE
COPY investigenie.example.toml /app/investigenie.toml
WORKDIR /app
ENV RUST_API_BIND=0.0.0.0:3000 \
    WEB_DIR=/app/web \
    FILESYSTEM_PATH=/data \
    LOG_PATH=/data/logs
USER 10001:10001
VOLUME /data
EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=8s --start-period=15s --retries=3 CMD ["/app/investigenie-api", "healthcheck"]
ENTRYPOINT ["/app/investigenie-api"]
