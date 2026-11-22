FROM rust:1-bookworm AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked \
    && cp target/release/asset-bot /usr/local/bin/asset-bot

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 bot \
    && mkdir /data \
    && chown bot /data
COPY --from=build /usr/local/bin/asset-bot /usr/local/bin/asset-bot
ENV DATABASE_PATH=/data/asset-bot.db \
    BIND_ADDR=0.0.0.0:8080
USER bot
VOLUME /data
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s \
    CMD curl -fsS http://127.0.0.1:8080/ || exit 1
CMD ["asset-bot"]
