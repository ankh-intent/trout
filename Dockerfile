# Builds a static trout binary and puts it in a small runtime image.
# Used by `fly deploy` (see fly.toml); works with plain `docker build` too.

FROM rust:1-alpine AS build

# C toolchain for the bundled SQLite and for `ring`.
RUN apk add --no-cache build-base

WORKDIR /src

# README.md is rendered into the home page at build time.
COPY Cargo.toml Cargo.lock rustfmt.toml README.md ./
COPY src ./src

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin trout \
    && cp target/release/trout /trout

FROM alpine:3

COPY --from=build /trout /usr/local/bin/trout
COPY deploy/fly/trout.toml /etc/trout/trout.toml

# The conversation store lives on a volume mounted here (see fly.toml).
# The process runs as root so it can write to the root-owned volume.
RUN mkdir -p /data

ENV TROUT_CONFIG=/etc/trout/trout.toml \
    TROUT_LISTEN=0.0.0.0:8080 \
    TROUT_STORE_PATH=/data/trout.db

EXPOSE 8080

CMD ["trout"]
