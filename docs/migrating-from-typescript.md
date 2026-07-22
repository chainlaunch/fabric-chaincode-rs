# Migrating from TypeScript (`fabric-chaincode-node`)

*How-to guide — port a `fabric-contract-api` (TypeScript/JavaScript)
chaincode to `fabric-chaincode-rs`.*

The Rust contract API was designed to feel like the TypeScript decorators:
one annotation per transaction, typed parameters, automatic metadata. Ledger
data is unaffected by the port as long as keys and JSON field names stay the
same.

## Decorator mapping

| TypeScript | Rust |
|---|---|
| `class MyContract extends Contract` | plain struct + `#[contract]` impl block |
| `@Transaction()` | `#[transaction]` |
| `@Transaction(false)` (read-only) | `#[transaction(evaluate)]` |
| `@Returns('Asset')` | inferred from the method's `Result<Asset, E>` |
| `@Object()` on a class | `#[derive(DataType)]` on a struct |
| `@Property()` on a field | any struct field (schema from the field's type) |
| `@Info({title, version})` | `#[contract(name = "...", version = "...", title = "...")]` |
| `ctx: Context` | `ctx: &ChaincodeStub` |
| `ctx.stub` | `ctx` itself |
| `ctx.clientIdentity` | `ctx.get_creator_identity()` (MSP ID + cert bytes; no attribute helper yet) |
| method name `createAsset` → wire `createAsset` | method `create_asset` → wire `CreateAsset` (override with `#[transaction(name = "createAsset")]` to keep old names!) |

> ⚠️ **Wire names differ by default.** fabric-chaincode-node exposes methods
> under their literal camelCase names (`createAsset`); the Rust macro
> PascalCases them (`CreateAsset`). If existing clients call camelCase names,
> pin them: `#[transaction(name = "createAsset")]`.

## Side by side

TypeScript:

```typescript
@Object()
export class Asset {
    @Property() public ID: string = '';
    @Property() public owner: string = '';
    @Property() public appraisedValue: number = 0;
}

@Info({ title: 'AssetTransfer', description: 'Basic asset transfer' })
export class AssetTransferContract extends Contract {

    @Transaction()
    public async CreateAsset(ctx: Context, id: string, owner: string,
                             appraisedValue: number): Promise<void> {
        const exists = await this.AssetExists(ctx, id);
        if (exists) {
            throw new Error(`The asset ${id} already exists`);
        }
        const asset: Asset = { ID: id, owner, appraisedValue };
        await ctx.stub.putState(id, Buffer.from(stringify(sortKeysRecursive(asset))));
    }

    @Transaction(false)
    @Returns('Asset')
    public async ReadAsset(ctx: Context, id: string): Promise<string> {
        const assetJSON = await ctx.stub.getState(id);
        if (assetJSON.length === 0) {
            throw new Error(`The asset ${id} does not exist`);
        }
        return assetJSON.toString();
    }
}
```

Rust:

```rust
#[derive(DataType, Serialize, Deserialize)]
struct Asset {
    #[serde(rename = "ID")]
    id: String,
    owner: String,
    #[serde(rename = "appraisedValue")]
    appraised_value: u64,
}

#[derive(Default)]
struct AssetTransferContract;

#[contract(name = "AssetTransfer", title = "AssetTransfer")]
impl AssetTransferContract {
    #[transaction]
    async fn create_asset(&self, ctx: &ChaincodeStub,
        id: String, owner: String, appraised_value: u64) -> Result<(), Error> {
        if !ctx.get_state(&id).await?.is_empty() {
            return Err(Error::InvalidArgument(format!("The asset {id} already exists")));
        }
        let asset = Asset { id: id.clone(), owner, appraised_value };
        ctx.put_state(&id, serde_json::to_vec(&asset).unwrap()).await
    }

    #[transaction(evaluate)]
    async fn read_asset(&self, ctx: &ChaincodeStub, id: String) -> Result<Asset, Error> {
        let bytes = ctx.get_state(&id).await?;
        if bytes.is_empty() {
            return Err(Error::InvalidArgument(format!("The asset {id} does not exist")));
        }
        serde_json::from_slice(&bytes).map_err(|e| Error::InvalidArgument(e.to_string()))
    }
}
```

`throw new Error(...)` becomes `return Err(...)` — the shim turns any `Err`
into a status-500 response with the message, exactly like an uncaught throw
in the Node runtime.

## Stub method mapping

| TypeScript `ctx.stub` | Rust `ctx` |
|---|---|
| `getState(key)` → `Uint8Array` (empty if missing) | `get_state(key).await?` → `Vec<u8>` (empty if missing) — same semantics |
| `putState(key, buffer)` | `put_state(key, bytes).await?` |
| `deleteState(key)` | `del_state(key).await?` |
| `getStateByRange(start, end)` (async iterable) | `get_state_by_range(start, end).await?` → iterator |
| `getStateByRangeWithPagination(...)` | `get_state_by_range_with_pagination(...).await?` |
| `getQueryResult(query)` | `get_query_result(query).await?` |
| `getHistoryForKey(key)` | `get_history_for_key(key).await?` |
| `createCompositeKey(type, attrs)` | `create_composite_key(type, &attrs)?` |
| `getStateByPartialCompositeKey(...)` | `get_state_by_partial_composite_key(...).await?` |
| `getPrivateData` / `putPrivateData` / `deletePrivateData` | `get_private_data` / `put_private_data` / `del_private_data` |
| `getPrivateDataHash(col, key)` | `get_private_data_hash(col, key).await?` |
| `getTransient()` (Map) | `get_transient()` (`&HashMap<String, Vec<u8>>`) |
| `setEvent(name, buffer)` | `set_event(name, bytes)?` |
| `invokeChaincode(name, args, channel)` | `invoke_chaincode(name, args, channel).await?` |
| `getTxID()` / `getChannelID()` / `getTxTimestamp()` | `get_tx_id()` / `get_channel_id()` / `get_tx_timestamp()` |
| `getCreator()` | `get_creator()` / `get_creator_identity()` |

The `for await (const res of iterator)` pattern becomes:

```rust
let mut iter = ctx.get_state_by_range("", "").await?;
while let Some(kv) = iter.next().await? {
    // kv.key, kv.value
}
iter.close().await?;
// or, to drain in one call:
let all = ctx.get_state_by_range("", "").await?.collect_remaining().await?;
```

## Semantic differences to check during the port

1. **Wire names** — see the warning above; pin camelCase names with
   `#[transaction(name = "...")]` if clients depend on them.
2. **JSON key order.** Many Node samples use `json-stringify-deterministic` +
   `sort-keys-recursive` to make state bytes deterministic. `serde_json`
   serializes struct fields in declaration order — deterministic by
   construction, but **not alphabetically sorted**. If your application (or
   an endorsement comparison) depends on the exact stored bytes, declare the
   Rust struct fields in the same order the Node code stored them, or keep
   reads tolerant (parse JSON rather than comparing bytes).
3. **Numbers.** JavaScript's `number` is a float; Rust makes you choose
   (`u64`, `i64`, `f64`). Amounts that were integers in practice should
   become integer types — this tightens validation for free (the transaction
   fails with a clear error if a client sends `"3.5"`).
4. **`undefined` checks** (`!assetJSON || assetJSON.length === 0`) become
   `bytes.is_empty()` — the empty-buffer semantics are the same.
5. **Concurrency model.** The Node runtime processes transactions on one
   event loop; the Rust shim runs each transaction on its own task. Don't
   share mutable state across transactions except through the ledger (the
   contract struct is `&self`, which enforces this at compile time).
6. **Metadata.** Both runtimes serve `org.hyperledger.fabric:GetMetadata`;
   the Rust document is generated from signatures, so `@Returns`/`@Object`
   information carries over automatically once types are ported.
7. **Client identity attributes** (`ctx.clientIdentity.getAttributeValue`)
   have no helper yet — parse the certificate from
   `get_creator_identity()?.id_bytes` with an x509 crate if you use ABAC.

## Migration checklist

- [ ] Port `@Object()` classes to `DataType` structs; match every JSON field
      name with `#[serde(rename)]` (TypeScript property names are the JSON
      names).
- [ ] Port each `@Transaction()` method; decide wire-name policy (keep
      camelCase via `name = "..."`, or move clients to PascalCase).
- [ ] Convert `throw` → `Err`, `Promise<T>` → `Result<T, E>`.
- [ ] Replace async-iterable loops with `next()/close()` or
      `collect_remaining()`.
- [ ] Review number types and JSON key-order assumptions (items 2–3 above).
- [ ] Build the image, deploy as a new sequence of the same chaincode name
      ([deployment guide](deployment.md)), and run your existing client test
      suite unchanged.
