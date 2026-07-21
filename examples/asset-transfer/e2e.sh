#!/usr/bin/env bash
# End-to-end interop test against a local ChainLaunch Pro instance (spec §7.3).
#
# Prereqs:
#   - ChainLaunch Pro API running (default http://localhost:8100)
#   - A Fabric network with at least one peer joined to a channel
#   - The image built: docker build -f examples/asset-transfer/Dockerfile \
#         -t chainlaunch/rust-cc-asset-transfer:0.1 .
#
# Required env:
#   NETWORK_ID, PEER_ID, KEY_ID, CHANNEL
# Optional env:
#   CHAINLAUNCH_API (default http://localhost:8100/api/v1)
#   CHAINLAUNCH_AUTH (default admin:admin), CC_PORT (default 40002)
#   DOCKER_IMAGE (default chainlaunch/rust-cc-asset-transfer:0.1)
set -euo pipefail

API=${CHAINLAUNCH_API:-http://localhost:8100/api/v1}
AUTH=${CHAINLAUNCH_AUTH:-admin:admin}
CC_PORT=${CC_PORT:-40002}
DOCKER_IMAGE=${DOCKER_IMAGE:-chainlaunch/rust-cc-asset-transfer:0.1}
: "${NETWORK_ID:?set NETWORK_ID}" "${PEER_ID:?set PEER_ID}" "${KEY_ID:?set KEY_ID}" "${CHANNEL:?set CHANNEL}"

req() { # method path [json]
  local method=$1 path=$2 body=${3:-}
  curl -sS -u "$AUTH" -X "$method" "$API$path" \
    -H 'Content-Type: application/json' ${body:+-d "$body"}
}

step() { printf '\n== %s\n' "$*"; }

step "1/8 register chaincode"
CC_ID=$(req POST /sc/fabric/chaincodes \
  "{\"name\":\"asset-transfer-rust\",\"network_id\":$NETWORK_ID}" | jq -r .id)
echo "chaincode id: $CC_ID"

step "2/8 create definition (CCaaS -> $DOCKER_IMAGE)"
DEF_ID=$(req POST "/sc/fabric/chaincodes/$CC_ID/definitions" "{
  \"version\":\"1.0\",\"sequence\":1,
  \"docker_image\":\"$DOCKER_IMAGE\",
  \"endorsement_policy\":\"\",
  \"chaincode_address\":\"host.docker.internal:$CC_PORT\",
  \"listen_address\":\"0.0.0.0:7052\"
}" | jq -r .id)
echo "definition id: $DEF_ID"

step "3/8 install on peer $PEER_ID"
req POST "/sc/fabric/definitions/$DEF_ID/install" "{\"peer_ids\":[$PEER_ID]}" | jq -c .

step "4/8 approve"
req POST "/sc/fabric/definitions/$DEF_ID/approve" "{\"peer_id\":$PEER_ID}" | jq -c .

step "5/8 commit"
req POST "/sc/fabric/definitions/$DEF_ID/commit" "{\"peer_id\":$PEER_ID}" | jq -c .

step "6/8 deploy container"
req POST "/sc/fabric/definitions/$DEF_ID/deploy" '{"environment_variables":{"RUST_LOG":"debug"}}' | jq -c .
sleep 3

step "7/8 invoke CreateAsset + TransferAsset"
req POST "/sc/fabric/chaincodes/$CC_ID/invoke" "{
  \"function\":\"CreateAsset\",\"args\":[\"asset1\",\"blue\",\"5\",\"alice\",\"100\"],
  \"channel\":\"$CHANNEL\",\"key_id\":$KEY_ID}" | jq -c .
req POST "/sc/fabric/chaincodes/$CC_ID/invoke" "{
  \"function\":\"TransferAsset\",\"args\":[\"asset1\",\"bob\"],
  \"channel\":\"$CHANNEL\",\"key_id\":$KEY_ID}" | jq -c .

step "8/8 query ReadAsset + GetAllAssets"
READ=$(req POST "/sc/fabric/chaincodes/$CC_ID/query" "{
  \"function\":\"ReadAsset\",\"args\":[\"asset1\"],
  \"channel\":\"$CHANNEL\",\"key_id\":$KEY_ID}")
echo "$READ" | jq -c .
echo "$READ" | grep -q '"owner":"bob"' || { echo "FAIL: expected owner bob"; exit 1; }
req POST "/sc/fabric/chaincodes/$CC_ID/query" "{
  \"function\":\"GetAllAssets\",\"args\":[],
  \"channel\":\"$CHANNEL\",\"key_id\":$KEY_ID}" | jq -c .

printf '\nPASS: rust chaincode interop OK (chaincode %s, definition %s)\n' "$CC_ID" "$DEF_ID"
