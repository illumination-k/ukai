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
the gateway, `ukai-store`, or the manifests.

`mise run bench` runs the benchmarks (`divan`, in each crate's `benches/`;
not part of `ci`). `crates/ukai-app/benches/panes.rs` measures splitting,
moving, and resizing panes that show long transcripts, and switching tabs:
each should stay in microseconds whatever the transcripts' length, since
panes are placed rather than remounted (`layout::tests` checks the same
without timing), and each reports what it allocates. `benches/memory.rs`
reports the heap the virtual DOM keeps for mounted tabs (`alloc` less
`dealloc`): the tabs shown last (`KEPT_TABS`) stay mounted, so it grows
with their transcripts, not just the shown one's. Run both before and
after a change to how panes or messages render.

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
using. On Linux without a display (a cloud session), the `run-ukai-app`
skill (`.claude/skills/run-ukai-app`) sets up Xvfb, builds with `dx` so the
CSS is bundled, and drives the app with keys, clicks, and screenshots.

## Security notes

- Agent-produced HTML is untrusted. It renders in a child web view of its own
  with no IPC handler; never in an iframe of the app's web view, whose IPC
  bridge every frame can reach (see "HTML レポートと画像" in `docs/design.md`).
- ACP `fs/*` requests are confined to the project root after path
  normalization, including symlinks.
- Secrets are only ever referenced as `${env:NAME}` or `${keychain:NAME}`
  (service `ukai`, account `NAME`); do not add a config key that stores one
  inline.
