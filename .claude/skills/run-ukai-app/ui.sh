#!/bin/bash
# Run the ukai desktop app headless (Xvfb) with throwaway state and config,
# and drive it for a manual check. See SKILL.md.
#
#   ui.sh start [project-dir]   build with dx if needed, start Xvfb and the app
#   ui.sh shot <file.png>       screenshot the whole screen
#   ui.sh key <keys>...         send keys (xdotool names: ctrl+b, ctrl+shift+b, Escape)
#   ui.sh click <x> <y>         left-click at screen coordinates
#   ui.sh log                   the app's log so far
#   ui.sh stop                  stop the app and Xvfb (by PID)

set -eu

root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
run=${UKAI_UI_DIR:-${TMPDIR:-/tmp}/ukai-ui}
display=${UKAI_UI_DISPLAY:-:99}
app=$root/target/dx/ukai/debug/linux/app/ukai
export DISPLAY=$display

case "${1:-}" in
start)
	project=${2:-$root}
	mkdir -p "$run"
	if [ ! -x "$app" ] || [ -n "$(find "$root/crates" -newer "$app" -type f -print -quit)" ]; then
		# Like CI: no incremental cache and line tables only, or target/
		# outgrows the disk of a cloud session.
		(cd "$root" && GGML_NATIVE=OFF GGML_CCACHE=OFF CARGO_INCREMENTAL=0 \
			CARGO_PROFILE_DEV_DEBUG=line-tables-only \
			"$(mise where cargo:dioxus-cli)/bin/dx" build --package ukai-app --platform desktop)
	fi
	if ! xdpyinfo >/dev/null 2>&1; then
		setsid Xvfb "$display" -screen 0 1440x900x24 </dev/null >"$run/xvfb.log" 2>&1 &
		echo $! >"$run/xvfb.pid"
		for _ in $(seq 50); do
			if xdpyinfo >/dev/null 2>&1; then break; fi
			sleep 0.1
		done
	fi
	state=$(mktemp -d -p "$run" state.XXXX)
	config=$(mktemp -d -p "$run" config.XXXX)
	# dx's bundle finds its assets next to the binary: run it from there.
	UKAI_STATE_DIR=$state UKAI_CONFIG_DIR=$config setsid env -C "$(dirname "$app")" \
		"$app" "$project" </dev/null >"$run/app.log" 2>&1 &
	echo $! >"$run/app.pid"
	echo "$state" >"$run/state-dir"
	# Wait for the window, then make it fill the screen.
	window=
	for _ in $(seq 100); do
		window=$(xdotool search --onlyvisible --pid "$(cat "$run/app.pid")" 2>/dev/null | head -1)
		[ -n "$window" ] && break
		sleep 0.2
	done
	# No window manager under Xvfb: place and focus the window ourselves.
	[ -n "$window" ] && xdotool windowsize "$window" 1440 900 windowmove "$window" 0 0 windowfocus "$window"
	sleep 3
	echo "app pid $(cat "$run/app.pid"), state in $state, log in $run/app.log"
	;;
shot) import -window root "$2" ;;
key)
	shift
	xdotool key --clearmodifiers "$@"
	sleep 0.5
	;;
click)
	xdotool mousemove "$2" "$3" click 1
	sleep 0.5
	;;
log) cat "$run/app.log" ;;
stop)
	for pid in app xvfb; do
		[ -f "$run/$pid.pid" ] && kill "$(cat "$run/$pid.pid")" 2>/dev/null || true
		rm -f "$run/$pid.pid"
	done
	;;
*)
	sed -n '2,12p' "$0"
	exit 2
	;;
esac
