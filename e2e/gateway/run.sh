#!/usr/bin/env bash
# Runs deploy/gateway in a kind cluster and checks it from outside:
# crates/ukai-gateway/tests/e2e.rs through the gateway and the knowledge
# service, then that the NetworkPolicies let in only whom they should.
#
# Needs Linux (the image is linux/amd64), Docker, and the runtime image built
# as ukai-runtime:e2e, as CI does:
#   docker build -f images/runtime/Dockerfile --build-arg CARGO_PROFILE=dev -t ukai-runtime:e2e .
#   mise run e2e:gateway
# The cluster (UKAI_E2E_CLUSTER, default ukai-e2e) is created if missing and
# left running; `kind delete cluster --name ukai-e2e` removes it.
set -euo pipefail

cluster=${UKAI_E2E_CLUSTER:-ukai-e2e}
namespace=ukai-e2e
image=ukai-runtime:e2e
here=$(cd "$(dirname "$0")" && pwd)

k() { kubectl --context "kind-$cluster" -n "$namespace" "$@"; }

# What went wrong, without any secret: the pods, their events, and the logs
# of everything ukai runs.
diagnose() {
	echo "::group::cluster state" >&2
	k get pods -o wide >&2 || true
	k describe pods >&2 || true
	for app in deploy/ukai-gateway deploy/ukai-knowledge statefulset/ukai-surrealdb; do
		k logs "$app" --all-containers --tail=200 >&2 || true
	done
	echo "::endgroup::" >&2
}

forwards=()
cleanup() {
	status=$?
	if [ "$status" -ne 0 ]; then
		diagnose
	fi
	for pid in "${forwards[@]}"; do
		kill "$pid" 2>/dev/null || true
	done
	exit "$status"
}
trap cleanup EXIT

if ! kind get clusters | grep -qx "$cluster"; then
	kind create cluster --name "$cluster" --wait 2m
fi
kind load docker-image "$image" --name "$cluster"

kubectl --context "kind-$cluster" create namespace "$namespace" --dry-run=client -o yaml |
	kubectl --context "kind-$cluster" apply -f -
secret() {
	k create secret generic "$1" "${@:2}" --dry-run=client -o yaml | k apply -f - >/dev/null
}
gateway_token=$(openssl rand -hex 32)
knowledge_token=$(openssl rand -hex 32)
secret ukai-gateway --from-literal=token="$gateway_token"
secret ukai-knowledge --from-literal=token="$knowledge_token"
secret ukai-surrealdb --from-literal=user=root --from-literal=pass="$(openssl rand -hex 24)"
secret ukai-rustfs --from-literal=access-key=ukai --from-literal=secret-key="$(openssl rand -hex 24)"

k apply -k "$here"
# A rerun brings new tokens: start everything that read the old ones again.
k rollout restart deploy/ukai-gateway deploy/ukai-knowledge
for workload in statefulset/ukai-surrealdb statefulset/ukai-rustfs deploy/ukai-knowledge deploy/ukai-gateway; do
	k rollout status "$workload" --timeout=5m
done

# `port` local:remote of `target`, once it answers.
forward() {
	k port-forward "$1" "$2" >/dev/null &
	forwards+=("$!")
	local port=${2%%:*}
	for _ in $(seq 60); do
		if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
			return
		fi
		sleep 1
	done
	echo "port-forward to $1 never answered" >&2
	return 1
}
forward svc/ukai-gateway 18080:80
forward svc/ukai-knowledge 15051:50051

UKAI_E2E_GATEWAY=http://127.0.0.1:18080 \
	UKAI_E2E_GATEWAY_TOKEN=$gateway_token \
	UKAI_E2E_KNOWLEDGE=http://127.0.0.1:15051 \
	UKAI_E2E_KNOWLEDGE_TOKEN=$knowledge_token \
	cargo nextest run --locked -p ukai-gateway --test e2e --run-ignored only

# Whether a pod with `labels` reaches `service`:`port`; `expected` is
# `reaches` or `blocked`.
probe() {
	local name=$1 labels=$2 service=$3 port=$4 expected=$5 outcome
	if k run "$name" --image=busybox:1.37 --restart=Never --rm -i --quiet \
		--pod-running-timeout=2m ${labels:+--labels="$labels"} \
		--command -- nc -z -w 5 "$service" "$port" >/dev/null 2>&1; then
		outcome=reaches
	else
		outcome=blocked
	fi
	echo "$name: $service:$port $outcome (expected $expected)"
	[ "$outcome" = "$expected" ]
}
gateway=app.kubernetes.io/name=ukai-gateway
knowledge=app.kubernetes.io/name=ukai-knowledge
agent=ukai.dev/workspace=e2e,app.kubernetes.io/managed-by=ukai
probe as-gateway "$gateway" ukai-knowledge 50051 reaches
probe as-nobody "" ukai-knowledge 50051 blocked
probe as-agent "$agent" ukai-knowledge 50051 blocked
probe as-agent-named-gateway "$gateway,$agent" ukai-knowledge 50051 blocked
probe as-knowledge "$knowledge" ukai-surrealdb 8000 reaches
probe as-gateway-to-store "$gateway" ukai-surrealdb 8000 blocked
probe as-agent-named-knowledge "$knowledge,$agent" ukai-surrealdb 8000 blocked

# Whether the gateway refuses a pod with `labels` that asks what a person
# asks, even with the gateway's token: `refused` or `answered`.
asks_as_person() {
	local name=$1 labels=$2 expected=$3 outcome
	# shellcheck disable=SC2016 # $TOKEN is the pod's, expanded there.
	if k run "$name" --image=busybox:1.37 --restart=Never --rm -i --quiet \
		--pod-running-timeout=2m ${labels:+--labels="$labels"} --env="TOKEN=$gateway_token" \
		--command -- sh -c 'wget -q -S -O /dev/null --header "Authorization: Bearer $TOKEN" \
			http://ukai-gateway/agents/e2e/reports 2>&1 | grep -q " 403 "' >/dev/null 2>&1; then
		outcome=refused
	else
		outcome=answered
	fi
	echo "$name: the gateway $outcome it (expected $expected)"
	[ "$outcome" = "$expected" ]
}
asks_as_person person-pod "" answered
asks_as_person agent-pod-with-token "$agent" refused
