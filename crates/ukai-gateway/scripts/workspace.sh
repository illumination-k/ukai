#!/usr/bin/env bash
# The workspace sidecar of a ukai agent pod. It is the only container that may
# hold a git credential: it clones into /pristine (which the agent never
# mounts), copies the checkout to /workspace/repo for the agent, and pushes
# snapshots of the agent's working tree to $UKAI_WIP_REF.
#
# git never runs against the agent's own .git: a credential.helper or hook
# planted there would run with this container's token. Snapshots use the
# pristine .git with the agent's tree as the work tree, so only file contents
# cross over.
#
# Environment:
#   UKAI_GIT_URL            repository to clone (required)
#   UKAI_GIT_REF            branch to check out; the remote's default when empty
#   UKAI_WIP_REF            ref that snapshots are pushed to (required)
#   UKAI_WIP_URL            remote for snapshots; the clone's origin when empty
#   UKAI_SNAPSHOT_INTERVAL  seconds between periodic snapshots (default 300)
#   GH_TOKEN                optional; set up as git's credential through gh
#
# A snapshot is also taken soon after /workspace/.ukai/snapshot appears (the
# agent's relay writes it after each turn), and on TERM before exiting.
set -euo pipefail

: "${UKAI_GIT_URL:?}"
: "${UKAI_WIP_REF:?}"
interval=${UKAI_SNAPSHOT_INTERVAL:-300}
wip_remote=${UKAI_WIP_URL:-origin}

pristine=/pristine/repo
tree_dir=/workspace/repo
state=/workspace/.ukai

# HOME is private to this container, so the credential gh writes stays here.
export HOME=/tmp/home
mkdir -p "$HOME" "$state"
git config --global user.name ukai
git config --global user.email ukai@localhost
if [ -n "${GH_TOKEN:-}" ]; then
	gh auth setup-git
fi

g() {
	git --git-dir="$pristine/.git" --work-tree="$tree_dir" "$@"
}

# The latest snapshot, if any: restored into a fresh workspace, and remembered
# so an unchanged tree (after a restore or a restart of this container) is not
# pushed again.
fetch_snapshot() {
	git -C "$pristine" fetch --quiet "$wip_remote" "$UKAI_WIP_REF" 2>/dev/null &&
		git -C "$pristine" rev-parse FETCH_HEAD
}

restore() {
	local snap=$1 branch
	branch=$(git -C "$pristine" log -1 --format='%(trailers:key=Ukai-Branch,valueonly)' "$snap")
	branch=${branch:-$(git -C "$pristine" rev-parse --abbrev-ref HEAD)}
	echo "workspace: restoring $UKAI_WIP_REF ($snap) onto $branch"
	git -C "$pristine" checkout --quiet -B "$branch" "$snap^"
	# The snapshot's tree becomes uncommitted changes on top of its parent.
	git -C "$pristine" read-tree -u --reset "$snap"
	git -C "$pristine" reset --quiet
}

if [ ! -d "$pristine/.git" ]; then
	git clone --quiet ${UKAI_GIT_REF:+--branch "$UKAI_GIT_REF"} "$UKAI_GIT_URL" "$pristine"
fi
snap=$(fetch_snapshot || true)
if [ ! -e "$state/ready" ]; then
	[ -n "$snap" ] && restore "$snap"
	cp -a "$pristine" "$tree_dir"
	touch "$state/ready"
	echo "workspace: ready"
fi

last_tree=
[ -n "$snap" ] && last_tree=$(git -C "$pristine" rev-parse "$snap^{tree}")

# Every step returns on failure explicitly: `set -e` does not apply inside a
# function called from `||`, and a push that failed silently would be
# recorded as done and never retried.
snapshot() {
	local tree branch commit
	# A private index, so neither the pristine index nor the agent's is touched.
	export GIT_INDEX_FILE=/tmp/ukai-index
	rm -f "$GIT_INDEX_FILE"
	g read-tree HEAD || return 1
	g add -A || return 1
	tree=$(g write-tree) || return 1
	unset GIT_INDEX_FILE
	if [ "$tree" = "$(g rev-parse 'HEAD^{tree}')" ] || [ "$tree" = "$last_tree" ]; then
		return 0
	fi
	# The agent's branch, read as text: no git command runs on its .git.
	branch=$(sed -n 's|^ref: refs/heads/||p' "$tree_dir/.git/HEAD" 2>/dev/null || true)
	commit=$(g commit-tree "$tree" -p HEAD -m "ukai: snapshot" -m "Ukai-Branch: ${branch:-$(g rev-parse --abbrev-ref HEAD)}") || return 1
	g push --quiet --force "$wip_remote" "$commit:$UKAI_WIP_REF" || return 1
	last_tree=$tree
	echo "workspace: pushed $commit to $UKAI_WIP_REF"
}

sleeper=
on_term() {
	[ -n "$sleeper" ] && kill "$sleeper" 2>/dev/null
	snapshot || echo "workspace: final snapshot failed" >&2
	exit 0
}
trap on_term TERM

next=$(($(date +%s) + interval))
while :; do
	sleep 2 &
	sleeper=$!
	wait "$sleeper" || true
	now=$(date +%s)
	if [ -e "$state/snapshot" ] || [ "$now" -ge "$next" ]; then
		rm -f "$state/snapshot"
		snapshot || echo "workspace: snapshot failed" >&2
		next=$((now + interval))
	fi
done
