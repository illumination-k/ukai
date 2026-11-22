---
name: run-ukai-app
description: Build and run the ukai desktop app (crates/ukai-app) headless on Linux under Xvfb, then click, press keys, and take screenshots to check a UI change in the real app. Use when asked to run, launch, screenshot, or manually check the app, or to confirm a UI change works beyond the tests.
---

# Running the ukai app for a manual check

The app is a Dioxus desktop app in a WebKitGTK web view. Built with plain
`cargo build` it runs unstyled, because `asset!` files (`main.css`, xterm.js)
are only bundled by `dx`. So build with `dx build` and run the binary from
its bundle directory. `ui.sh` (next to this file) does all of this.

## One-time setup (Debian/Ubuntu, e.g. a cloud session)

```bash
mise install                        # pinned tools, including dx (cargo:dioxus-cli)
mise bootstrap packages apply       # webkit2gtk, gtk, alsa, cmake, clang, ... (needs root)
apt-get install -y xvfb xdotool x11-utils imagemagick   # display, input, xdpyinfo, screenshots
```

Disk: a cloud session has a fixed allowance, and the default debug profile
(incremental, full debuginfo) plus dx's own `desktop-dev` profile can fill
it, which shows up as linker `Bus error`s or `No space left on device`.
`ui.sh` builds like CI does (`CARGO_INCREMENTAL=0`,
`CARGO_PROFILE_DEV_DEBUG=line-tables-only`); use the same for `mise run ci`.
If the disk is already full, `rm -rf target/debug` and build again.

## Running and driving it

```bash
U=.claude/skills/run-ukai-app/ui.sh
export UKAI_UI_DIR=<scratchpad>/ui   # where logs, PIDs, state and config go (default $TMPDIR/ukai-ui)

$U start               # dx build if crates/ changed, Xvfb on :99, the app on this repo
$U shot <dir>/a.png    # screenshot (1440×900), then Read the PNG to look at it
$U key ctrl+b          # keys, as xdotool names them: ctrl+shift+b, Escape, Return
$U click 278 19        # left-click at screen coordinates (read them off a screenshot)
$U log                 # the app's stderr
$U stop                # stops the app and Xvfb by PID
```

- `start` takes a project directory (default: this repository, whose
  `ukai.toml` lists claude/cloud/codex/gemini, so the empty pane shows them).
- State and config go to fresh directories under `$UKAI_UI_DIR`, never the
  real ones; `cat "$(cat $UKAI_UI_DIR/state-dir)/state.json"` shows what the
  app saved (written ~0.5 s after a change).
- The first `dx build` takes about 6 minutes; later ones only rebuild what
  changed.
- Combine several screenshots into one image to look at them together:
  `convert a.png b.png -resize 50% -append both.png`.

## Pitfalls

- There is no window manager under Xvfb: `ui.sh` sizes, places, and focuses
  the window itself. Do not use `xdotool windowactivate --sync`; it waits
  forever without one.
- App-wide shortcuts reach the app once its window has focus; if a key does
  nothing, click an empty part of a pane first.
- The layout moves: with a workspace opened the sidebar is 520 px wide
  rather than 260, which shifts the tab bar. Take a screenshot before
  clicking by coordinates.
- `libEGL warning: DRI3 error` in the log is harmless under Xvfb.
- Hovering leaves a tooltip on screen that may be stale in the next shot.
- Agents need their own CLIs and credentials (`npx`, API keys); the UI
  around them can be checked without starting one.
