# Development image for working on ukai itself, e.g. as the image of a
# `transport = "kubernetes"` agent:
#
# docker build --platform linux/amd64 -f images/dev/Dockerfile -t <registry>/ukai-dev .
#
# It carries the toolchain pinned in mise.toml (so the SessionStart hook's
# `mise install` has nothing left to do), the system libraries the Dioxus
# desktop app links against, a virtual display to run and screenshot the app
# (`start-display`), and claude-agent-acp. It runs as any uid: tools and caches
# live under /opt and are writable by everyone, and HOME is expected to be an
# emptyDir.
FROM node:24-trixie-slim

# Build dependencies match .github/workflows/ci_rust.yml; the rest runs the app
# headless: Xvfb, a session bus, input and screenshots, and fonts for the UI.
RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    build-essential ca-certificates curl gh git jq less pkg-config procps ripgrep unzip \
    libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev \
    libxdo-dev librsvg2-dev libssl-dev libasound2-dev cmake clang libclang-dev \
    xvfb xauth dbus dbus-x11 xdotool scrot \
    fonts-noto-core fonts-noto-cjk fonts-noto-color-emoji \
  && rm -rf /var/lib/apt/lists/*

ARG MISE_VERSION=v2026.10.9
ARG CLAUDE_AGENT_ACP_VERSION=0.70.0

ENV MISE_DATA_DIR=/opt/mise \
    MISE_CACHE_DIR=/opt/mise/cache \
    MISE_TRUSTED_CONFIG_PATHS=/workspace \
    RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo \
    HOME=/home/ukai \
    SHELL=/bin/bash \
    DISPLAY=:99 \
    WEBKIT_DISABLE_DMABUF_RENDERER=1 \
    LIBGL_ALWAYS_SOFTWARE=1
ENV PATH=/opt/mise/shims:/opt/cargo/bin:$PATH

RUN curl -fsSL https://mise.run | MISE_VERSION=$MISE_VERSION MISE_INSTALL_PATH=/usr/local/bin/mise sh \
  && npm install -g --no-fund --no-audit @agentclientprotocol/claude-agent-acp@$CLAUDE_AGENT_ACP_VERSION \
  && npm cache clean --force

COPY images/dev/start-display.sh /usr/local/bin/start-display

# The toolchain, then the crates the workspace depends on, prepared under
# /workspace where mise trusts config files. `CI` skips the postinstall hook
# that installs git hooks into a checkout.
WORKDIR /workspace/build
COPY mise.toml mise.lock ./
RUN CI=1 mise install
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo fetch --locked \
  && mkdir -p "$HOME" \
  && chmod -R a+rwX /opt/mise /opt/rustup /opt/cargo "$HOME" \
  && rm -rf /workspace/build

WORKDIR /workspace
