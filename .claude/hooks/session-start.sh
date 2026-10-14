#!/bin/bash

# Runs at the start of each Claude Code session and prepares the toolchain
# declared in mise.toml. stdout is reserved for the hook protocol, so progress
# goes to stderr.

set -eu

cd "$(dirname "$0")/../.."

source .claude/hooks/common.sh

exec 3>&1 1>&2

if ! check_command mise; then
	curl https://mise.run | sh
	export PATH="$HOME/.local/bin:$PATH"
fi

mise trust --all
mise install

if [ -n "${CLAUDE_ENV_FILE:-}" ]; then
	DETECTED_SHELL=${CLAUDE_CODE_SHELL:-$(basename "${SHELL:-/bin/bash}")}
	case "$DETECTED_SHELL" in
	bash | zsh) ;;
	*)
		echo "Unsupported shell: $DETECTED_SHELL; falling back to bash."
		DETECTED_SHELL=bash
		;;
	esac

	# `mise env` (plain exports) rather than `mise activate` (prompt hooks), so
	# non-interactive Bash tool calls see the pinned tools on first source.
	{
		echo "export PATH=\"\$HOME/.local/bin:\$PATH\""
		mise env -s "$DETECTED_SHELL"
	} >"$CLAUDE_ENV_FILE"
else
	echo "CLAUDE_ENV_FILE is not set. Skipping shell environment setup."
fi
