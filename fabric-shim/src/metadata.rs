//! Contract metadata, served for the `org.hyperledger.fabric:GetMetadata`
//! system function — the same mechanism `fabric-contract-api-go` and
//! `fabric-chaincode-node` provide.
//!
//! Implement [`crate::Chaincode::metadata`] to describe your contract; the
//! shim then answers `GetMetadata` automatically, which is what powers
//! ChainLaunch's chaincode metadata endpoint and UI explorer.
//!
//! Schemas follow the fabric-contract-api contract schema
//! (<https://hyperledger.github.io/fabric-chaincode-node/main/api/contract-schema.json>);
//! parameter/return schemas are free-form JSON Schema values.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const METADATA_FUNCTION: &str = "org.hyperledger.fabric:GetMetadata";
const CONTRACT_SCHEMA_URL: &str =
    "https://hyperledger.github.io/fabric-chaincode-node/main/api/contract-schema.json";

/// Top-level chaincode metadata document.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Metadata {
    #[serde(rename = "$schema", skip_serializing_if = "String::is_empty", default)]
    pub schema: String,
    pub info: Info,
    pub contracts: BTreeMap<String, Contract>,
    #[serde(skip_serializing_if = "Components::is_empty", default)]
    pub components: Components,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Info {
    pub title: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Contract {
    pub name: String,
    pub info: Info,
    pub transactions: Vec<Transaction>,
    #[serde(default)]
    pub default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Transaction {
    pub name: String,
    /// Conventional tags: `submit` (writes) and `evaluate` (read-only).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tag: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub parameters: Vec<Parameter>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub returns: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Parameter {
    pub name: String,
    pub schema: Value,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Components {
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub schemas: BTreeMap<String, Value>,
}

impl Components {
    fn is_empty(&self) -> bool {
        self.schemas.is_empty()
    }
}

impl Metadata {
    pub fn new(title: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            schema: CONTRACT_SCHEMA_URL.to_string(),
            info: Info {
                title: title.into(),
                version: version.into(),
                description: None,
            },
            contracts: BTreeMap::new(),
            components: Components::default(),
        }
    }

    pub fn contract(mut self, contract: Contract) -> Self {
        self.contracts.insert(contract.name.clone(), contract);
        self
    }

    /// Register a named schema under `components.schemas`, referencable from
    /// parameters as `{"$ref": "#/components/schemas/<name>"}`.
    pub fn component(mut self, name: impl Into<String>, schema: Value) -> Self {
        self.components.schemas.insert(name.into(), schema);
        self
    }
}

impl Contract {
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            info: Info {
                title: name.clone(),
                version: "latest".into(),
                description: None,
            },
            name,
            transactions: Vec::new(),
            default: true,
        }
    }

    pub fn transaction(mut self, tx: Transaction) -> Self {
        self.transactions.push(tx);
        self
    }
}

impl Transaction {
    /// A transaction that writes to the ledger (tagged `submit`).
    pub fn submit(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            tag: vec!["submit".into()],
            ..Default::default()
        }
    }

    /// A read-only transaction (tagged `evaluate`).
    pub fn evaluate(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            tag: vec!["evaluate".into()],
            ..Default::default()
        }
    }

    pub fn parameter(mut self, name: impl Into<String>, schema: Value) -> Self {
        self.parameters.push(Parameter {
            name: name.into(),
            schema,
            description: None,
        });
        self
    }

    pub fn returns(mut self, schema: Value) -> Self {
        self.returns = Some(schema);
        self
    }
}
