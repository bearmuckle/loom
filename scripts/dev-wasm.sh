#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
BACKEND_BIND="${LOOM_BACKEND_BIND:-127.0.0.1:8765}"
BACKEND_URL="${LOOM_BACKEND_URL:-ws://${BACKEND_BIND}/ws}"
HEALTH_URL="${LOOM_HEALTH_URL:-http://${BACKEND_BIND}/health}"
FRONTEND_HOST="${LOOM_FRONTEND_HOST:-127.0.0.1}"
FRONTEND_PORT="${LOOM_FRONTEND_PORT:-8080}"
TOKEN="${LOOM_TOKEN:-loom-local-dev-token}"

if ! command -v curl >/dev/null 2>&1; then
    echo "error: curl is required to wait for the backend to become ready" >&2
    exit 1
fi
if ! command -v trunk >/dev/null 2>&1; then
    echo "error: trunk is required; install it with 'cargo install trunk'" >&2
    exit 1
fi
if ! rustup target list --installed | grep -qx wasm32-unknown-unknown; then
    echo "error: the wasm32-unknown-unknown target is required" >&2
    echo "       install it with 'rustup target add wasm32-unknown-unknown'" >&2
    exit 1
fi

urlencode() {
    local value="$1"
    value="${value//%/%25}"
    value="${value// /%20}"
    value="${value//#/%23}"
    value="${value//&/%26}"
    value="${value//+/%2B}"
    value="${value//\?/%3F}"
    value="${value//=/%3D}"
    printf '%s' "$value"
}

backend_pid=
frontend_pid=

cleanup() {
    trap - EXIT INT TERM
    if [[ -n "$frontend_pid" ]]; then
        kill "$frontend_pid" 2>/dev/null || true
    fi
    if [[ -n "$backend_pid" ]]; then
        kill "$backend_pid" 2>/dev/null || true
    fi
    wait "$frontend_pid" 2>/dev/null || true
    wait "$backend_pid" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

echo "Starting Loom backend on ${BACKEND_BIND}..."
cargo run --quiet -p loom-cli -- \
    --serve \
    --bind "$BACKEND_BIND" \
    --token "$TOKEN" &
backend_pid=$!

for _ in {1..300}; do
    if curl --silent --fail "$HEALTH_URL" >/dev/null; then
        break
    fi
    if ! kill -0 "$backend_pid" 2>/dev/null; then
        wait "$backend_pid"
        echo "error: Loom backend exited before becoming ready" >&2
        exit 1
    fi
    sleep 0.2
done

if ! curl --silent --fail "$HEALTH_URL" >/dev/null; then
    echo "error: Loom backend did not become ready at ${HEALTH_URL}" >&2
    exit 1
fi

REMOTE_QUERY="$(urlencode "$BACKEND_URL")"
TOKEN_QUERY="$(urlencode "$TOKEN")"
FRONTEND_URL="http://${FRONTEND_HOST}:${FRONTEND_PORT}/?remote=${REMOTE_QUERY}&token=${TOKEN_QUERY}"

echo "Starting Loom WASM frontend from crates/loom-ui..."
echo "Open ${FRONTEND_URL}"
echo "Local development only: the URL contains a bearer token. Keep both listeners on loopback."
(
    cd "$ROOT/crates/loom-ui"
    exec env RUSTC_BOOTSTRAP="${RUSTC_BOOTSTRAP:-1}" \
        trunk serve --address "$FRONTEND_HOST" --port "$FRONTEND_PORT"
) &
frontend_pid=$!

wait "$frontend_pid"
