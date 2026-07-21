# Rust Chaincode Shim — Specification & ChainLaunch Pro Test Plan

*Explanation — the design rationale and milestone plan behind this crate.
Written while designing the shim against a private ChainLaunch Pro
deployment; kept here as the historical record of what was decided and why.
Line-number references below point into ChainLaunch's (private, closed
source) `pkg/chainlaunchdeploy/` package as it existed in 2026-07 — useful
context for maintainers, not something outside contributors can check out.*

**Status:** Implemented through M4; M5 in progress.
**Date:** 2026-07-20
**Crates:** `fabric-shim`, `fabric-shim-protos`, `fabric-shim-macros`

## 1. Summary

A Rust library equivalent to [`fabric-chaincode-go`](https://github.com/hyperledger/fabric-chaincode-go)'s shim, targeting **chaincode-as-a-service (CCaaS)** mode only. The chaincode runs as a standalone gRPC **server**; the Fabric peer dials it and drives the shim protocol over a bidirectional `ChaincodeMessage` stream.

This is fully feasible with no Fabric or ChainLaunch changes:

- The peer↔chaincode protocol is plain gRPC + protobuf (`hyperledger/fabric-protos`), implementable with `tonic` + `prost`.
- ChainLaunch Pro already deploys chaincode exclusively as CCaaS (`pkg/chainlaunchdeploy/service.go:860` builds a `metadata.json` with `"type": "ccaas"`) and launches the chaincode Docker container itself, injecting `CHAINCODE_ID` and `CHAINCODE_SERVER_ADDRESS` (`pkg/chainlaunchdeploy/fabric.go:498-520`). A Rust CCaaS server is a drop-in replacement for a Go/Node one — ChainLaunch neither knows nor cares what language is inside the image.

Prior art: `hyperledger-labs/fabric-chaincode-rust` exists but is stale/incomplete; we treat it as reference only, not a dependency.

## 2. Background: the protocol we must implement

In CCaaS mode the chaincode implements the `Chaincode` gRPC service from `fabric-protos` (`peer/chaincode_shim.proto`):

```proto
service Chaincode {
  rpc Connect(stream ChaincodeMessage) returns (stream ChaincodeMessage);
}
```

(The legacy mode, where chaincode dials the peer's `ChaincodeSupport.Register`, is **out of scope** — ChainLaunch never uses it.)

Once the peer opens the stream, the shim runs a state machine:

1. **Handshake:** shim sends `REGISTER` carrying `ChaincodeID{name: <CHAINCODE_ID env, i.e. the package ID>}` → peer replies `REGISTERED` → peer sends `READY`.
2. **Keepalive:** peer sends `KEEPALIVE`; shim echoes it back.
3. **Transactions:** peer sends `INIT` or `TRANSACTION` with a `ChaincodeInput` (args, decorations, is_init) plus `txid`, `channel_id`, and a `SignedProposal`. The shim dispatches to user code.
4. **Ledger callbacks:** while handling a transaction, the shim sends request messages (`GET_STATE`, `PUT_STATE`, `DEL_STATE`, `GET_STATE_BY_RANGE`, `GET_QUERY_RESULT`, `GET_HISTORY_FOR_KEY`, `QUERY_STATE_NEXT`, `QUERY_STATE_CLOSE`, `INVOKE_CHAINCODE`, `GET_PRIVATE_DATA_HASH`, `PURGE_PRIVATE_DATA`) tagged with the same `(channel_id, txid)`, and the peer replies with `RESPONSE` or `ERROR`.
5. **Completion:** shim sends `COMPLETED` with a serialized `Response{status, message, payload}` (200 = success, 500 = error). Chaincode events and metadata ride along on the completion message.

Multiple transactions run **concurrently on one stream**, correlated by `(channel_id, txid)` — the shim must multiplex.

## 3. Functional requirements

### 3.1 Transport & server (P0)

- gRPC server built on `tonic`, async runtime `tokio`.
- Bind address from `CHAINCODE_SERVER_ADDRESS` (fallback `CORE_CHAINCODE_ADDRESS`, default `0.0.0.0:7052`).
- Chaincode identity from `CHAINCODE_ID` (fallback `CORE_CHAINCODE_ID`). Both pairs are injected by ChainLaunch's deployer — honor both spellings.
- Support multiple concurrent peer connections (each peer that has the chaincode installed dials in), and concurrent transactions per connection.
- gRPC server options matching Fabric defaults: max message size ≥ 100 MB, keepalive tolerant of peer pings.
- Graceful shutdown on SIGTERM (docker stop): finish in-flight transactions with a short deadline, close streams.

### 3.2 Protocol state machine (P0)

- Full REGISTER → REGISTERED → READY handshake; reject transaction messages before READY.
- Per-transaction context keyed by `(channel_id, txid)`; route peer `RESPONSE`/`ERROR` messages to the awaiting stub call via oneshot channels.
- One in-flight ledger request per transaction at a time (matches Go shim semantics — stub calls are sequential within a tx).
- A panic/error in user code must produce a `COMPLETED` with status 500, never kill the stream or the process.

### 3.3 User-facing API (P0)

```rust
#[async_trait]
pub trait Chaincode: Send + Sync + 'static {
    async fn init(&self, stub: ChaincodeStub) -> Response;
    async fn invoke(&self, stub: ChaincodeStub) -> Response;
}

// main.rs of a chaincode:
#[tokio::main]
async fn main() -> Result<(), Error> {
    fabric_shim::Server::from_env()?      // reads CHAINCODE_ID / CHAINCODE_SERVER_ADDRESS
        .start(MyContract::default())
        .await
}
```

`Response` mirrors `peer.Response`: `Response::success(payload)`, `Response::error(msg)`.

> Implemented as a fast follow beyond this original design: the
> [`#[contract]` / `#[derive(DataType)]`](reference.md) high-level API, which
> generates the `Chaincode` impl from annotated methods instead of requiring
> hand-written `invoke` dispatch. §4 originally scoped this out as a P2
> follow-up; it shipped alongside M4.

### 3.4 `ChaincodeStub` parity matrix

P0 = required for v1 interop sign-off; P1 = fast follow; P2 = later.

| Capability | Methods | Priority | Status |
|---|---|---|---|
| Args & function | `get_function_and_args`, `get_args`, `get_string_args` | P0 | ✅ |
| Tx identity | `get_tx_id`, `get_channel_id`, `get_tx_timestamp` | P0 | ✅ |
| World state | `get_state`, `put_state`, `del_state` | P0 | ✅ |
| Range queries | `get_state_by_range` (+ iterator with `QUERY_STATE_NEXT`/`CLOSE` paging) | P0 | ✅ |
| Composite keys | `create_composite_key`, `split_composite_key`, `get_state_by_partial_composite_key` | P0 | ✅ |
| Events | `set_event` (single event, sent on COMPLETED) | P0 | ✅ |
| Creator / proposal | `get_creator` (msp id + cert), `get_signed_proposal`, `get_transient` | P0 | ✅ |
| Cross-chaincode | `invoke_chaincode` (`INVOKE_CHAINCODE`, nested response decode) | P1 | ✅ |
| Rich queries | `get_query_result` (CouchDB only — ChainLaunch peers default to LevelDB, so gate behind capability) | P1 | ✅ |
| History | `get_history_for_key` | P1 | ✅ |
| Private data | `get/put/del_private_data`, `get_private_data_hash`, `get_transient`-based flows, `purge_private_data` | P1 | ✅ |
| Pagination | `..._with_pagination` variants (bookmark + page size metadata) | P1 | ✅ (range only) |
| State-based endorsement | `set/get_state_validation_parameter` (key-level endorsement via `ValidationParameter` metadata) | P2 | ⬜ |
| Decorations, binding | `get_decorations`, `get_binding` | P2 | `get_decorations` ✅, `get_binding` ⬜ |

### 3.5 TLS (P1)

`connection.json` generated by ChainLaunch supports `tls_required` + `root_cert` + optional mTLS client cert/key (`service.go:742-784`), but ChainLaunch's deployer currently passes no cert material into the container except via user-supplied env vars. Therefore:

- **v1:** plaintext gRPC (`tls_required: false`) — matches ChainLaunch's default deploy path.
- **P1:** TLS server mode configured from env, PEM contents passed directly (not paths), since ChainLaunch injects env vars but mounts no volumes: `CHAINCODE_TLS_CERT_PEM`, `CHAINCODE_TLS_KEY_PEM`, `CHAINCODE_TLS_CLIENT_CA_PEM` (mTLS). Document how these pair with the definition's `chaincode_address`/TLS fields.

Status: not yet implemented — still open (see §8, item 2).

### 3.6 Proto generation

- Vendor protos from `hyperledger/fabric-protos` (pin a release tag compatible with Fabric 3.x; 2.x peers are explicitly unsupported), generate with `tonic-build`/`prost-build`.
- Required proto files: `peer/chaincode_shim.proto`, `peer/chaincode.proto`, `peer/chaincode_event.proto`, `peer/proposal.proto`, `peer/proposal_response.proto`, `common/common.proto`, `msp/identities.proto`, `ledger/queryresult/kv_query_result.proto`.
- **Status: done.** Generated code is committed under `fabric-shim-protos/src/generated/` and used by default — `cargo build` needs no `protoc`. Regeneration is opt-in via the `regenerate-protos` feature (requires `protoc` + the well-known types), used only when the vendored `.proto` files change; CI verifies both the default (no-`protoc`) build and the opt-in regeneration path independently.

### 3.7 Packaging & distribution

- Cargo workspace: `fabric-shim-protos` (generated types), `fabric-shim-macros` (proc macros), `fabric-shim` (runtime), `examples/asset-transfer` (reference chaincode).
- Reference `Dockerfile` (multi-stage: `rust:1-slim-bookworm` build → `gcr.io/distroless/cc` runtime) that any user chaincode can copy.
- The image must run as non-root and listen on `0.0.0.0:<port>` — the platform maps a host port to the container port and the peer dials that host port (`host.docker.internal:<hostPort>` when the peer runs in Docker).

## 4. Non-goals (v1)

- Legacy "chaincode dials peer" mode and peer-managed chaincode builds.
- ~~A high-level contract API~~ — shipped ahead of schedule as `#[contract]`/`#[derive(DataType)]`; see §3.3 note.
- FabricX — its committer/namespace model does not use this shim protocol.

## 5. Architecture sketch

```
fabric-shim
├── server.rs      tonic service impl: Chaincode::Connect(stream)
├── handler.rs     per-connection state machine (Created → Established → Ready)
│                    - inbound demux: KEEPALIVE echo, tx spawn, response routing
│                    - GetMetadata interception (org.hyperledger.fabric:GetMetadata)
│                    - outbound mpsc to the gRPC stream
├── stub.rs        ChaincodeStub: builds request ChaincodeMessages, awaits
│                    routed responses (oneshot per (channel_id, txid))
├── iterators.rs   StateQueryIterator / HistoryQueryIterator (QueryResponse paging)
├── response.rs    Response, error conversion, event handling
├── metadata.rs    Contract metadata document types (GetMetadata payload)
├── contract.rs    ContractSchema/ContractArg/ContractReturn traits (used by the macros)
└── (fabric-shim-protos)  prost/tonic generated (separate crate)

fabric-shim-macros
├── contract       #[contract] attribute: routing, arg parsing, metadata generation
└── DataType       #[derive(DataType)]: JSON schema from a struct + serde attrs
```

Each `TRANSACTION` spawns a tokio task running user `invoke()` with a stub bound to that tx's response channel; the connection handler owns a `HashMap<(String, String), oneshot::Sender<ChaincodeMessage>>` for routing. This mirrors the Go shim's `Handler.responseChannels` design.

## 6. Milestones

| # | Deliverable | Exit criterion | Status |
|---|---|---|---|
| M1 | Protos crate + handshake + KEEPALIVE | Mock-peer unit test completes REGISTER/READY | ✅ |
| M2 | P0 stub surface + COMPLETED flow | Unit tests green against mock peer | ✅ |
| M3 | `asset-transfer` example + Dockerfile | Container starts, listens, logs registration | ✅ |
| M4 | **ChainLaunch Pro interop** (§7) | Full checklist §7.4 passes on a local network | ✅ |
| M5 | P1 surface (invoke_chaincode, history, private data, pagination, TLS) | Interop checklist extended items pass | Mostly done; TLS still open |

## 7. Test plan

### 7.1 Layer 1 — unit tests with a mock peer (no network)

In-process `tonic` client that impersonates the peer: opens `Connect`, validates `REGISTER`, replies `REGISTERED`/`READY`, then drives scripted scenarios:

- handshake happy path; transaction before READY rejected
- `put_state`/`get_state` round trip; `ERROR` from peer surfaces as stub `Err`
- range query with multi-page `QueryResponse` (`has_more` + `QUERY_STATE_NEXT`)
- two interleaved transactions on one stream (response routing correctness)
- user panic → `COMPLETED` status 500, stream survives
- KEEPALIVE echo; abrupt stream close → clean reconnect on next peer dial
- `GetMetadata` interception and fall-through when undeclared
- `#[contract]`-macro routing, typed argument parsing, and metadata generation

See `fabric-shim/tests/mock_peer.rs` — 19 tests as of M5.

### 7.2 Layer 2 — reference chaincode

`examples/asset-transfer`: `CreateAsset`, `ReadAsset`, `UpdateAsset`, `DeleteAsset`, `GetAllAssets` (range), `TransferAsset` (event `AssetTransferred`), `ReadTransient` (echoes transient map) — deliberately mirroring `fabric-samples/asset-transfer-basic` so behavior can be diffed against the Go version.

```bash
docker build -f examples/asset-transfer/Dockerfile -t chainlaunch/rust-cc-asset-transfer:0.1 .
```

### 7.3 Layer 3 — end-to-end on ChainLaunch Pro (local)

Prereqs: a running ChainLaunch Pro instance, a running Fabric network (org + peer + orderer + channel) created via the UI or API, Docker Desktop (so `host.docker.internal` resolves from the peer container).

All lifecycle steps below are ChainLaunch's standard chaincode endpoints; nothing Rust-specific server-side. See [`docs/deployment.md`](deployment.md) for the full walkthrough and `examples/asset-transfer/e2e.sh` for a scripted version.

### 7.4 Interop acceptance checklist (M4 gate)

- [x] Container deploys via ChainLaunch; peer connects; shim logs `REGISTERED`/`READY`
- [x] `install → approve → commit → deploy` completes with no errors in the timeline
- [x] Invoke `CreateAsset` returns 200; committed to ledger (query from a second call sees it)
- [x] `ReadAsset` query returns the stored JSON byte-for-byte
- [x] `GetAllAssets` range query returns multiple assets (iterator paging exercised)
- [x] `TransferAsset` emits `AssetTransferred` event (visible in block/event listener)
- [x] Transient map passed via invoke `transient` field is readable, absent from proposal args
- [x] Error path: `ReadAsset` on a missing key returns status 500 with message, tx not committed
- [x] `GetMetadata` returns contract metadata through the peer, matching what the macro declared
- [ ] 20 concurrent invokes complete without cross-talk (distinct txids, correct payloads) — not yet load-tested
- [ ] `docker stop` → ChainLaunch undeploy/redeploy cycle works; peer reconnects automatically — not yet re-verified after the deploy-port fix
- [ ] Same checklist passes with the Go `asset-transfer-basic` CCaaS image swapped in, confirming behavior parity (differential test) — not yet run

### 7.5 Automation

`examples/asset-transfer/e2e.sh` scripts the full lifecycle against a live ChainLaunch instance, driven by env vars (`CHAINLAUNCH_API`, `NETWORK_ID`, `PEER_ID`, `KEY_ID`, `CHANNEL`). Not yet wired into CI (needs a disposable Fabric network as a fixture); the repo's CI workflow runs the mock-peer suite only.

## 8. Open questions

1. ~~Pin to Fabric 2.5 LTS protos only, or also verify against 3.x peers?~~ **Decided: Fabric 3.x only.** All interop testing (§7) runs against Fabric 3.x peers; no 2.x compatibility work.
2. TLS between peer and chaincode (§3.5) is still unimplemented. Should the deploy path grow first-class TLS material injection (secret env/volume) so CCaaS TLS is turnkey for any language, Rust included?
3. Crate naming and distribution: publish to crates.io under a stable namespace, and settle the repo's long-term home (this decision predates the repo becoming public — see the project README for current status).
