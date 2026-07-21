# Deploying Rust Chaincode as a Service

*How-to guide — run a `fabric-shim` chaincode in production, with ChainLaunch
or on any Fabric 3.x network.*

A `fabric-shim` chaincode is always deployed **chaincode-as-a-service
(CCaaS)**: a long-running gRPC server that peers dial into. This guide covers
the container contract your image must honor, deployment through ChainLaunch,
deployment on plain Fabric, and operations (upgrades, logs, scaling,
troubleshooting).

## The container contract

Whatever runs your chaincode must satisfy three rules:

1. **Bind the address in `CHAINCODE_SERVER_ADDRESS`** (fallback
   `CORE_CHAINCODE_ADDRESS`; the shim defaults to `0.0.0.0:7052`). Bind
   `0.0.0.0`, not `localhost` — the peer connects from outside the container.
2. **Present the package ID from `CHAINCODE_ID`** (fallback
   `CORE_CHAINCODE_ID`). This must be the exact ID produced when the package
   was installed on the peer (`<label>:<sha256>`); a mismatch makes the peer
   reject the registration.
3. **Exit cleanly on SIGTERM.** `Server::start` handles this: in-flight
   transactions finish, then the process exits — which is what
   `docker stop` sends.

`Server::from_env()` reads all of these, so a correct `main` is just:

```rust
Server::from_env()?.start(MyContract).await
```

One server instance serves any number of peers concurrently; every peer that
has the chaincode installed opens its own connection.

### Address semantics: two addresses, two jobs

| Field | Who uses it | Meaning |
|---|---|---|
| `chaincode_address` | The **peer** | The endpoint written into the installed package's `connection.json`. The peer dials this. Its **port is published on the Docker host** at deploy time. |
| `listen_address` | The **container** | The port the chaincode binds *inside* the container (default `0.0.0.0:7052`). |

Pick `chaincode_address` by where your peers run:

| Peer runs as | `chaincode_address` |
|---|---|
| Docker container on the same host | `host.docker.internal:<port>` |
| Native service (systemd/launchd) on the same host | `localhost:<port>` |
| Another machine | `<host-ip-or-dns>:<port>` |

> ⚠️ `chaincode_address` is baked into the installed package. Changing it
> later requires a new install + approve + commit round (new package hash) —
> pick a stable, routable address up front.

## Deploying with ChainLaunch

ChainLaunch drives the whole lifecycle through its API (or web UI under
*Smart Contracts → Fabric*). The chaincode language is irrelevant to
ChainLaunch — it just runs your image.

### Full lifecycle

```bash
API=http://localhost:8100/api/v1
AUTH='-u admin:admin123'

# 1. Register a chaincode on a network
curl -s $AUTH -X POST $API/sc/fabric/chaincodes \
  -d '{"name":"my-chaincode","network_id":1}'                # → chaincode.id

# 2. Create a definition
curl -s $AUTH -X POST $API/sc/fabric/chaincodes/{ccId}/definitions -d '{
  "version": "1.0",
  "sequence": 1,
  "docker_image": "registry.example.com/my-chaincode:1.0",
  "endorsement_policy": "",
  "chaincode_address": "host.docker.internal:40002",
  "listen_address": "0.0.0.0:7052",
  "environment_variables": {"RUST_LOG": "info"}
}'                                                           # → definition.id

# 3. Install the CCaaS package on peers (builds connection.json for you)
curl -s $AUTH -X POST $API/sc/fabric/definitions/{defId}/install -d '{"peer_ids":[10,11]}'

# 4-5. Approve per org, then commit to the channel
curl -s $AUTH -X POST $API/sc/fabric/definitions/{defId}/approve -d '{"peer_id":10}'
curl -s $AUTH -X POST $API/sc/fabric/definitions/{defId}/commit  -d '{"peer_id":10}'

# 6. Launch the container
curl -s $AUTH -X POST $API/sc/fabric/definitions/{defId}/deploy \
  -d '{"environment_variables":{"RUST_LOG":"debug"}}'
```

Notes:

- **Endorsement policy**: empty string uses the channel default (typically
  `MAJORITY Endorsement`). To pin one, use Fabric policy syntax:
  `OR('Org1MSP.member','Org2MSP.member')`.
- **Deploy** pulls the image, publishes the `chaincode_address` port to the
  `listen_address` port, injects `CHAINCODE_ID`/`CHAINCODE_SERVER_ADDRESS`
  (plus your custom env vars), and starts the container with restart policy
  `unless-stopped`. Redeploying replaces the running container.
- **Invoke/query** need a `key_id` of a signing key in the peer's
  organization; the transient map is passed as base64-encoded values:

  ```bash
  curl -s $AUTH -X POST $API/sc/fabric/chaincodes/{ccId}/query -d '{
    "function": "ReadTransient", "args": [], "channel": "mychannel",
    "key_id": "2", "transient": {"asset_properties": "dG9wc2VjcmV0"}
  }'
  ```

### Upgrading to a new version

1. Push the new image tag.
2. Create a **new definition** with `sequence` incremented (and a new
   `version` string). Keep `chaincode_address` unchanged unless you must move
   it.
3. Run install → approve → commit for the new definition. (If the package
   bytes are identical — same label and address — install may report the
   package already exists on the peer; that is harmless.)
4. `deploy` the new definition; ChainLaunch replaces the old container.

### Operating endpoints

| Task | Endpoint |
|---|---|
| Container logs (SSE follow) | `GET /sc/fabric/definitions/{id}/logs?follow=true&tail=100` |
| Container status/ports | `GET /sc/fabric/definitions/{id}/docker-info` |
| Lifecycle history | `GET /sc/fabric/definitions/{id}/timeline` |
| Stop the container | `POST /sc/fabric/definitions/{id}/undeploy` |
| Contract metadata | `GET /sc/fabric/chaincodes/{id}/metadata` |

## Deploying on plain Fabric (no ChainLaunch)

On a vanilla Fabric 3.x network you package and run the chaincode yourself.

### 1. Build the CCaaS package

```bash
# connection.json — what the peer will dial
cat > connection.json <<EOF
{
  "address": "my-chaincode.example.com:7052",
  "dial_timeout": "10s",
  "tls_required": false
}
EOF

# metadata.json — type ccaas is what makes the peer treat this as external
cat > metadata.json <<EOF
{"type": "ccaas", "label": "my-chaincode_1.0"}
EOF

tar czf code.tar.gz connection.json
tar czf my-chaincode.tgz metadata.json code.tar.gz
```

### 2. Standard lifecycle with the peer CLI

```bash
peer lifecycle chaincode install my-chaincode.tgz
peer lifecycle chaincode queryinstalled     # note the package ID: my-chaincode_1.0:<hash>

peer lifecycle chaincode approveformyorg -C mychannel -n my-chaincode \
  -v 1.0 --sequence 1 --package-id "my-chaincode_1.0:<hash>" ...
peer lifecycle chaincode commit -C mychannel -n my-chaincode -v 1.0 --sequence 1 ...
```

### 3. Run the service

```bash
docker run -d --name my-chaincode \
  -e CHAINCODE_ID="my-chaincode_1.0:<hash>" \
  -e CHAINCODE_SERVER_ADDRESS="0.0.0.0:7052" \
  -p 7052:7052 --restart unless-stopped \
  my-chaincode:1.0
```

Or with Compose, alongside a peer on the same Docker network:

```yaml
services:
  my-chaincode:
    image: my-chaincode:1.0
    environment:
      CHAINCODE_ID: "my-chaincode_1.0:<hash>"
      CHAINCODE_SERVER_ADDRESS: "0.0.0.0:7052"
      RUST_LOG: info
    networks: [fabric]
    restart: unless-stopped
# connection.json address is then "my-chaincode:7052"
```

The same container works on Kubernetes: a Deployment (1 replica per package
ID) plus a Service whose DNS name is the `connection.json` address.

### TLS

The current release speaks **plaintext gRPC** between peer and chaincode
(`tls_required: false`), matching ChainLaunch's default CCaaS wiring. Run the
connection over a trusted network (same host / same overlay network). TLS
server mode via PEM-carrying environment variables is planned; track the
[spec](spec.md) §3.5.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| Peer logs `context deadline exceeded` dialing the chaincode | `chaincode_address` not reachable from the peer: wrong host (`localhost` vs `host.docker.internal`), unpublished port, or the published host port doesn't match `connection.json`. Compare `docker port <container>` with the address in the definition. |
| Chaincode logs show a connection that immediately closes, no `READY` | `CHAINCODE_ID` doesn't match the installed package ID (e.g. container started manually with a stale hash). Redeploy so the platform injects the current one. |
| `chaincode registration failed: ... not found` on invoke | Definition committed with a different sequence/version than approved, or container not running. Check the timeline endpoint and `docker ps`. |
| Invoke returns status 500 with your own message | That's your contract returning `Err(...)` — working as intended; read the message. |
| First invoke is slow, later ones fast | Normal: the peer establishes the gRPC session and registration handshake lazily on first use. |

A healthy startup, in the chaincode's own logs:

```
INFO fabric_shim::server: chaincode server listening chaincode_id=my-chaincode_1.0:ab12… addr=0.0.0.0:7052
INFO fabric_shim::server: peer connected peer=…
DEBUG fabric_shim::handler: sent REGISTER …
DEBUG fabric_shim::handler: REGISTERED received
DEBUG fabric_shim::handler: READY received
```
