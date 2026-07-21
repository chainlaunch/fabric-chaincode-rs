# Getting Started with fabric-chaincode-rust

*Tutorial — from zero to a Rust chaincode running on a Fabric 3.x network.*

By the end of this tutorial you will have written a small asset-management
chaincode in Rust, packaged it as a Docker image, and invoked it on a live
Fabric channel through ChainLaunch. It assumes you know what a Fabric peer,
channel, and chaincode are, and that you can read Rust; it does not assume
prior experience with `fabric-chaincode-go` or `fabric-chaincode-node`.

## How your chaincode runs

`fabric-shim` only supports **chaincode-as-a-service (CCaaS)**: your chaincode
is a standalone gRPC server, usually a Docker container. The peer connects to
*it* — you never embed the chaincode inside the peer. Two environment
variables wire it up, both injected automatically by ChainLaunch (and by
Fabric's `ccaas` builder in general):

| Variable | Meaning |
|---|---|
| `CHAINCODE_ID` | The installed package ID, e.g. `basic_1.0:cafe...` — your server presents this when a peer connects |
| `CHAINCODE_SERVER_ADDRESS` | The address to bind, e.g. `0.0.0.0:7052` |

## 1. Create the project

```bash
cargo new my-chaincode && cd my-chaincode
cargo add tokio --features macros,rt-multi-thread
cargo add serde --features derive
cargo add serde_json
```

`fabric-shim` is not on crates.io yet — depend on it by git (or path, if you
have this repo checked out):

```toml
# Cargo.toml
[dependencies]
fabric-shim = { git = "https://github.com/chainlaunch/fabric-chaincode-rust" }
# or: fabric-shim = { path = "../fabric-chaincode-rust/fabric-shim" }
```

> Building `fabric-shim` compiles protobuf definitions, which needs `protoc`
> with the well-known types on your PATH: `brew install protobuf` on macOS,
> `apt-get install protobuf-compiler libprotobuf-dev` on Debian/Ubuntu.

## 2. Write the contract

Replace `src/main.rs` with:

```rust
use fabric_shim::{contract, ChaincodeStub, DataType, Error, Server};
use serde::{Deserialize, Serialize};

#[derive(DataType, Serialize, Deserialize)]
struct Asset {
    #[serde(rename = "ID")]
    id: String,
    owner: String,
    value: u64,
}

#[derive(Default)]
struct AssetContract;

#[contract(name = "AssetContract")]
impl AssetContract {
    /// Exposed on the wire as "CreateAsset".
    #[transaction]
    async fn create_asset(
        &self,
        ctx: &ChaincodeStub,
        id: String,
        owner: String,
        value: u64,
    ) -> Result<(), Error> {
        if !ctx.get_state(&id).await?.is_empty() {
            return Err(Error::InvalidArgument(format!("asset {id} already exists")));
        }
        let asset = Asset { id: id.clone(), owner, value };
        ctx.put_state(&id, serde_json::to_vec(&asset).unwrap()).await
    }

    /// Read-only: tagged "evaluate" so gateways don't send it for ordering.
    #[transaction(evaluate)]
    async fn read_asset(&self, ctx: &ChaincodeStub, id: String) -> Result<Asset, Error> {
        let bytes = ctx.get_state(&id).await?;
        if bytes.is_empty() {
            return Err(Error::InvalidArgument(format!("asset {id} does not exist")));
        }
        serde_json::from_slice(&bytes).map_err(|e| Error::InvalidArgument(e.to_string()))
    }

    #[transaction]
    async fn transfer_asset(
        &self,
        ctx: &ChaincodeStub,
        id: String,
        new_owner: String,
    ) -> Result<String, Error> {
        let mut asset = self.read_asset(ctx, id.clone()).await?;
        let old_owner = std::mem::replace(&mut asset.owner, new_owner);
        ctx.put_state(&id, serde_json::to_vec(&asset).unwrap()).await?;
        ctx.set_event("AssetTransferred", serde_json::to_vec(&asset).unwrap())?;
        Ok(old_owner)
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    Server::from_env()?.start(AssetContract).await
}
```

What the `#[contract]` macro did for you:

- **Routing.** `create_asset` is callable as `CreateAsset` (or
  `AssetContract:CreateAsset`). Unknown functions get a clean error.
- **Typed arguments.** Fabric delivers all arguments as bytes; the macro
  parses them into your parameter types (`u64` from `"300"`, structs from
  JSON) and rejects bad input with an error naming the parameter.
- **Return handling.** `Ok(Asset)` is serialized to JSON; `Ok(String)` is
  returned verbatim; `Ok(())` returns an empty payload; any `Err` becomes a
  status-500 response with the error message.
- **Metadata.** The chaincode answers `org.hyperledger.fabric:GetMetadata`
  with a contract-schema JSON document derived from your signatures — the
  same mechanism the Go and Node contract APIs provide, and what powers
  ChainLaunch's chaincode explorer.

Check it compiles and starts:

```bash
cargo build
CHAINCODE_ID=dev_1.0:0000 CHAINCODE_SERVER_ADDRESS=127.0.0.1:7052 cargo run
# INFO fabric_shim::server: chaincode server listening ...
```

Stop it with Ctrl-C. (Nothing will connect yet — a peer only dials in after
the chaincode is installed and committed.)

## 3. Package it as a Docker image

Create a `Dockerfile`:

```dockerfile
FROM rust:1-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler libprotobuf-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/my-chaincode /usr/local/bin/my-chaincode
ENV CHAINCODE_SERVER_ADDRESS=0.0.0.0:7052
EXPOSE 7052
ENTRYPOINT ["/usr/local/bin/my-chaincode"]
```

```bash
docker build -t my-chaincode:1.0 .
```

The final image is ~50 MB, runs as non-root, and has no shell.

## 4. Deploy it on a ChainLaunch Fabric network

You need a running ChainLaunch instance with a Fabric network (at least one
peer joined to a channel). The six lifecycle steps below use ChainLaunch's
standard chaincode API — set `API`, `AUTH`, and the IDs for your environment:

```bash
API=http://localhost:8100/api/v1
AUTH='-u admin:admin123'
NETWORK_ID=1   # your Fabric network
PEER_ID=10     # a peer joined to the channel
KEY_ID=2       # a signing key of the peer's organization

# 1. Register the chaincode
CC_ID=$(curl -s $AUTH -X POST $API/sc/fabric/chaincodes \
  -H 'Content-Type: application/json' \
  -d "{\"name\":\"my-chaincode\",\"network_id\":$NETWORK_ID}" | jq .chaincode.id)

# 2. Definition: which image to run, and where the peer will reach it
DEF_ID=$(curl -s $AUTH -X POST $API/sc/fabric/chaincodes/$CC_ID/definitions \
  -H 'Content-Type: application/json' \
  -d '{
    "version": "1.0", "sequence": 1,
    "docker_image": "my-chaincode:1.0",
    "endorsement_policy": "",
    "chaincode_address": "host.docker.internal:40002",
    "listen_address": "0.0.0.0:7052"
  }' | jq .definition.id)

# 3-5. Fabric lifecycle
curl -s $AUTH -X POST $API/sc/fabric/definitions/$DEF_ID/install -d "{\"peer_ids\":[$PEER_ID]}"
curl -s $AUTH -X POST $API/sc/fabric/definitions/$DEF_ID/approve -d "{\"peer_id\":$PEER_ID}"
curl -s $AUTH -X POST $API/sc/fabric/definitions/$DEF_ID/commit  -d "{\"peer_id\":$PEER_ID}"

# 6. Launch the container (ChainLaunch injects CHAINCODE_ID + CHAINCODE_SERVER_ADDRESS)
curl -s $AUTH -X POST $API/sc/fabric/definitions/$DEF_ID/deploy \
  -d '{"environment_variables":{"RUST_LOG":"info"}}'
```

Two values deserve attention (see the [deployment guide](deployment.md) for
the full story):

- `chaincode_address` is what the **peer dials**. Use
  `host.docker.internal:<port>` when the peer runs in Docker, or
  `localhost:<port>` / a host IP when it runs as a native service. Its port is
  published on the Docker host.
- `endorsement_policy` may be left empty to use the channel default, or set
  explicitly, e.g. `OR('Org1MSP.member')`.

## 5. Invoke it

```bash
curl -s $AUTH -X POST $API/sc/fabric/chaincodes/$CC_ID/invoke \
  -H 'Content-Type: application/json' \
  -d "{\"function\":\"CreateAsset\",\"args\":[\"asset1\",\"alice\",\"300\"],
       \"channel\":\"<your-channel>\",\"key_id\":\"$KEY_ID\"}"

curl -s $AUTH -X POST $API/sc/fabric/chaincodes/$CC_ID/query \
  -H 'Content-Type: application/json' \
  -d "{\"function\":\"ReadAsset\",\"args\":[\"asset1\"],
       \"channel\":\"<your-channel>\",\"key_id\":\"$KEY_ID\"}"
# → {"ID":"asset1","owner":"alice","value":300}
```

You can watch the handshake in the container logs (`GET
/sc/fabric/definitions/$DEF_ID/logs?follow=true`, or `docker logs`): a
successful start shows `peer connected`, `sent REGISTER`, `REGISTERED
received`, `READY received`.

Finally, ChainLaunch's metadata endpoint shows what your annotations
generated:

```bash
curl -s $AUTH $API/sc/fabric/chaincodes/$CC_ID/metadata | jq .result
```

## Where to go next

- [Deploying as a service](deployment.md) — address semantics, upgrades,
  plain-Fabric CCaaS without ChainLaunch, Compose, troubleshooting.
- [Migrating from Go](migrating-from-go.md) /
  [Migrating from TypeScript](migrating-from-typescript.md) — API mappings
  and semantic differences.
- [API reference](reference.md) — every stub method, macro option, and
  environment variable.
- `examples/asset-transfer/` in this repo — the full reference contract,
  byte-compatible with `fabric-samples/asset-transfer-basic`.
