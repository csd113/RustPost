FROM rust:1.91-bookworm AS build

WORKDIR /usr/src/rustpost
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets
RUN cargo build --release --locked --bin rustpost-cli

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 rustpost \
    && useradd --uid 10001 --gid rustpost --home-dir /data --no-create-home rustpost \
    && mkdir /data \
    && chown rustpost:rustpost /data

COPY --from=build /usr/src/rustpost/target/release/rustpost-cli /usr/local/bin/rustpost-cli
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

ARG RUSTPOST_VERSION=1.0.0
LABEL org.opencontainers.image.source="https://github.com/csd113/RustPost" \
      org.opencontainers.image.title="RustPost" \
      org.opencontainers.image.version="${RUSTPOST_VERSION}"

ENV RUSTPOST_CONTAINER=1
USER rustpost
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["serve"]
