//! High-level contract API — the Rust equivalent of `fabric-contract-api-go`
//! and fabric-chaincode-node's `@Transaction`/`@Object` decorators.
//!
//! Annotate an impl block with [`#[contract]`](macro@crate::contract) and the
//! macro generates the [`crate::Chaincode`] implementation for you: function
//! routing by PascalCase name (namespaced `Contract:Function` calls accepted),
//! typed argument parsing, JSON serialization of return values, and contract
//! metadata for `org.hyperledger.fabric:GetMetadata` — all derived from the
//! method signatures. Annotate parameter/return structs with
//! [`#[derive(DataType)]`](macro@crate::DataType); their JSON schema follows
//! the struct's serde attributes.
//!
//! ```no_run
//! use fabric_shim::{contract, ChaincodeStub, DataType, Error, Server};
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(DataType, Serialize, Deserialize)]
//! #[serde(rename_all = "camelCase")]
//! pub struct Car {
//!     #[serde(rename = "ID")]
//!     pub id: String,
//!     pub max_speed: u32,
//! }
//!
//! #[derive(Default)]
//! pub struct CarContract;
//!
//! #[contract(name = "CarContract")]
//! impl CarContract {
//!     /// Called as "CreateCar" (or "CarContract:CreateCar").
//!     #[transaction]
//!     async fn create_car(&self, ctx: &ChaincodeStub, car: Car) -> Result<(), Error> {
//!         ctx.put_state(&car.id.clone(), serde_json::to_vec(&car).unwrap()).await
//!     }
//!
//!     #[transaction(evaluate)]
//!     async fn read_car(&self, ctx: &ChaincodeStub, id: String) -> Result<Car, Error> {
//!         let bytes = ctx.get_state(&id).await?;
//!         serde_json::from_slice(&bytes).map_err(|e| Error::InvalidArgument(e.to_string()))
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Error> {
//!     Server::from_env()?.start(CarContract).await
//! }
//! ```
//!
//! Transaction methods must take `&self`, then a context (`ctx:
//! &ChaincodeStub`), then any typed parameters, and return `Result<T, E>`
//! where `E: Display`. Strings and numbers are parsed from the raw argument
//! text; `DataType` structs and `Vec<T>` are parsed as JSON.

use std::collections::BTreeMap;

use serde_json::{json, Value};

/// JSON-schema description of a type, used in generated contract metadata.
pub trait ContractSchema {
    fn schema() -> Value;

    /// Register any named component schemas this type references.
    fn components(_out: &mut BTreeMap<String, Value>) {}
}

/// A type that can be parsed from a raw transaction argument.
pub trait ContractArg: ContractSchema + Sized {
    fn from_arg(raw: &[u8]) -> Result<Self, String>;
}

/// A type that can be returned from a transaction method.
pub trait ContractReturn: ContractSchema {
    fn into_payload(self) -> Result<Vec<u8>, String>;
}

impl ContractSchema for String {
    fn schema() -> Value {
        json!({"type": "string"})
    }
}

impl ContractArg for String {
    fn from_arg(raw: &[u8]) -> Result<Self, String> {
        String::from_utf8(raw.to_vec()).map_err(|e| e.to_string())
    }
}

impl ContractReturn for String {
    fn into_payload(self) -> Result<Vec<u8>, String> {
        Ok(self.into_bytes())
    }
}

impl ContractSchema for bool {
    fn schema() -> Value {
        json!({"type": "boolean"})
    }
}

impl ContractArg for bool {
    fn from_arg(raw: &[u8]) -> Result<Self, String> {
        std::str::from_utf8(raw)
            .map_err(|e| e.to_string())?
            .trim()
            .parse()
            .map_err(|_| "expected \"true\" or \"false\"".to_string())
    }
}

impl ContractReturn for bool {
    fn into_payload(self) -> Result<Vec<u8>, String> {
        Ok(self.to_string().into_bytes())
    }
}

macro_rules! numeric_contract_types {
    ($json_type:literal => $($t:ty),+) => {$(
        impl ContractSchema for $t {
            fn schema() -> Value {
                json!({"type": $json_type})
            }
        }

        impl ContractArg for $t {
            fn from_arg(raw: &[u8]) -> Result<Self, String> {
                std::str::from_utf8(raw)
                    .map_err(|e| e.to_string())?
                    .trim()
                    .parse()
                    .map_err(|e| format!("{e}"))
            }
        }

        impl ContractReturn for $t {
            fn into_payload(self) -> Result<Vec<u8>, String> {
                Ok(self.to_string().into_bytes())
            }
        }
    )+};
}

numeric_contract_types!("integer" => u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);
numeric_contract_types!("number" => f32, f64);

/// Unit return: empty payload, no `returns` metadata.
impl ContractSchema for () {
    fn schema() -> Value {
        Value::Null
    }
}

impl ContractReturn for () {
    fn into_payload(self) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
}

impl<T: ContractSchema> ContractSchema for Vec<T> {
    fn schema() -> Value {
        json!({"type": "array", "items": T::schema()})
    }

    fn components(out: &mut BTreeMap<String, Value>) {
        T::components(out);
    }
}

impl<T: ContractSchema + serde::de::DeserializeOwned> ContractArg for Vec<T> {
    fn from_arg(raw: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(raw).map_err(|e| format!("invalid JSON array: {e}"))
    }
}

impl<T: ContractSchema + serde::Serialize> ContractReturn for Vec<T> {
    fn into_payload(self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(&self).map_err(|e| e.to_string())
    }
}
