#!/usr/bin/env bash
set -euo pipefail

# Fast local republish script for River UI iteration.
# Usage: ./scripts/local-republish.sh [--port PORT] [--skip-build]
#
# Prerequisites (one-time):
#   cargo build --release -p web-container-tool
#   cargo build --release --target wasm32-unknown-unknown -p web-container-contract
#   mkdir -p test-contract
#   target/release/web-container-tool generate --output test-contract/test-keys.toml
#
# Start test node (one-time):
#   freenet network --network-port 31338 --ws-api-port 7510 \
#     --ws-api-address 0.0.0.0 --is-gateway --skip-load-from-network \
#     --id test-node --public-network-address 127.0.0.1

cd "$(dirname "$0")/.."

PORT=7510
SKIP_BUILD=false

while [[ $# -gt 0 ]]; do
    case $1 in
        --port) PORT="$2"; shift 2 ;;
        --skip-build) SKIP_BUILD=true; shift ;;
        *) echo "Unknown arg: $1"; exit 1 ;;
    esac
done

# Verify prerequisites
if [ ! -f "test-contract/test-keys.toml" ]; then
    echo "ERROR: No test keys. Run: target/release/web-container-tool generate --output test-contract/test-keys.toml"
    exit 1
fi

if [ ! -f "target/wasm32-unknown-unknown/release/web_container_contract.wasm" ]; then
    echo "ERROR: No web container WASM. Run: cargo build --release --target wasm32-unknown-unknown -p web-container-contract"
    exit 1
fi

# Compute test contract ID early (needed for base_path)
# We need parameters to exist first — if they don't, do a preliminary sign to generate them
if [ ! -f "target/webapp/webapp-test.parameters" ]; then
    # Need at least a dummy sign to get parameters file; will re-sign after build
    if [ -f "target/webapp/webapp.tar.xz" ]; then
        target/release/web-container-tool sign \
            --input target/webapp/webapp.tar.xz \
            --output target/webapp/webapp-test.metadata \
            --parameters target/webapp/webapp-test.parameters \
            --key-file test-contract/test-keys.toml \
            --version 1 2>/dev/null
    fi
fi

if [ -f "target/webapp/webapp-test.parameters" ]; then
    CONTRACT_ID=$(fdev get-contract-id \
        --code target/wasm32-unknown-unknown/release/web_container_contract.wasm \
        --parameters target/webapp/webapp-test.parameters 2>/dev/null || echo "")
elif [ -f "test-contract/contract-id.txt" ]; then
    CONTRACT_ID=$(cat test-contract/contract-id.txt)
else
    CONTRACT_ID=""
fi

# Step 1: Build UI (unless --skip-build)
if [ "$SKIP_BUILD" = false ]; then
    echo "==> Building UI..."

    # Set base_path for test contract so assets resolve correctly
    DIOXUS_TOML="ui/Dioxus.toml"
    PROD_ID="raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv"

    if [ -n "$CONTRACT_ID" ]; then
        BASE_PATH="v1/contract/web/${CONTRACT_ID}"
    else
        BASE_PATH="v1/contract/web/${PROD_ID}"
    fi

    # Uncomment and set base_path for this build
    # Handle both commented (#base_path = ...) and uncommented (base_path = ...) states
    if grep -q "^#base_path" "$DIOXUS_TOML"; then
        # Currently commented — uncomment and set test ID
        perl -pi -e "s|^#base_path\s*=\s*\".*\"|base_path = \"${BASE_PATH}\"|" "$DIOXUS_TOML"
    elif grep -q "^base_path" "$DIOXUS_TOML"; then
        # Currently uncommented — just update the value
        perl -pi -e "s|^base_path\s*=\s*\".*\"|base_path = \"${BASE_PATH}\"|" "$DIOXUS_TOML"
    fi

    # Build, then restore Dioxus.toml
    restore_dioxus() {
        # Re-comment with production ID
        perl -pi -e "s|^base_path\s*=\s*\".*\"|#base_path = \"v1/contract/web/${PROD_ID}\"|" "$DIOXUS_TOML"
    }
    trap restore_dioxus EXIT

    (cd ui && dx build --release)

    # Restore immediately (trap is backup)
    restore_dioxus
    trap - EXIT
fi

# Step 2: Compress
echo "==> Compressing webapp..."
mkdir -p target/webapp
(cd target/dx/river-ui/release/web/public && tar -cJf ../../../../../webapp/webapp.tar.xz *)

# Step 3: Sign with test keys
VERSION=$(( $(date +%s) / 60 ))
echo "==> Signing (version $VERSION)..."
target/release/web-container-tool sign \
    --input target/webapp/webapp.tar.xz \
    --output target/webapp/webapp-test.metadata \
    --parameters target/webapp/webapp-test.parameters \
    --key-file test-contract/test-keys.toml \
    --version "$VERSION"

# Step 4: Get contract ID (re-read after sign, in case first run)
CONTRACT_ID=$(fdev get-contract-id \
    --code target/wasm32-unknown-unknown/release/web_container_contract.wasm \
    --parameters target/webapp/webapp-test.parameters 2>/dev/null)

echo "$CONTRACT_ID" > test-contract/contract-id.txt

# Step 5: Publish
echo "==> Publishing to localhost:$PORT..."
fdev --port "$PORT" execute put \
    --code target/wasm32-unknown-unknown/release/web_container_contract.wasm \
    --parameters target/webapp/webapp-test.parameters \
    contract \
    --webapp-archive target/webapp/webapp.tar.xz \
    --webapp-metadata target/webapp/webapp-test.metadata

echo ""
echo "==> Published: $CONTRACT_ID"
echo "    Desktop: http://127.0.0.1:${PORT}/v1/contract/web/${CONTRACT_ID}/"
LAN_IP=$(ifconfig en0 2>/dev/null | grep "inet " | awk '{print $2}' || echo "YOUR_LAN_IP")
echo "    Phone:   http://${LAN_IP}:${PORT}/v1/contract/web/${CONTRACT_ID}/"
echo ""
echo "Hard-refresh browser to see changes (Cmd+Shift+R)"
