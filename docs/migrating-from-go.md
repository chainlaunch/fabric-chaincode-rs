# Migrating from Go (`fabric-chaincode-go`)

*How-to guide — port an existing Go chaincode to `fabric-chaincode-rs`.*

This guide maps both Go styles to Rust: the high-level
`fabric-contract-api-go` (`contractapi.Contract`) and the low-level
`shim.Chaincode` interface. Ledger data is unaffected — a Rust chaincode
reads state written by its Go predecessor, provided you keep the same keys
and JSON field names.

## Concept mapping

| Go (`contractapi`) | Rust (`fabric-shim`) |
|---|---|
| `contractapi.Contract` embedded struct | Plain struct + `#[contract]` on its impl block |
| Exported method = transaction | Method annotated `#[transaction]` |
| `GetEvaluateTransactions()` / `evaluate` tag | `#[transaction(evaluate)]` |
| `ctx contractapi.TransactionContextInterface` | `ctx: &ChaincodeStub` (first parameter) |
| `ctx.GetStub()` | not needed — `ctx` *is* the stub |
| Typed params parsed from strings | Same, via `ContractArg` (compile-time checked) |
| Return `(T, error)` | Return `Result<T, E: Display>` |
| Struct with `json:` tags | Struct with `#[derive(DataType, Serialize, Deserialize)]` + `serde` attrs |
| Metadata from reflection (`GetMetadata`) | Metadata from the macro (same wire function) |
| `contractapi.NewChaincode(...)` + `cc.Start()` | `Server::from_env()?.start(MyContract).await` |

| Go (`shim` low-level) | Rust equivalent |
|---|---|
| `shim.Chaincode` interface (`Init`/`Invoke`) | `Chaincode` trait (`init`/`invoke`), implement directly instead of using `#[contract]` |
| `shim.Success(payload)` / `shim.Error(msg)` | `Response::success(payload)` / `Response::error(msg)` |
| `shim.ChaincodeServer{...}.Start()` | `Server::new(id, addr)?.start(cc).await` |

## Side by side

Go (`contractapi`):

```go
type SmartContract struct { contractapi.Contract }

type Asset struct {
    ID             string `json:"ID"`
    Owner          string `json:"owner"`
    AppraisedValue int    `json:"appraisedValue"`
}

func (s *SmartContract) CreateAsset(ctx contractapi.TransactionContextInterface,
    id string, owner string, appraisedValue int) error {
    exists, err := s.AssetExists(ctx, id)
    if err != nil { return err }
    if exists { return fmt.Errorf("the asset %s already exists", id) }
    asset := Asset{ID: id, Owner: owner, AppraisedValue: appraisedValue}
    assetJSON, err := json.Marshal(asset)
    if err != nil { return err }
    return ctx.GetStub().PutState(id, assetJSON)
}

func (s *SmartContract) ReadAsset(ctx contractapi.TransactionContextInterface,
    id string) (*Asset, error) {
    assetJSON, err := ctx.GetStub().GetState(id)
    if err != nil { return nil, err }
    if assetJSON == nil { return nil, fmt.Errorf("the asset %s does not exist", id) }
    var asset Asset
    return &asset, json.Unmarshal(assetJSON, &asset)
}
```

Rust:

```rust
#[derive(DataType, Serialize, Deserialize)]
struct Asset {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "owner")]
    owner: String,
    #[serde(rename = "appraisedValue")]
    appraised_value: i64,
}

#[derive(Default)]
struct SmartContract;

#[contract(name = "SmartContract")]
impl SmartContract {
    #[transaction]
    async fn create_asset(&self, ctx: &ChaincodeStub,
        id: String, owner: String, appraised_value: i64) -> Result<(), Error> {
        if !ctx.get_state(&id).await?.is_empty() {
            return Err(Error::InvalidArgument(format!("the asset {id} already exists")));
        }
        let asset = Asset { id: id.clone(), owner, appraised_value };
        ctx.put_state(&id, serde_json::to_vec(&asset).unwrap()).await
    }

    #[transaction(evaluate)]
    async fn read_asset(&self, ctx: &ChaincodeStub, id: String) -> Result<Asset, Error> {
        let bytes = ctx.get_state(&id).await?;
        if bytes.is_empty() {
            return Err(Error::InvalidArgument(format!("the asset {id} does not exist")));
        }
        serde_json::from_slice(&bytes).map_err(|e| Error::InvalidArgument(e.to_string()))
    }
}
```

Keep the `serde` renames identical to your Go `json:` tags — that is what
keeps the on-ledger JSON byte-compatible.

## Stub method mapping

| Go `ChaincodeStubInterface` | Rust `ChaincodeStub` |
|---|---|
| `GetState(key)` | `get_state(key).await` |
| `PutState(key, value)` | `put_state(key, value).await` |
| `DelState(key)` | `del_state(key).await` |
| `GetStateByRange(start, end)` | `get_state_by_range(start, end).await` → iterator |
| `GetStateByRangeWithPagination(...)` | `get_state_by_range_with_pagination(...).await` |
| `GetQueryResult(query)` (CouchDB) | `get_query_result(query).await` |
| `GetHistoryForKey(key)` | `get_history_for_key(key).await` |
| `CreateCompositeKey(type, attrs)` | `create_composite_key(type, &attrs)` (same `\u{0}` format) |
| `SplitCompositeKey(key)` | `split_composite_key(key)` |
| `GetStateByPartialCompositeKey(...)` | `get_state_by_partial_composite_key(...).await` |
| `GetPrivateData(col, key)` / `PutPrivateData` / `DelPrivateData` | `get_private_data` / `put_private_data` / `del_private_data` |
| `GetPrivateDataHash(col, key)` | `get_private_data_hash(col, key).await` |
| `PurgePrivateData(col, key)` | `purge_private_data(col, key).await` |
| `GetPrivateDataByRange(...)` | `get_private_data_by_range(...).await` |
| `GetTransient()` | `get_transient()` |
| `GetCreator()` | `get_creator()` (raw) / `get_creator_identity()` (decoded `SerializedIdentity`) |
| `GetArgs()` / `GetStringArgs()` / `GetFunctionAndParameters()` | `get_args()` / `get_string_args()` / `get_function_and_args()` |
| `GetTxID()` / `GetChannelID()` / `GetTxTimestamp()` | `get_tx_id()` / `get_channel_id()` / `get_tx_timestamp()` |
| `SetEvent(name, payload)` | `set_event(name, payload)` |
| `InvokeChaincode(name, args, channel)` | `invoke_chaincode(name, args, channel).await` |
| `GetDecorations()` | `get_decorations()` |
| `GetSignedProposal()` | `get_signed_proposal()` |

### Not yet available in Rust

| Go API | Status |
|---|---|
| `cid.GetClientIdentity()` (MSP ID / cert attrs / ABAC) | No helper yet. `get_creator_identity()` gives the MSP ID and raw certificate bytes; parse attributes with an x509 crate if you need ABAC. |
| `Get/SetStateValidationParameter` (key-level endorsement) | Not implemented (planned). |
| `GetQueryResultWithPagination`, `GetPrivateDataQueryResult` | Not implemented (planned). |
| `GetBinding` | Not implemented. |

If your Go chaincode relies on one of these, keep that logic in Go or wait
for the corresponding milestone.

## Semantic differences to check during the port

1. **Missing keys.** Go's `GetState` returns `nil` for a missing key; Rust's
   `get_state` returns an **empty `Vec<u8>`**. Replace `assetJSON == nil`
   checks with `bytes.is_empty()`. (Fabric itself cannot distinguish a
   missing key from an empty value here — same as Go.)
2. **Errors, not panics.** Every ledger call returns `Result`; `?` replaces
   the `if err != nil` ladder. A panic in your handler is caught by the shim
   and returned as a status-500 response — the process and the peer
   connection survive, but treat panics as bugs, not control flow.
3. **Iterators are explicit.** Go's `defer resultsIterator.Close()` becomes
   either `iter.collect_remaining().await` (drains and closes) or a
   `while let Some(kv) = iter.next().await?` loop followed by
   `iter.close().await?`. The peer also cleans up open iterators when the
   transaction completes.
4. **Init is just a transaction.** Fabric 2+ lifecycle without
   `--init-required` never sends INIT; the trait's `init` defaults to
   delegating to `invoke`. Port Go `InitLedger` as an ordinary `#[transaction]`.
5. **One event per transaction.** `set_event` replaces any previous event,
   matching Go's `SetEvent` semantics.
6. **Numbers arrive as strings.** Same as Go's contractapi: `"300"` is
   parsed into your `u64`/`i64` parameter. Pick integer widths deliberately —
   Go's `int` is 64-bit on the peer platforms you likely used.
7. **Deployment is CCaaS-only.** If your Go chaincode was peer-managed
   (built by the peer from source), switch to the CCaaS model — see
   [deployment](deployment.md). If it already ran as CCaaS
   (`shim.ChaincodeServer`), the container contract is identical.

## Migration checklist

- [ ] Recreate data structs with `DataType + Serialize + Deserialize`,
      copying every `json:` tag into `#[serde(rename)]`.
- [ ] Port each transaction as a `#[transaction]` method; wire names must
      match the Go method names (PascalCase does this automatically; use
      `#[transaction(name = "...")]` for irregular names).
- [ ] Mark read-only transactions `#[transaction(evaluate)]` (Go:
      `GetEvaluateTransactions` or usage convention).
- [ ] Replace `nil` state checks with `is_empty()`.
- [ ] Replace iterator `defer Close()` with `collect_remaining()`/`close()`.
- [ ] Build the Docker image, deploy on a dev channel, and run your existing
      client tests against it — the client side (gateway SDKs, ChainLaunch
      invoke/query) needs no changes.
- [ ] If both versions must coexist during rollout, deploy the Rust image as
      a new sequence of the *same* chaincode name — state carries over.
