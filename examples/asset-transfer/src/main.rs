//! Reference CCaaS chaincode mirroring `fabric-samples/asset-transfer-basic`,
//! written with the `#[contract]` API: routing, argument parsing, and
//! `GetMetadata` all come from the annotations.

#![forbid(unsafe_code)]

use fabric_shim::{contract, ChaincodeStub, DataType, Error, Server};
use serde::{Deserialize, Serialize};

/// JSON field names match the Go sample so ledgers written by either
/// implementation are byte-compatible at the application level.
#[derive(Debug, DataType, Serialize, Deserialize, PartialEq)]
struct Asset {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "color")]
    color: String,
    #[serde(rename = "size")]
    size: u32,
    #[serde(rename = "owner")]
    owner: String,
    #[serde(rename = "appraisedValue")]
    appraised_value: u64,
}

/// Mirrors the Go sample's `QueryResult{Key, Record}` wrapper — confirmed
/// byte-for-byte against the official chaincode via a differential test on
/// a live Fabric network (see docs/verification.md). GetAllAssets returns
/// this wrapped shape, not a bare `Vec<Asset>`, to match upstream exactly.
#[derive(DataType, Serialize, Deserialize)]
struct QueryResult {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "Record")]
    record: Asset,
}

#[derive(Default)]
struct AssetTransfer;

impl AssetTransfer {
    async fn get_asset(ctx: &ChaincodeStub, id: &str) -> Result<Asset, Error> {
        let bytes = ctx.get_state(id).await?;
        if bytes.is_empty() {
            return Err(Error::InvalidArgument(format!(
                "the asset {id} does not exist"
            )));
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| Error::InvalidArgument(format!("stored asset {id} is corrupt: {e}")))
    }

    async fn put_asset(ctx: &ChaincodeStub, asset: &Asset) -> Result<(), Error> {
        let json = serde_json::to_vec(asset)
            .map_err(|e| Error::InvalidArgument(format!("serialize asset: {e}")))?;
        ctx.put_state(&asset.id, json).await
    }
}

#[contract(name = "AssetTransfer")]
impl AssetTransfer {
    #[transaction]
    async fn init_ledger(&self, ctx: &ChaincodeStub) -> Result<(), Error> {
        for (id, color, size, owner, appraised_value) in [
            ("asset1", "blue", 5, "Tomoko", 300),
            ("asset2", "red", 5, "Brad", 400),
            ("asset3", "green", 10, "Jin Soo", 500),
            ("asset4", "yellow", 10, "Max", 600),
            ("asset5", "black", 15, "Adriana", 700),
            ("asset6", "white", 15, "Michel", 800),
        ] {
            let asset = Asset {
                id: id.into(),
                color: color.into(),
                size,
                owner: owner.into(),
                appraised_value,
            };
            Self::put_asset(ctx, &asset).await?;
        }
        Ok(())
    }

    #[transaction]
    async fn create_asset(
        &self,
        ctx: &ChaincodeStub,
        id: String,
        color: String,
        size: u32,
        owner: String,
        appraised_value: u64,
    ) -> Result<(), Error> {
        if !ctx.get_state(&id).await?.is_empty() {
            return Err(Error::InvalidArgument(format!(
                "the asset {id} already exists"
            )));
        }
        Self::put_asset(
            ctx,
            &Asset {
                id,
                color,
                size,
                owner,
                appraised_value,
            },
        )
        .await
    }

    #[transaction(evaluate)]
    async fn read_asset(&self, ctx: &ChaincodeStub, id: String) -> Result<Asset, Error> {
        Self::get_asset(ctx, &id).await
    }

    #[transaction]
    async fn update_asset(
        &self,
        ctx: &ChaincodeStub,
        id: String,
        color: String,
        size: u32,
        owner: String,
        appraised_value: u64,
    ) -> Result<(), Error> {
        Self::get_asset(ctx, &id).await?; // must exist
        Self::put_asset(
            ctx,
            &Asset {
                id,
                color,
                size,
                owner,
                appraised_value,
            },
        )
        .await
    }

    #[transaction]
    async fn delete_asset(&self, ctx: &ChaincodeStub, id: String) -> Result<(), Error> {
        Self::get_asset(ctx, &id).await?; // must exist
        ctx.del_state(&id).await
    }

    #[transaction(evaluate)]
    async fn asset_exists(&self, ctx: &ChaincodeStub, id: String) -> Result<bool, Error> {
        Ok(!ctx.get_state(&id).await?.is_empty())
    }

    /// Returns the previous owner and emits an `AssetTransferred` event.
    #[transaction]
    async fn transfer_asset(
        &self,
        ctx: &ChaincodeStub,
        id: String,
        new_owner: String,
    ) -> Result<String, Error> {
        let mut asset = Self::get_asset(ctx, &id).await?;
        let old_owner = std::mem::replace(&mut asset.owner, new_owner);
        Self::put_asset(ctx, &asset).await?;
        ctx.set_event("AssetTransferred", serde_json::to_vec(&asset).unwrap())?;
        Ok(old_owner)
    }

    #[transaction(evaluate)]
    async fn get_all_assets(&self, ctx: &ChaincodeStub) -> Result<Vec<QueryResult>, Error> {
        let iter = ctx.get_state_by_range("", "").await?;
        Ok(iter
            .collect_remaining()
            .await?
            .into_iter()
            .filter_map(|kv| {
                serde_json::from_slice(&kv.value)
                    .ok()
                    .map(|record| QueryResult {
                        key: kv.key,
                        record,
                    })
            })
            .collect())
    }

    /// Test helper for the ChainLaunch interop checklist: echoes the
    /// `asset_properties` transient field without writing state.
    #[transaction(evaluate)]
    async fn read_transient(&self, ctx: &ChaincodeStub) -> Result<String, Error> {
        let value = ctx
            .get_transient()
            .get("asset_properties")
            .cloned()
            .ok_or_else(|| {
                Error::InvalidArgument("transient field asset_properties not set".into())
            })?;
        String::from_utf8(value).map_err(|e| Error::InvalidArgument(e.to_string()))
    }
}

#[tokio::main]
async fn main() -> Result<(), fabric_shim::Error> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    Server::from_env()?.start(AssetTransfer).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use fabric_shim::Chaincode;

    #[test]
    fn asset_json_matches_go_sample_shape() {
        let asset = Asset {
            id: "asset1".into(),
            color: "blue".into(),
            size: 5,
            owner: "Tomoko".into(),
            appraised_value: 300,
        };
        let json = serde_json::to_string(&asset).unwrap();
        assert_eq!(
            json,
            r#"{"ID":"asset1","color":"blue","size":5,"owner":"Tomoko","appraisedValue":300}"#
        );
        let back: Asset = serde_json::from_str(&json).unwrap();
        assert_eq!(back, asset);
    }

    #[test]
    fn contract_metadata_is_generated_from_annotations() {
        let md = AssetTransfer.metadata().expect("macro generates metadata");
        let json = serde_json::to_value(&md).unwrap();

        assert_eq!(json["info"]["title"], "AssetTransfer");
        let txs = json["contracts"]["AssetTransfer"]["transactions"]
            .as_array()
            .unwrap();
        let names: Vec<&str> = txs.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "InitLedger",
                "CreateAsset",
                "ReadAsset",
                "UpdateAsset",
                "DeleteAsset",
                "AssetExists",
                "TransferAsset",
                "GetAllAssets",
                "ReadTransient"
            ]
        );

        let create = &txs[1];
        assert_eq!(create["tag"][0], "submit");
        assert_eq!(create["parameters"][0]["name"], "id");
        assert_eq!(create["parameters"][2]["name"], "size");
        assert_eq!(create["parameters"][2]["schema"]["type"], "integer");
        assert_eq!(create["parameters"][4]["name"], "appraisedValue");

        let read = &txs[2];
        assert_eq!(read["tag"][0], "evaluate");
        assert_eq!(read["returns"]["$ref"], "#/components/schemas/Asset");

        // DataType schema honors the serde renames.
        let asset_schema = &json["components"]["schemas"]["Asset"];
        assert_eq!(asset_schema["properties"]["ID"]["type"], "string");
        assert_eq!(
            asset_schema["properties"]["appraisedValue"]["type"],
            "integer"
        );
        assert!(asset_schema["required"]
            .as_array()
            .unwrap()
            .contains(&serde_json::Value::from("appraisedValue")));
    }
}
