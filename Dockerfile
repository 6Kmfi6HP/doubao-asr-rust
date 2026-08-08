# syntax=docker/dockerfile:1.7

FROM rust:1.86-alpine3.21 AS builder

RUN apk add --no-cache musl-dev
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && \
    mkdir -p /out && \
    cp target/release/doubao-asr target/release/doubao-asr-server /out/ && \
    strip /out/doubao-asr /out/doubao-asr-server

FROM scratch AS artifacts
COPY --from=builder /out/ /

FROM debian:bookworm-slim AS runtime

ARG VERSION=dev
ARG VCS_REF=unknown
ARG SOURCE=https://github.com/6Kmfi6HP/doubao-asr-rust

LABEL org.opencontainers.image.title="doubao-asr-rust" \
      org.opencontainers.image.description="OpenAI-compatible Doubao IME ASR server" \
      org.opencontainers.image.url="${SOURCE}" \
      org.opencontainers.image.source="${SOURCE}" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${VCS_REF}" \
      org.opencontainers.image.licenses="MIT"

RUN apt-get update && \
    apt-get install --no-install-recommends -y ca-certificates curl ffmpeg && \
    rm -rf /var/lib/apt/lists/* && \
    groupadd --system --gid 10001 doubao && \
    useradd --system --uid 10001 --gid doubao --home-dir /data --shell /usr/sbin/nologin doubao && \
    install -d -o doubao -g doubao -m 0700 /data

COPY --from=builder /out/doubao-asr /usr/local/bin/doubao-asr
COPY --from=builder /out/doubao-asr-server /usr/local/bin/doubao-asr-server

ENV DOUBAO_ASR_LISTEN=0.0.0.0:8000 \
    DOUBAO_ASR_CREDENTIALS=/data/asr_credentials.json

USER 10001:10001
VOLUME ["/data"]
EXPOSE 8000
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["curl", "--fail", "--silent", "--show-error", "http://127.0.0.1:8000/healthz"]
ENTRYPOINT ["/usr/local/bin/doubao-asr-server"]
