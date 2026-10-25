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

- `crates/ukai-config` — `ukai.toml` schema (serde), the user-level `config.toml` it is layered over (`load_layered`; `[keybindings]` and `[voice]` are user-only), config/state directory resolution, `${env:NAME}` / `${keychain:NAME}` interpolation, cross-reference validation, and `UKAI_REPORT_DIRS`, report directories added from the environment.
- `crates/ukai-acp` — resolves an agent spec into an ACP transport (`agent-client-protocol` for stdio, `agent-client-protocol-http` for HTTP/SSE and WebSocket) and runs a session over it: `spawn_session` returns a `SessionHandle` for commands and a channel of `SessionEvent`s. `grpc` carries the gateway's gRPC services as gRPC-Web over each endpoint's reqwest client, so its headers and TLS apply and HTTP/1.1 proxies (IAP) pass it. Also the client side of `fs/*`, confined to the project root, `reports`, which reads a gateway agent's reports back from its pod over HTTP, `ports`, which lists what listens in that pod and tunnels a local port to one of them over WebSocket, `schedules`, the wire types and client of a gateway agent's schedules, `knowledge`, likewise for a gateway's notes, `checkout`, an agent's checkout as the composer's `@` and `!` reach it (its files, and the user's commands run there; the same functions serve a local agent here and a gateway agent's relay), and `updates`, which tells (from `agentInfo` and the npm registry) when an agent is behind its latest release and restarts a gateway agent's pod to catch up. No UI types.
- `crates/ukai-core` — UI-independent state: `Projects` (opened project roots under opaque ids), `Layout` (tabs and pane trees the user arranges at runtime; each pane names its project; invariants are property-tested and re-checked on load), `Transcript` (the fold of `SessionEvent`s an agent pane renders), and `state` (the versioned `state.json`, written atomically), `report` (listing reports and images, serving one to its web view confined to its base directory, and `ReportTree` for serving a whole checkout's reports, which `ukai-relay` answers from), `markdown` (agent messages parsed into a tree the app renders as elements, never as HTML), `review` (line diffs of the changes tool calls report, and the prompt that sends comments on them), `attachment` (how a file attached to a prompt is sent, given the agent's prompt capabilities), `browser` (the port a user types for a browser pane, and which previews an agent asked for are new), `config` (presenting the settings — model, reasoning effort, mode — an agent lets the user change, and the model and effort a project's new sessions of an agent start with), `palette` (fuzzy matching for the command palette), `slash` (completing the slash commands an agent offers), `recall` (bringing back the prompts sent earlier with ↑ and ↓), `mention` (completing `@` with the checkout's files), `shell` (what a `!` command sends the agent), `sidebar` (the sidebar's tree: each project's workspaces with the agent panes working in them, and which nodes are folded), `schedule` (commands and prompts a gateway runs on a cron pattern: when they are due, what is wrong with them, and their recent runs; shared by the gateway and the app), and `knowledge` (validating notes, shared likewise).
- `crates/ukai-voice` — voice input: microphone capture (`cpal`) and on-device transcription with whisper.cpp (`whisper-rs`). Nothing leaves the machine; do not add a cloud speech API. `audio` holds the pure conversions (downmix, resample, how the transcript joins the draft) and is where tests go; `cargo run -p ukai-voice --example transcribe -- <model> [file.wav]` checks a model without the app.
- `crates/ukai-app` — the Dioxus 0.7 desktop app (`ukai` binary). Keep logic out of components; put it in a library crate where it can be tested headless.
- `crates/ukai-relay` — runs a stdio ACP agent behind `/acp` with sessions that outlive client connections: answers reconnects' `initialize` and `session/load` from what it recorded, keeps turns running with no client, and re-sends pending permission requests. The protocol side (`hub`) is pure; extend its tests for behavior changes. `reports` serves the reports of the checkout it works in read-only, since the agent writes them there and no client can read them; `ports` likewise lists the pod's listening ports and forwards a WebSocket to one, and `checkout` lists the checkout's files and runs the user's commands in it. `mcp` is the MCP server (`ukai-relay mcp`) the hub adds to every session's `mcpServers`, whose `instructions` tell the agent about ukai — reports, previews of the servers it runs (`open_preview`), and knowledge in a gateway's pod; extend it when the agent should know about another feature. `init` (Linux) is what it does as a pod's PID 1: run itself again as its child, forward signals, and reap orphans; its tests are in `tests/init.rs`, a binary of their own, since it reaps every child of the process.
- `crates/ukai-gateway` — serves the agents in a `gateway.toml` at `/agents/<id>/acp` (WebSocket) from one namespace, starting a pod per workspace and relaying to the `ukai-relay` in it; `/agents/<id>/reports` proxies that workspace's reports without ever starting a pod, answering from the copy `archive` keeps in an S3 bucket once the pod is gone, and `/agents/<id>/forward/<port>` tunnels to its ports likewise. The rest is gRPC and gRPC-Web on the same port (`ukai.gateway.v1`, in `api`): `Agents` lists the agents and restarts a workspace's pod, `Ports` lists what listens in it, `Files` and `Commands` list its checkout's files and run the user's commands there (through the relay), and `Schedules` keeps cron schedules (`schedules`: stored as ConfigMaps, fired by the gateway, commands run through `pods/exec`, prompts as an ACP session on the relay); `knowledge` serves `ukai.gateway.v1.Knowledge` (gRPC and gRPC-Web on the same port) to clients and `WorkspaceKnowledge` to pods, answering each by asking `ukai-knowledge` on behalf of whoever it finds is asking. `config` is the `gateway.toml` schema and its validation; `job` builds the pod's Job spec (pure, tested as JSON); `scripts/workspace.sh` is the sidecar that clones and snapshots, the only container holding git credentials.
- `crates/ukai-knowledge` — the service keeping the notes (`ukai-knowledge` binary): `store` keeps them in SurrealDB (its tests run on the SDK's in-memory engine, not a server), and `service` answers `ukai.knowledge.v1.KnowledgeStore` to the gateway alone, trusting the caller it names; extend its tests (in-process, no server) for behavior changes.
- `crates/ukai-proto` — the `.proto` files for gRPC between ukai's parts and the code generated from them, nothing else; `protox` compiles them, so no `protoc` is needed. Conversions to ukai's types live with those types (`ukai_acp::knowledge::wire`).
- `deploy/gateway` — kustomize base for the gateway (with ukai-knowledge and SurrealDB for knowledge, and RustFS for the reports copy); environment-specific overlays stay out of the repository. `images/runtime` builds the gateway, knowledge service, and relay (published to `ghcr.io/illumination-k/ukai-runtime` from main), `images/dev` an agent image for working on this repository.
- `ukai.toml` — this repository's own config, and the reference example of the format. `ukai-config`'s tests parse it, so keep it valid.

## Development Process

Run `mise install` first. All tools and tasks live in the single `mise.toml`.
On Debian/Ubuntu, `mise bootstrap packages apply` also installs the system
libraries the webview, the microphone, and whisper.cpp need (`[bootstrap.packages]`).

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

`mise run e2e:gateway` deploys `deploy/gateway` into a kind cluster and checks
it from outside (`e2e/gateway/run.sh`, with the tests in
`crates/ukai-gateway/tests/e2e.rs`, ignored otherwise). It needs Linux, Docker,
and the runtime image built as `ukai-runtime:e2e` (see the script); CI runs it
on every change that reaches the cluster side. Extend it when a change crosses
the gateway, `ukai-knowledge`, or the manifests.

Session tests in `crates/ukai-acp/src/session/tests.rs` run against an
in-process mock agent over `Channel::duplex`; extend the mock rather than
reaching for a real agent in tests.

The `dx` CLI version in `mise.toml` must match the `dioxus` crate version in
`Cargo.toml`; bump them together.

Use [`rstest`](https://docs.rs/rstest) for parameterized cases. Validation and
interpolation code collects every problem rather than stopping at the first;
keep it that way and test each problem variant.

Logs go through `tracing` to stderr. Never log header values or anything read
through `${env:...}` or `${keychain:...}`: those are credentials.

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
- Secrets are only ever referenced as `${env:NAME}` or `${keychain:NAME}`
  (service `ukai`, account `NAME`); do not add a config key that stores one
  inline.
