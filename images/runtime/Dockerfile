# ukai's cluster side: `ukai-gateway`, and the `ukai-relay` that agent pods
# copy out of this image (a static binary, so any agent image can run it).
#
# docker build --platform linux/amd64 -f images/runtime/Dockerfile -t <registry>/ukai-runtime .
#
# The gateway runs this image with `UKAI_RELAY_IMAGE` set to it.
FROM rust:1.97.1-trixie AS build

RUN apt-get update \
  && apt-get install -y --no-install-recommends musl-tools \
  && rm -rf /var/lib/apt/lists/* \
  && rustup target add x86_64-unknown-linux-musl

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo build --release --locked -p ukai-relay --target x86_64-unknown-linux-musl \
  && cargo build --release --locked -p ukai-gateway \
  && cp target/x86_64-unknown-linux-musl/release/ukai-relay target/release/ukai-gateway /usr/local/bin/

FROM debian:trixie-slim

RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates \
  && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/ukai-relay /usr/local/bin/ukai-gateway /usr/local/bin/

USER 65534:65534
ENTRYPOINT ["ukai-gateway"]
