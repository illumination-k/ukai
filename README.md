# ukai

A desktop client for the [Agent Client Protocol (ACP)](https://agentclientprotocol.com)
that drives many agents side by side — cloud agents over HTTP/WebSocket and
local agents over stdio — with per-project pane layouts, commands, and
in-pane HTML reports.

Pre-alpha. See [`docs/design.md`](docs/design.md) for the design and roadmap.

## Development

```bash
mise install    # toolchain (Rust, dx, cargo-nextest, cargo-deny, ...)
mise run dev    # run the desktop app with hot reload
mise run ci     # lint + test
```

On Linux the webview needs system libraries; on Debian/Ubuntu,
`mise bootstrap packages apply` installs the ones `mise.toml` declares.

## Configuration

Open project folders from the sidebar (or `ukai <dir>`). Each project can
declare the agents a pane may open in a `ukai.toml` at its root; agents shared
by every project (such as cloud agents) go in `~/.config/ukai/config.toml`.
This repository's own [`ukai.toml`](ukai.toml) is the annotated example.

Tabs and splits are arranged in the app and saved, with the projects you
opened, in `~/.local/state/ukai/state.json`.
