# Agent guide

`AGENTS.md` is a symlink to this file, so Claude Code, Codex, and any other
`AGENTS.md` consumer read the same guidance.

This project is pre-alpha. Nothing is stable; backwards compatibility is not a goal yet.

## Purpose

`ukai` is a desktop client for the [Agent Client Protocol (ACP)](https://agentclientprotocol.com)
that drives many agents side by side — primarily cloud agents reached over
HTTP/WebSocket, plus local stdio agents. Each project declares in a `ukai.toml`
what a pane can open (the command or endpoint for each agent); tabs and splits
are arranged by the user at runtime, never in the file, and agent-written HTML reports
render inside panes.

`docs/design.md` is the design and the milestone plan. Read it before changing
crate boundaries, the `ukai.toml` schema, or how reports are rendered, and
update it in the same change when the design moves.

## Layout

- `crates/ukai-config` — `ukai.toml` schema (serde), the user-level `config.toml` it is layered over (`load_layered`), config/state directory resolution, `${env:NAME}` interpolation, and cross-reference validation.
- `crates/ukai-acp` — resolves an agent spec into an ACP transport (`agent-client-protocol` for stdio, `agent-client-protocol-http` for HTTP/SSE and WebSocket) and runs a session over it: `spawn_session` returns a `SessionHandle` for commands and a channel of `SessionEvent`s. Also the client side of `fs/*`, confined to the project root. No UI types.
- `crates/ukai-core` — UI-independent state: `Projects` (opened project roots under opaque ids), `Layout` (tabs and pane trees the user arranges at runtime; each pane names its project; invariants are property-tested and re-checked on load), `Transcript` (the fold of `SessionEvent`s an agent pane renders), and `state` (the versioned `state.json`, written atomically), `report` (listing reports and images, and serving one to its web view confined to its base directory), `review` (line diffs of the changes tool calls report, and the prompt that sends comments on them), `attachment` (how a file attached to a prompt is sent, given the agent's prompt capabilities), `palette` (fuzzy matching for the command palette), and `slash` (completing the slash commands an agent offers).
- `crates/ukai-app` — the Dioxus 0.7 desktop app (`ukai` binary). Keep logic out of components; put it in a library crate where it can be tested headless.
- `crates/ukai-relay` — runs a stdio ACP agent behind `/acp` with sessions that outlive client connections: answers reconnects' `initialize` and `session/load` from what it recorded, keeps turns running with no client, and re-sends pending permission requests. The protocol side (`hub`) is pure; extend its tests for behavior changes.
- `crates/ukai-gateway` — serves the agents in a `gateway.toml` at `/agents/<id>/acp` (WebSocket) from one namespace, starting a pod per workspace and relaying to the `ukai-relay` in it. `config` is the `gateway.toml` schema and its validation; `job` builds the pod's Job spec (pure, tested as JSON); `scripts/workspace.sh` is the sidecar that clones and snapshots, the only container holding git credentials.
- `deploy/gateway` — kustomize base for the gateway; environment-specific overlays stay out of the repository. `images/runtime` builds the gateway and relay, `images/dev` an agent image for working on this repository.
- `ukai.toml` — this repository's own config, and the reference example of the format. `ukai-config`'s tests parse it, so keep it valid.

## Development Process

Run `mise install` first. All tools and tasks live in the single `mise.toml`.

```bash
mise run fmt   # cargo fmt, dx fmt (rsx!), dprint, shfmt
mise run lint  # clippy, fmt checks, cargo-deny, cargo-audit, dprint, shellcheck, GHA lint, prek
mise run test  # cargo-nextest and doctests
mise run ci    # lint + test; run before finishing a session
mise run dev   # dx serve the desktop app with hot reload
```

To check a real agent without the UI, run
`cargo run -p ukai-acp --example prompt -- <agent-id> "<prompt>"` from the
repository root; it uses the agents in `ukai.toml` and declines every
permission request.

Session tests in `crates/ukai-acp/src/session/tests.rs` run against an
in-process mock agent over `Channel::duplex`; extend the mock rather than
reaching for a real agent in tests.

The `dx` CLI version in `mise.toml` must match the `dioxus` crate version in
`Cargo.toml`; bump them together.

Use [`rstest`](https://docs.rs/rstest) for parameterized cases. Validation and
interpolation code collects every problem rather than stopping at the first;
keep it that way and test each problem variant.

Logs go through `tracing` to stderr. Never log header values or anything read
through `${env:...}`: those are credentials.

When running the app for a manual check, point it at throwaway directories so
it does not touch your real projects and layout:
`UKAI_STATE_DIR=$(mktemp -d) UKAI_CONFIG_DIR=$(mktemp -d) cargo run -p ukai-app`.
Stop it by PID, not with a `pkill` pattern that also matches a copy you are
using.

## Security notes

- Agent-produced HTML is untrusted. It renders in a child web view of its own
  with no IPC handler; never in an iframe of the app's web view, whose IPC
  bridge every frame can reach (see "HTML レポートと画像" in `docs/design.md`).
- ACP `fs/*` requests are confined to the project root after path
  normalization, including symlinks.
- Secrets are only ever referenced as `${env:NAME}`; do not add a config key
  that stores one inline.
