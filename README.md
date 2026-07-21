# fabric-chaincode-rust

A Rust chaincode shim for Hyperledger Fabric — the equivalent of
[`fabric-chaincode-go`](https://github.com/hyperledger/fabric-chaincode-go),
targeting **chaincode-as-a-service (CCaaS)** on **Fabric 3.x** peers. The
chaincode runs as a standalone gRPC server; the peer dials in and drives the
shim protocol. This is exactly how ChainLaunch deploys chaincode, so any
chaincode built with this shim is a drop-in Docker image for ChainLaunch's
`install → approve → commit → deploy` lifecycle.

## Documentation

| Doc | What it covers |
|---|---|
| [Getting started](docs/getting-started.md) | Tutorial: first chaincode → Docker image → deployed on ChainLaunch |
| [Deploying as a service](docs/deployment.md) | The CCaaS container contract, ChainLaunch lifecycle, plain-Fabric CCaaS, upgrades, troubleshooting |
| [Migrating from Go](docs/migrating-from-go.md) | `contractapi`/`shim` → Rust: API mapping, semantic differences, checklist |
| [Migrating from TypeScript](docs/migrating-from-typescript.md) | `fabric-contract-api` decorators → Rust: mapping, wire-name gotcha, checklist |
| [API reference](docs/reference.md) | Every stub method, macro option, type mapping, env var, and error |
| [Design spec](docs/spec.md) | Protocol design rationale, parity matrix, and milestone history |

## Layout

| Crate | Purpose |
|---|---|
| `fabric-shim-protos` | Bindings generated from vendored `hyperledger/fabric-protos` v0.3.7 (tonic/prost) |
| `fabric-shim` | The shim runtime: server, handshake state machine, `ChaincodeStub`, query iterators |
| `examples/asset-transfer` | Reference chaincode mirroring `fabric-samples/asset-transfer-basic`, JSON-compatible with the Go version |

## Writing a chaincode (contract API)

The `#[contract]` macro gives the same DX as `fabric-contract-api-go` /
fabric-chaincode-node decorators: annotate methods, get routing, typed
argument parsing, JSON returns, and `org.hyperledger.fabric:GetMetadata`
(contract metadata with JSON schemas) for free.

```rust
use fabric_shim::{contract, ChaincodeStub, DataType, Error, Server};
use serde::{Deserialize, Serialize};

#[derive(DataType, Serialize, Deserialize)]
struct Asset {
    #[serde(rename = "ID")]   // schema follows your serde attributes
    id: String,
    owner: String,
    size: u32,
}

#[derive(Default)]
struct AssetContract;

#[contract(name = "AssetContract")]
impl AssetContract {
    /// Exposed on the wire as "CreateAsset" (also "AssetContract:CreateAsset").
    #[transaction]
    async fn create_asset(
        &self,
        ctx: &ChaincodeStub,
        id: String,
        owner: String,
        size: u32,             // parsed from the string arg, error if invalid
    ) -> Result<(), Error> {
        let asset = Asset { id: id.clone(), owner, size };
        ctx.put_state(&id, serde_json::to_vec(&asset).unwrap()).await
    }

    #[transaction(evaluate)]   // read-only; tagged "evaluate" in metadata
    async fn read_asset(&self, ctx: &ChaincodeStub, id: String) -> Result<Asset, Error> {
        let bytes = ctx.get_state(&id).await?;
        serde_json::from_slice(&bytes).map_err(|e| Error::InvalidArgument(e.to_string()))
    }
}

#[tokio::main]
async fn main() -> Result<(), fabric_shim::Error> {
    // Reads CHAINCODE_ID and CHAINCODE_SERVER_ADDRESS (also the CORE_-prefixed
    // variants), which ChainLaunch injects at deploy time.
    Server::from_env()?.start(AssetContract).await
}
```

Transaction methods take `&self`, a ctx, then typed parameters, and return
`Result<T, E: Display>`. Method names become PascalCase wire names
(override with `#[transaction(name = "...")]`); parameter names become
camelCase in metadata. Supported types: `String`, integers/floats, `bool`,
`Vec<T>` (JSON), and any `#[derive(DataType)]` struct (JSON, schema
registered under `components.schemas`).

For full control you can instead implement the low-level `Chaincode` trait
(`invoke(&self, stub) -> Response`) directly — see `fabric-shim/src/lib.rs`.

Stub surface: state get/put/del, range queries (+ pagination, partial
composite keys), rich queries, history, private data (get/put/del/hash/purge),
transient map, creator identity, tx timestamp, chaincode events, and
cross-chaincode `invoke_chaincode`. See `fabric-shim/src/stub.rs`.

## Build & test

```bash
cargo test --workspace          # unit + mock-peer integration tests
cargo clippy --workspace --all-targets -- -D warnings
```

Codegen runs at build time and needs `protoc` with the well-known types
(macOS: `brew install protobuf`; Debian: `protobuf-compiler libprotobuf-dev`).

The integration tests (`fabric-shim/tests/mock_peer.rs`) run an in-process
mock peer — a real gRPC client, like the Fabric peer in CCaaS mode — covering
the handshake, ledger round trips, error surfacing, query paging, interleaved
transactions, panic isolation, keepalives, and proposal decoding.

## Deploying on ChainLaunch Pro

```bash
docker build -f examples/asset-transfer/Dockerfile -t chainlaunch/rust-cc-asset-transfer:0.1 .

NETWORK_ID=1 PEER_ID=10 KEY_ID=5 CHANNEL=mychannel ./examples/asset-transfer/e2e.sh
```

The e2e script drives ChainLaunch's standard chaincode endpoints
(`/sc/fabric/chaincodes`, `/sc/fabric/definitions/{id}/{install,approve,commit,deploy}`,
then `/invoke` and `/query`) — nothing Rust-specific is required server-side.

## Status

- [x] M1 protos + handshake, M2 stub surface, M3 example image
- [x] M4 interop sign-off on a live ChainLaunch Fabric network (spec §7.4 checklist)
- [x] High-level `#[contract]`/`#[derive(DataType)]` API with auto-generated `GetMetadata`
- [ ] M5 remaining: TLS via env-injected PEMs, state-based endorsement, load testing (see [spec](docs/spec.md) §6-8)
- [ ] Commit generated proto code so downstream builds don't need `protoc` ([spec](docs/spec.md) §3.6)
- [ ] Published to crates.io — currently a git/path dependency only
