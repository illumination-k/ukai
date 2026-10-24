# ukai's cluster side: `ukai-gateway`, `ukai-knowledge`, and the
# `ukai-relay` that agent pods copy out of this image (a static binary, so
# any agent image can run it).
#
# docker build --platform linux/amd64 -f images/runtime/Dockerfile -t <registry>/ukai-runtime .
#
# Every change on main that reaches either binary is published as
# ghcr.io/illumination-k/ukai-runtime:{latest,<commit>}
# (.github/workflows/image_runtime.yml).
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
# `dev` for e2e/gateway, which wants the image soon rather than small.
ARG CARGO_PROFILE=release
RUN out=$([ "$CARGO_PROFILE" = dev ] && echo debug || echo "$CARGO_PROFILE") \
  && cargo build --profile "$CARGO_PROFILE" --locked -p ukai-relay --target x86_64-unknown-linux-musl \
  && cargo build --profile "$CARGO_PROFILE" --locked -p ukai-gateway -p ukai-knowledge \
  && cp "target/x86_64-unknown-linux-musl/$out/ukai-relay" "target/$out/ukai-gateway" \
    "target/$out/ukai-knowledge" /usr/local/bin/

FROM debian:trixie-slim

RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates \
  && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/ukai-relay /usr/local/bin/ukai-gateway /usr/local/bin/ukai-knowledge /usr/local/bin/

USER 65534:65534
ENTRYPOINT ["ukai-gateway"]
