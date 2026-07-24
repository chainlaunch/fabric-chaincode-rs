#!/usr/bin/env bash
# Deploys the official Go asset-transfer-basic chaincode, the official
# TypeScript asset-transfer-basic chaincode, and this repo's Rust
# asset-transfer example side by side on a real (vanilla, no ChainLaunch)
# Fabric network, then times invoke/query latency for all three with
# scripts/benchmark.py. See docs/verification.md for what this does and
# does not measure, and results from the last run.
#
# Run from the repo root: ./scripts/benchmark.sh [num-calls]
# Requires: docker, git, python3, jq.
set -euo pipefail

FABRIC_VERSION="${FABRIC_VERSION:-3.1.5}"
FABRIC_CA_VERSION="${FABRIC_CA_VERSION:-1.5.17}"
FABRIC_SAMPLES_REF="${FABRIC_SAMPLES_REF:-65592350d7d7c51b02c8a4d89383d4bbdcc45725}"
ORDERER_OPERATIONS_HOST_PORT="${ORDERER_OPERATIONS_HOST_PORT:-9443}"
NUM_CALLS="${1:-30}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK_DIR="$(mktemp -d -t fabric-benchmark.XXXXXX)"
trap 'cleanup' EXIT

cleanup() {
  echo "--- cleanup ---"
  docker rm -f basic-go-cc basic-rust-cc basic-ts-cc >/dev/null 2>&1 || true
  if [ -d "$WORK_DIR/fabric-samples/test-network" ]; then
    (cd "$WORK_DIR/fabric-samples/test-network" && PATH="$WORK_DIR/fabric-samples/bin:$PATH" ./network.sh down >/dev/null 2>&1) || true
  fi
  rm -rf "$WORK_DIR"
}

echo "--- fetching Fabric ${FABRIC_VERSION} binaries + images, and fabric-samples@${FABRIC_SAMPLES_REF} ---"
cd "$WORK_DIR"
git clone --quiet https://github.com/hyperledger/fabric-samples.git
git -C fabric-samples checkout --quiet "$FABRIC_SAMPLES_REF"
curl -sSL https://raw.githubusercontent.com/hyperledger/fabric/main/scripts/install-fabric.sh -o install-fabric.sh
chmod +x install-fabric.sh
./install-fabric.sh --fabric-version "$FABRIC_VERSION" --ca-version "$FABRIC_CA_VERSION" docker binary

export PATH="$WORK_DIR/fabric-samples/bin:$PATH"

if [ "$ORDERER_OPERATIONS_HOST_PORT" != "9443" ]; then
  echo "--- remapping orderer operations port 9443 -> ${ORDERER_OPERATIONS_HOST_PORT} ---"
  sed -i.bak "s/- 9443:9443/- ${ORDERER_OPERATIONS_HOST_PORT}:9443/" \
    "$WORK_DIR/fabric-samples/test-network/compose/compose-test-net.yaml"
fi

echo "--- building chaincode images ---"
docker build -t basic-go-bench "$WORK_DIR/fabric-samples/asset-transfer-basic/chaincode-external"
docker build -f "$REPO_ROOT/examples/asset-transfer/Dockerfile" -t basic-rust-bench "$REPO_ROOT"
# --platform linux/amd64 throughout: the sample's Dockerfile hardcodes an
# amd64 tini binary regardless of build host, which breaks under Rosetta on
# Apple Silicon if the base layer runs arm64 natively (mixed-arch tini
# fails with "rosetta error: failed to open elf"). Forcing amd64 for both
# build and run keeps every layer consistent.
docker build --platform linux/amd64 --build-arg CC_SERVER_PORT=9999 \
  -t basic-ts-bench "$WORK_DIR/fabric-samples/asset-transfer-basic/chaincode-typescript"

echo
echo "--- image sizes ---"
for img in basic-go-bench basic-rust-bench basic-ts-bench; do
  docker images "$img" --format "{{.Repository}}: {{.Size}}"
done
echo

echo "--- starting test network (Fabric ${FABRIC_VERSION}) ---"
cd "$WORK_DIR/fabric-samples/test-network"
./network.sh up createChannel -c mychannel -i "$FABRIC_VERSION" -ca

ORDERER_CA="${PWD}/organizations/ordererOrganizations/example.com/orderers/orderer.example.com/msp/tlscacerts/tlsca.example.com-cert.pem"
PEER0_ORG1_CA="${PWD}/organizations/peerOrganizations/org1.example.com/tlsca/tlsca.org1.example.com-cert.pem"
PEER0_ORG2_CA="${PWD}/organizations/peerOrganizations/org2.example.com/tlsca/tlsca.org2.example.com-cert.pem"
export FABRIC_CFG_PATH="${PWD}/../config"
export FABRIC_LOGGING_SPEC=warning

use_org1() {
  export CORE_PEER_TLS_ENABLED=true
  export CORE_PEER_LOCALMSPID=Org1MSP
  export CORE_PEER_TLS_ROOTCERT_FILE="$PEER0_ORG1_CA"
  export CORE_PEER_MSPCONFIGPATH="${PWD}/organizations/peerOrganizations/org1.example.com/users/Admin@org1.example.com/msp"
  export CORE_PEER_ADDRESS=localhost:7051
}
use_org2() {
  export CORE_PEER_TLS_ENABLED=true
  export CORE_PEER_LOCALMSPID=Org2MSP
  export CORE_PEER_TLS_ROOTCERT_FILE="$PEER0_ORG2_CA"
  export CORE_PEER_MSPCONFIGPATH="${PWD}/organizations/peerOrganizations/org2.example.com/users/Admin@org2.example.com/msp"
  export CORE_PEER_ADDRESS=localhost:9051
}

echo "--- packaging CCaaS chaincode definitions ---"
for cc_port in "basic-go:9999" "basic-rust:7052" "basic-ts:9999"; do
  cc="${cc_port%%:*}"; port="${cc_port##*:}"
  mkdir -p "$WORK_DIR/pkg/$cc"
  (cd "$WORK_DIR/pkg/$cc" &&
    echo "{\"type\":\"ccaas\",\"label\":\"${cc}_1.0\"}" > metadata.json &&
    echo "{\"address\":\"${cc}-cc:${port}\",\"dial_timeout\":\"10s\",\"tls_required\":false}" > connection.json &&
    tar czf code.tar.gz connection.json &&
    tar czf "../${cc}.tgz" metadata.json code.tar.gz)
done

echo "--- install / approve / commit on both orgs ---"
# Plain variables, not an associative array — this must run on macOS's
# bundled bash 3.2, which predates `declare -A` (bash 4+).
pkg_id_for() {
  case "$1" in
    basic-go) echo "$GO_PKG_ID" ;;
    basic-rust) echo "$RUST_PKG_ID" ;;
    basic-ts) echo "$TS_PKG_ID" ;;
  esac
}

use_org1
for cc in basic-go basic-rust basic-ts; do
  peer lifecycle chaincode install "$WORK_DIR/pkg/$cc.tgz" >/dev/null
done
INSTALLED_JSON=$(peer lifecycle chaincode queryinstalled --output json)
GO_PKG_ID=$(echo "$INSTALLED_JSON" | jq -r '.installed_chaincodes[] | select(.label=="basic-go_1.0") | .package_id')
RUST_PKG_ID=$(echo "$INSTALLED_JSON" | jq -r '.installed_chaincodes[] | select(.label=="basic-rust_1.0") | .package_id')
TS_PKG_ID=$(echo "$INSTALLED_JSON" | jq -r '.installed_chaincodes[] | select(.label=="basic-ts_1.0") | .package_id')
for cc in basic-go basic-rust basic-ts; do
  [ -n "$(pkg_id_for "$cc")" ] || { echo "FATAL: could not resolve $cc package ID"; exit 1; }
  echo "$cc package ID: $(pkg_id_for "$cc")"
done
use_org2
for cc in basic-go basic-rust basic-ts; do
  peer lifecycle chaincode install "$WORK_DIR/pkg/$cc.tgz" >/dev/null
done

for org in 1 2; do
  use_org${org}
  for cc in basic-go basic-rust basic-ts; do
    peer lifecycle chaincode approveformyorg -o localhost:7050 --ordererTLSHostnameOverride orderer.example.com --tls --cafile "$ORDERER_CA" \
      --channelID mychannel --name "$cc" --version 1.0 --package-id "$(pkg_id_for "$cc")" --sequence 1 >/dev/null
  done
done

use_org1
for cc in basic-go basic-rust basic-ts; do
  peer lifecycle chaincode commit -o localhost:7050 --ordererTLSHostnameOverride orderer.example.com --tls --cafile "$ORDERER_CA" \
    --channelID mychannel --name "$cc" --version 1.0 --sequence 1 \
    --peerAddresses localhost:7051 --tlsRootCertFiles "$PEER0_ORG1_CA" \
    --peerAddresses localhost:9051 --tlsRootCertFiles "$PEER0_ORG2_CA" >/dev/null
done

# Readiness time: elapsed from `docker run` until the container answers a
# real query successfully — a uniform proxy across three very different
# runtimes/log formats (Go's own log line, Node's fabric-chaincode-node
# banner, our tracing output all look nothing alike, but "peer can query
# it" means the same thing for all three: REGISTER/READY handshake done).
wait_ready() {
  local cc="$1" start elapsed out
  start=$(python3 -c 'import time; print(time.time())')
  for _ in $(seq 1 100); do
    # A clean success OR a "does not exist" rejection both prove the
    # chaincode is up and answering (REGISTER/READY handshake done) — a
    # connection-refused/timeout error is the only "not ready yet" case.
    # set +e around the probe: it's *expected* to fail repeatedly until the
    # container comes up, and `set -e` would otherwise kill the whole script
    # on the very first attempt.
    set +e
    out=$(peer chaincode query -C mychannel -n "$cc" -c '{"function":"ReadAsset","Args":["__readiness_probe__"]}' 2>&1)
    rc=$?
    set -e
    if [ "$rc" -eq 0 ] || echo "$out" | /usr/bin/grep -q "does not exist\|already exists\|status:500"; then
      break
    fi
    sleep 0.1
  done
  elapsed=$(python3 -c "import time; print(f'{time.time() - $start:.2f}')")
  echo "$cc ready in ${elapsed}s"
}

echo "--- starting chaincode containers (timing readiness) ---"
docker run -d --name basic-go-cc --network fabric_test \
  -e CHAINCODE_SERVER_ADDRESS=0.0.0.0:9999 -e CHAINCODE_ID="$GO_PKG_ID" -e CORE_CHAINCODE_ID_NAME="$GO_PKG_ID" \
  basic-go-bench >/dev/null
wait_ready basic-go

docker run -d --name basic-rust-cc --network fabric_test \
  -e CHAINCODE_ID="$RUST_PKG_ID" \
  basic-rust-bench >/dev/null
wait_ready basic-rust

docker run -d --name basic-ts-cc --network fabric_test --platform linux/amd64 \
  -e CHAINCODE_SERVER_ADDRESS=0.0.0.0:9999 -e CHAINCODE_ID="$TS_PKG_ID" \
  basic-ts-bench >/dev/null
wait_ready basic-ts
echo

echo "--- InitLedger on all three ---"
for cc in basic-go basic-rust basic-ts; do
  peer chaincode invoke -o localhost:7050 --ordererTLSHostnameOverride orderer.example.com --tls --cafile "$ORDERER_CA" -C mychannel -n "$cc" \
    --peerAddresses localhost:7051 --tlsRootCertFiles "$PEER0_ORG1_CA" \
    --peerAddresses localhost:9051 --tlsRootCertFiles "$PEER0_ORG2_CA" \
    -c '{"function":"InitLedger","Args":[]}' >/dev/null 2>&1
  sleep 1
done

echo "--- benchmarking, CLI-per-call ($NUM_CALLS calls per op per chaincode) ---"
python3 "$REPO_ROOT/scripts/benchmark.py" \
  --test-network-dir "$PWD" \
  --run-id "bench-$(date +%s 2>/dev/null || echo run)" \
  -n "$NUM_CALLS"

# --- Persistent-connection benchmark (fabric-gateway, connect once) ---
# The CLI-based run above pays a fresh TLS handshake + process spawn on
# every single call, which dominates its latency numbers (see
# docs/verification.md §5). This second pass connects once via the Rust
# fabric-gateway client (sibling repo, ../fabric-gateway-rs) and reuses that
# connection for every call, isolating peer+chaincode latency from CLI/TLS
# setup cost -- and adds a genuine concurrent-throughput measurement the
# CLI-per-call approach cannot produce at all (one CLI process can only
# have one call in flight).
echo
echo "--- building gateway-bench (release) ---"
(cd "$REPO_ROOT/scripts/gateway-bench" && cargo build --release --quiet)

use_org1
ADMIN_CERT=$(ls "$CORE_PEER_MSPCONFIGPATH"/signcerts/*.pem | head -1)
ADMIN_KEY=$(ls "$CORE_PEER_MSPCONFIGPATH"/keystore/*_sk | head -1)

echo "--- benchmarking, persistent connection (fabric-gateway) ---"
"$REPO_ROOT/scripts/gateway-bench/target/release/gateway-bench" \
  --endpoint localhost:7051 \
  --override peer0.org1.example.com \
  --tls-ca "$PEER0_ORG1_CA" \
  --msp Org1MSP \
  --cert "$ADMIN_CERT" \
  --key "$ADMIN_KEY" \
  --channel mychannel \
  --chaincodes basic-go,basic-rust,basic-ts \
  --num-calls "$NUM_CALLS" \
  --concurrency 20 \
  --run-id "gwbench-$(date +%s 2>/dev/null || echo run)"
