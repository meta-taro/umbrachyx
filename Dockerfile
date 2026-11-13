# The server in a container: `docker run` serves one branch over the Redis
# protocol and keeps its log on a volume.
#
# The toolchain is the one rust-toolchain.toml pins, and the build uses
# Cargo.lock as it is, so the image holds what the tests ran against.
FROM rust:1.98.0-slim-bookworm AS build
# The allocator (jemalloc) is built from source and needs make.
RUN apt-get update \
    && apt-get install -y --no-install-recommends make \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p umbrachyx --bin umbrachyx

FROM debian:bookworm-slim
LABEL org.opencontainers.image.source="https://github.com/meta-taro/umbrachyx" \
      org.opencontainers.image.description="An in-memory database that keeps every change as history" \
      org.opencontainers.image.licenses="MIT"
# tini is process 1, so `docker stop` ends the server at once: the kernel
# gives process 1 no default action for the signal `docker stop` sends,
# and the server has no handler of its own. Nothing is lost by stopping it
# this way, since a write is acknowledged only once it is synced.
RUN apt-get update \
    && apt-get install -y --no-install-recommends tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin umbrachyx \
    && mkdir /data \
    && chown umbrachyx:umbrachyx /data
COPY --from=build /src/target/release/umbrachyx /usr/local/bin/umbrachyx
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
USER umbrachyx
WORKDIR /data
# The log, and next to it the file of keys. Keep both: the log without its
# keys cannot be read.
VOLUME /data
ENV UMBRACHYX_DB=/data/umbrachyx.log \
    UMBRACHYX_BRANCH=app
EXPOSE 6380
ENTRYPOINT ["tini", "--", "entrypoint.sh"]
CMD ["serve"]
