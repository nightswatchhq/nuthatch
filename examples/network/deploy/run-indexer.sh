#!/bin/sh
# Operator-only launcher. Credentials and budget state stay outside the nest/NID.
set -eu

: "${NETWORK_STATE_DIR:?set NETWORK_STATE_DIR to the restricted operator directory}"
: "${NUTHATCH_BINARY:?set NUTHATCH_BINARY to the verified canonical-aware binary}"
test -x "$NUTHATCH_BINARY"
test -r "$NETWORK_STATE_DIR/rpc-url"
test -f "$NETWORK_STATE_DIR/budget.toml"
test -f "$NETWORK_STATE_DIR/budget.redb"
test -f "$NETWORK_STATE_DIR/budget.budget-initialized"
test -f "$NETWORK_STATE_DIR/nest/nuthatch.toml"

IFS= read -r network_rpc_url < "$NETWORK_STATE_DIR/rpc-url"
test -n "$network_rpc_url"
export NUTHATCH_RPC_BUDGET="$NETWORK_STATE_DIR/budget.toml"
# This validation listener is deliberately not reachable through the VPS proxy.
# No recent-history override: cumulative state starts at configured deployment.
exec "$NUTHATCH_BINARY" dev --dir "$NETWORK_STATE_DIR/nest" \
    --listen 127.0.0.1:8124 --rpc "$network_rpc_url" \
    --state-rpc "$network_rpc_url" --seal-direct --concurrency 1
