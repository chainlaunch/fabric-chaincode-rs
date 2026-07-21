# API Reference

*Reference — the complete `fabric-shim` surface. For a guided introduction
see [Getting started](getting-started.md).*

Crate layout:

| Crate | Contents |
|---|---|
| `fabric-shim` | Everything below; the only crate a chaincode depends on |
| `fabric-shim-macros` | `#[contract]` / `#[derive(DataType)]` (re-exported by `fabric-shim`) |
| `fabric-shim-protos` | Generated protobuf/gRPC bindings (re-exported as `fabric_shim::protos`) |

Supported target: Fabric 3.x peers, CCaaS mode only, plaintext gRPC.

## Environment variables

Read by `Server::from_env()`; the first defined variable of each pair wins.

| Variable | Fallback | Default | Meaning |
|---|---|---|---|
| `CHAINCODE_ID` | `CORE_CHAINCODE_ID` | — (required) | Installed package ID presented in REGISTER |
| `CHAINCODE_SERVER_ADDRESS` | `CORE_CHAINCODE_ADDRESS` | `0.0.0.0:7052` | `host:port` to bind |

## `Server`

```rust
Server::from_env() -> Result<Server>
Server::new(chaincode_id, "host:port") -> Result<Server>
    .max_message_size(bytes)             // default 100 MiB (DEFAULT_MAX_MESSAGE_SIZE)
    .start(chaincode).await -> Result<()>            // serves until SIGTERM / Ctrl-C
    .serve_with_listener(chaincode, tokio_listener, shutdown_future).await
```

`start` accepts any `impl Chaincode` and serves any number of concurrent peer
connections. On SIGTERM (what `docker stop` sends) it stops accepting work
and returns.

## The `Chaincode` trait

```rust
#[fabric_shim::async_trait]
pub trait Chaincode: Send + Sync + 'static {
    fn metadata(&self) -> Option<metadata::Metadata> { None }
    async fn init(&self, stub: ChaincodeStub) -> Response { self.invoke(stub).await }
    async fn invoke(&self, stub: ChaincodeStub) -> Response;
}
```

- `invoke` handles every transaction. `init` only runs for legacy
  `--init-required` deployments and defaults to delegating.
- When `metadata()` returns `Some`, the shim itself answers
  `org.hyperledger.fabric:GetMetadata` (the call never reaches `invoke`).
- A panic inside `invoke` is caught: the transaction completes with status
  500 and the peer connection survives.

You rarely implement this trait by hand — `#[contract]` generates it.

## `#[contract]` attribute macro

```rust
#[contract]                                            // name = type name, version = crate version
#[contract(name = "MyContract", version = "1.2.0", title = "My Contract")]
impl MyType { ... }
```

Generates `impl Chaincode for MyType` with routing, argument parsing, and
metadata. Within the block, annotate transaction methods:

```rust
#[transaction]                       // submit (writes); wire name = PascalCase(method)
#[transaction(evaluate)]             // read-only
#[transaction(submit, name = "createAsset")]  // explicit wire name
```

Method requirements:

| Rule | Detail |
|---|---|
| Receiver | `&self` |
| First parameter | context: `ctx: &ChaincodeStub` (any name) |
| Remaining parameters | any `ContractArg` type; parsed from the raw args in order |
| Return type | `Result<T, E>` with `T: ContractReturn`, `E: Display` |
| Async | optional (both `async fn` and `fn` work) |

Runtime behavior:

- Function dispatch strips an optional `Contract:` namespace prefix
  (`MyContract:CreateAsset` and `CreateAsset` both match).
- Wrong argument count → status 500 `"CreateAsset expects 2 argument(s), got 1"`.
- Unparsable argument → status 500 `"invalid argument `qty`: ..."`.
- `Err(e)` → status 500 with `e.to_string()`.
- Unknown function → status 500 `"unknown function <name>"`.
- Metadata: parameter names are camelCased (`appraised_value` →
  `appraisedValue`); tags are `submit`/`evaluate`; `Result<(), E>` produces
  no `returns` schema.

### Argument / return type support

| Type | As argument | As return | Wire encoding |
|---|---|---|---|
| `String` | ✓ | ✓ | raw UTF-8 |
| `u8..u64`, `i8..i64`, `usize`, `isize` | ✓ | ✓ | decimal string; schema `integer` |
| `f32`, `f64` | ✓ | ✓ | decimal string; schema `number` |
| `bool` | ✓ | ✓ | `"true"`/`"false"` |
| `()` | — | ✓ | empty payload |
| `Vec<T>` | ✓ (JSON array) | ✓ (JSON array) | JSON |
| `#[derive(DataType)]` struct | ✓ (JSON) | ✓ (JSON) | JSON; schema `$ref` |

## `#[derive(DataType)]`

For structs with named fields; also requires `Serialize` + `Deserialize`.
Generates `ContractSchema` (a `$ref` to `#/components/schemas/<Name>` plus
the component object schema), `ContractArg` (JSON parse), and
`ContractReturn` (JSON serialize).

Schema generation honors serde attributes so the schema always matches the
serialized JSON:

- `#[serde(rename = "ID")]` on a field
- `#[serde(rename_all = "camelCase" | "PascalCase" | "lowercase" |
  "UPPERCASE" | "SCREAMING_SNAKE_CASE" | "kebab-case")]` on the struct
- `Option<T>` fields are omitted from the schema's `required` list

Not supported: generic structs, enums, tuple structs.

## `ChaincodeStub`

All ledger calls are `async` and return `Result<_, Error>`. One ledger call
per transaction is in flight at a time (calls made concurrently on the same
stub serialize).

### Transaction context

| Method | Returns | Notes |
|---|---|---|
| `get_tx_id()` | `&str` | |
| `get_channel_id()` | `&str` | |
| `get_tx_timestamp()` | `Result<Timestamp>` | From the channel header — identical on all endorsers, safe for logic |
| `get_args()` | `&[Vec<u8>]` | Raw args including the function name at index 0 |
| `get_string_args()` | `Vec<String>` | Lossy UTF-8 |
| `get_function_and_args()` | `(String, Vec<String>)` | |
| `get_transient()` | `&HashMap<String, Vec<u8>>` | Private inputs; never written to the ledger |
| `get_decorations()` | `&HashMap<String, Vec<u8>>` | |
| `get_creator()` | `&[u8]` | Marshaled `msp.SerializedIdentity` |
| `get_creator_identity()` | `Result<SerializedIdentity>` | `.mspid` + `.id_bytes` (PEM cert) |
| `get_signed_proposal()` | `Option<&SignedProposal>` | |

### World state

| Method | Notes |
|---|---|
| `get_state(key)` → `Vec<u8>` | **Empty vec when the key does not exist** |
| `put_state(key, value)` | Key must be non-empty |
| `del_state(key)` | |
| `get_state_by_range(start, end)` → `StateQueryIterator` | `[start, end)`; empty string = unbounded |
| `get_state_by_range_with_pagination(start, end, page_size, bookmark)` → `(iterator, QueryResponseMetadata)` | Evaluate-only |
| `get_query_result(query)` → `StateQueryIterator` | CouchDB state databases only |
| `get_history_for_key(key)` → `HistoryQueryIterator` | Requires peer history DB |

### Composite keys

| Method | Notes |
|---|---|
| `create_composite_key(object_type, &attrs)` | `\u{0}`-delimited, Go-compatible format; also a free function |
| `split_composite_key(key)` | → `(object_type, attrs)`; also a free function |
| `get_state_by_partial_composite_key(object_type, &attrs)` | Range scan over the composite prefix |

### Private data

All take a non-empty `collection` first.

`get_private_data`, `put_private_data`, `del_private_data`,
`get_private_data_hash` (readable from any org), `purge_private_data`,
`get_private_data_by_range`.

### Events & cross-chaincode

| Method | Notes |
|---|---|
| `set_event(name, payload)` | One event per transaction; a second call replaces the first |
| `invoke_chaincode(name, args, channel)` → `Result<Response>` | Empty `channel` = same channel. Same-channel calls join this transaction's read/write set; cross-channel calls are read-only |

### Not implemented (planned)

`get_state_validation_parameter` / `set_state_validation_parameter`
(key-level endorsement), `get_query_result_with_pagination`,
`get_private_data_query_result`, `get_binding`, client-identity attribute
helpers (ABAC), TLS between peer and chaincode.

## Iterators

```rust
StateQueryIterator::next().await -> Result<Option<queryresult::Kv>>       // .key / .value / .namespace
StateQueryIterator::collect_remaining().await -> Result<Vec<Kv>>          // drains + closes
StateQueryIterator::close().await -> Result<()>
HistoryQueryIterator::next().await -> Result<Option<KeyModification>>     // .tx_id / .value / .timestamp / .is_delete
```

Batches are fetched from the peer transparently (`QUERY_STATE_NEXT`). Prefer
`collect_remaining()`; if you loop manually, call `close()` when done (the
peer also closes leftover iterators at transaction end).

## `Response`

```rust
Response::success(payload)      // status 200
Response::success_empty()
Response::error(message)        // status 500
response.is_error()             // status >= 400
```

Constants: `fabric_shim::OK` (200), `ERROR` (500), `ERROR_THRESHOLD` (400).
Statuses ≥ 400 mark the transaction as failed; the message travels back to
the client through the peer/gateway.

## `Error`

```rust
pub enum Error {
    Peer(String),          // peer answered a ledger request with ERROR
    Protocol(String),      // shim protocol violation
    Decode(prost::DecodeError),
    ConnectionClosed,
    Config(String),        // bad env vars / addresses
    InvalidArgument(String),
    Transport(String),
}
```

All variants implement `Display`, so `Result<T, Error>` works directly as a
`#[transaction]` return type.

## `metadata` module

Used automatically by `#[contract]`; build by hand only when implementing
`Chaincode` directly:

```rust
metadata::Metadata::new(title, version)
    .contract(metadata::Contract::new("Name")
        .transaction(metadata::Transaction::submit("CreateAsset")
            .parameter("id", json!({"type": "string"}))
            .returns(json!({"$ref": "#/components/schemas/Asset"}))))
    .component("Asset", json!({ "type": "object", ... }))
```

`metadata::METADATA_FUNCTION` is the wire name the shim intercepts
(`org.hyperledger.fabric:GetMetadata`). The generated document follows the
fabric-contract-api contract schema, so ChainLaunch's metadata endpoint and
any tooling built for Go/Node metadata work unchanged.

## Re-exports

| Path | What |
|---|---|
| `fabric_shim::async_trait` | `async-trait` attribute (needed for manual `Chaincode` impls) |
| `fabric_shim::serde_json` | Used by macro-generated code; handy for payloads |
| `fabric_shim::protos` | Generated Fabric protobufs (`peer`, `common`, `msp`, `queryresult`) |
