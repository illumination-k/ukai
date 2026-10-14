#!/usr/bin/env bash
# Start a virtual display ($DISPLAY, default :99) and a session bus for the
# desktop app, unless they are already running. Safe to call repeatedly.
#
#   start-display && cargo run -p ukai-app &
#   scrot /tmp/ukai.png
set -eu

display=${DISPLAY:-:99}
runtime=${XDG_RUNTIME_DIR:-/tmp/runtime-$(id -u)}
mkdir -p "$runtime"
chmod 700 "$runtime"

if ! xdpyinfo -display "$display" >/dev/null 2>&1; then
	Xvfb "$display" -screen 0 1600x1000x24 -nolisten tcp >/tmp/xvfb.log 2>&1 &
	for _ in $(seq 50); do
		xdpyinfo -display "$display" >/dev/null 2>&1 && break
		sleep 0.1
	done
fi

bus="$runtime/bus"
if [ ! -S "$bus" ]; then
	dbus-daemon --session --address="unix:path=$bus" --fork >/dev/null
fi

echo "export DISPLAY=$display XDG_RUNTIME_DIR=$runtime DBUS_SESSION_BUS_ADDRESS=unix:path=$bus"
