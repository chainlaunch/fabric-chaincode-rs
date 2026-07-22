#!/usr/bin/env bash
# Differential test: deploys the official, unmodified Go asset-transfer-basic
# reference chaincode and this repo's Rust asset-transfer example side by
# side on a real (vanilla, no ChainLaunch) Fabric network, drives an
# identical sequence of invokes/queries against both, and asserts the JSON
# results are byte-for-byte identical. This is the strongest evidence this
# repo has that the shim is protocol-compatible with the reference
# implementation — see docs/verification.md.
#
# Run from the repo root: ./scripts/differential-test.sh
# Requires: docker, git, go (for building fabric-samples' chaincode-go image
# isn't actually needed — only docker build is used), python3, jq.
set -euo pipefail

FABRIC_VERSION="${FABRIC_VERSION:-3.1.5}"
FABRIC_CA_VERSION="${FABRIC_CA_VERSION:-1.5.17}"
# Pinned for reproducibility — fabric-samples has no version tags, only main.
FABRIC_SAMPLES_REF="${FABRIC_SAMPLES_REF:-65592350d7d7c51b02c8a4d89383d4bbdcc45725}"
# test-network hardcodes the orderer's operations/metrics listener at host
# port 9443. Override if that's already in use on your machine (CI runners
# never need this — only local development boxes with something else on
# 9443 do).
ORDERER_OPERATIONS_HOST_PORT="${ORDERER_OPERATIONS_HOST_PORT:-9443}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK_DIR="$(mktemp -d -t fabric-diff-test.XXXXXX)"
trap 'cleanup' EXIT

cleanup() {
  echo "--- cleanup ---"
  docker rm -f basic-go-cc basic-rust-cc >/dev/null 2>&1 || true
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
docker build -t basic-go-ccaas:diff "$WORK_DIR/fabric-samples/asset-transfer-basic/chaincode-external"
docker build -f "$REPO_ROOT/examples/asset-transfer/Dockerfile" -t basic-rust-ccaas:diff "$REPO_ROOT"

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
mkdir -p "$WORK_DIR/pkg/basic-go" "$WORK_DIR/pkg/basic-rust"
(cd "$WORK_DIR/pkg/basic-go" &&
  echo '{"type":"ccaas","label":"basic-go_1.0"}' > metadata.json &&
  echo '{"address":"basic-go-cc:9999","dial_timeout":"10s","tls_required":false}' > connection.json &&
  tar czf code.tar.gz connection.json &&
  tar czf ../basic-go.tgz metadata.json code.tar.gz)
(cd "$WORK_DIR/pkg/basic-rust" &&
  echo '{"type":"ccaas","label":"basic-rust_1.0"}' > metadata.json &&
  echo '{"address":"basic-rust-cc:7052","dial_timeout":"10s","tls_required":false}' > connection.json &&
  tar czf code.tar.gz connection.json &&
  tar czf ../basic-rust.tgz metadata.json code.tar.gz)

echo "--- install / approve / commit on both orgs ---"
use_org1
peer lifecycle chaincode install "$WORK_DIR/pkg/basic-go.tgz"
peer lifecycle chaincode install "$WORK_DIR/pkg/basic-rust.tgz"
# Package IDs are content-derived (same across orgs); query structured JSON
# rather than scrape log text, which is fragile against ANSI codes/format
# changes across Fabric versions.
GO_PKG_ID=$(peer lifecycle chaincode queryinstalled --output json | jq -r '.installed_chaincodes[] | select(.label=="basic-go_1.0") | .package_id')
RUST_PKG_ID=$(peer lifecycle chaincode queryinstalled --output json | jq -r '.installed_chaincodes[] | select(.label=="basic-rust_1.0") | .package_id')
[ -n "$GO_PKG_ID" ] || { echo "FATAL: could not resolve basic-go package ID"; exit 1; }
[ -n "$RUST_PKG_ID" ] || { echo "FATAL: could not resolve basic-rust package ID"; exit 1; }
echo "basic-go package ID:   $GO_PKG_ID"
echo "basic-rust package ID: $RUST_PKG_ID"
use_org2
peer lifecycle chaincode install "$WORK_DIR/pkg/basic-go.tgz" >/dev/null
peer lifecycle chaincode install "$WORK_DIR/pkg/basic-rust.tgz" >/dev/null

for org in 1 2; do
  use_org${org}
  for cc in basic-go basic-rust; do
    pkg_id_var="$([ "$cc" == "basic-go" ] && echo "$GO_PKG_ID" || echo "$RUST_PKG_ID")"
    peer lifecycle chaincode approveformyorg -o localhost:7050 --ordererTLSHostnameOverride orderer.example.com --tls --cafile "$ORDERER_CA" \
      --channelID mychannel --name "$cc" --version 1.0 --package-id "$pkg_id_var" --sequence 1 >/dev/null
  done
done

use_org1
for cc in basic-go basic-rust; do
  peer lifecycle chaincode commit -o localhost:7050 --ordererTLSHostnameOverride orderer.example.com --tls --cafile "$ORDERER_CA" \
    --channelID mychannel --name "$cc" --version 1.0 --sequence 1 \
    --peerAddresses localhost:7051 --tlsRootCertFiles "$PEER0_ORG1_CA" \
    --peerAddresses localhost:9051 --tlsRootCertFiles "$PEER0_ORG2_CA" >/dev/null
done

echo "--- starting chaincode containers ---"
docker run -d --name basic-go-cc --network fabric_test \
  -e CHAINCODE_SERVER_ADDRESS=0.0.0.0:9999 -e CHAINCODE_ID="$GO_PKG_ID" -e CORE_CHAINCODE_ID_NAME="$GO_PKG_ID" \
  basic-go-ccaas:diff
docker run -d --name basic-rust-cc --network fabric_test \
  -e CHAINCODE_ID="$RUST_PKG_ID" \
  basic-rust-ccaas:diff
sleep 3

query() {
  peer chaincode query -C mychannel -n "$1" -c "$2"
}
invoke() {
  local out
  if ! out=$(peer chaincode invoke -o localhost:7050 --ordererTLSHostnameOverride orderer.example.com --tls --cafile "$ORDERER_CA" -C mychannel -n "$1" \
    --peerAddresses localhost:7051 --tlsRootCertFiles "$PEER0_ORG1_CA" \
    --peerAddresses localhost:9051 --tlsRootCertFiles "$PEER0_ORG2_CA" -c "$2" 2>&1); then
    echo "invoke failed for $1 $2:"
    echo "$out"
    return 1
  fi
  sleep 1
}
assert_match() {
  if [ "$1" != "$2" ]; then
    echo "MISMATCH ($3):"
    echo "  go:   $1"
    echo "  rust: $2"
    exit 1
  fi
  echo "MATCH ($3)"
}

echo "--- running identical invoke/query sequence against both chaincodes ---"
invoke basic-go '{"function":"InitLedger","Args":[]}'
invoke basic-rust '{"function":"InitLedger","Args":[]}'

assert_match \
  "$(query basic-go '{"function":"ReadAsset","Args":["asset1"]}')" \
  "$(query basic-rust '{"function":"ReadAsset","Args":["asset1"]}')" \
  "InitLedger-seeded ReadAsset"

invoke basic-go '{"function":"CreateAsset","Args":["asset100","purple","20","carol","999"]}'
invoke basic-rust '{"function":"CreateAsset","Args":["asset100","purple","20","carol","999"]}'
assert_match \
  "$(query basic-go '{"function":"ReadAsset","Args":["asset100"]}')" \
  "$(query basic-rust '{"function":"ReadAsset","Args":["asset100"]}')" \
  "CreateAsset + ReadAsset"

invoke basic-go '{"function":"TransferAsset","Args":["asset100","bob"]}'
invoke basic-rust '{"function":"TransferAsset","Args":["asset100","bob"]}'
assert_match \
  "$(query basic-go '{"function":"ReadAsset","Args":["asset100"]}')" \
  "$(query basic-rust '{"function":"ReadAsset","Args":["asset100"]}')" \
  "post-TransferAsset ReadAsset"

assert_match \
  "$(query basic-go '{"function":"GetAllAssets","Args":[]}')" \
  "$(query basic-rust '{"function":"GetAllAssets","Args":[]}')" \
  "GetAllAssets"

set +e
query basic-go '{"function":"ReadAsset","Args":["nosuchasset"]}' >/dev/null 2>&1
GO_MISSING_EXIT=$?
query basic-rust '{"function":"ReadAsset","Args":["nosuchasset"]}' >/dev/null 2>&1
RUST_MISSING_EXIT=$?
set -e
if [ "$GO_MISSING_EXIT" == "0" ] || [ "$RUST_MISSING_EXIT" == "0" ]; then
  echo "MISMATCH (missing-key error path): go_exit=$GO_MISSING_EXIT rust_exit=$RUST_MISSING_EXIT (expected both nonzero)"
  exit 1
fi
echo "MATCH (missing-key error path: both reject, exit go=$GO_MISSING_EXIT rust=$RUST_MISSING_EXIT)"

echo
echo "ALL DIFFERENTIAL CHECKS PASSED — Rust shim matches the official Go reference chaincode"
