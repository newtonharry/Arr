# Arr image. Mount your state directory read-write at /state, and your media folders
# (library and downloads) read-write under one mount so imports can hardlink.
FROM rust:1.94.1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
# ffmpeg provides /usr/bin/ffprobe for the media probe used by completed-download import.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates ffmpeg \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /nonexistent arr
COPY --from=build /src/target/release/arr /usr/local/bin/arr
USER 10001
# Inside a container the listener must be explicit: set listen = "0.0.0.0:8787" and
# allow_non_loopback = true in /state/config.toml, and publish the port to 127.0.0.1 only.
ENTRYPOINT ["arr"]
CMD ["serve", "--config", "/state/config.toml"]
