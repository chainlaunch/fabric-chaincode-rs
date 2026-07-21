//! Hyperledger Fabric chaincode shim for Rust — chaincode-as-a-service
//! (CCaaS) only, targeting Fabric 3.x peers.
//!
//! The chaincode runs as a gRPC *server*; the peer dials in and drives the
//! shim protocol over a bidirectional `ChaincodeMessage` stream. This is the
//! deployment model ChainLaunch uses for all Fabric chaincode.
//!
//! ```no_run
//! use fabric_shim::{Chaincode, ChaincodeStub, Response, Server};
//!
//! #[derive(Default)]
//! struct MyContract;
//!
//! #[fabric_shim::async_trait]
//! impl Chaincode for MyContract {
//!     async fn invoke(&self, stub: ChaincodeStub) -> Response {
//!         let (function, _args) = stub.get_function_and_args();
//!         match function.as_str() {
//!             "Ping" => Response::success(b"pong".to_vec()),
//!             other => Response::error(format!("unknown function {other}")),
//!         }
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() -> Result<(), fabric_shim::Error> {
//!     Server::from_env()?.start(MyContract).await
//! }
//! ```

// Lets macro-generated `fabric_shim::` paths resolve inside this crate too.
extern crate self as fabric_shim;

pub mod contract;
mod error;
mod handler;
mod iterators;
pub mod metadata;
mod response;
mod server;
mod stub;

pub use async_trait::async_trait;
pub use error::{Error, Result};
/// Contract API macros: `#[contract]` on an impl block, `#[derive(DataType)]`
/// on parameter/return structs. See [`mod@contract`] for a full example.
pub use fabric_shim_macros::{contract, DataType};
pub use iterators::{HistoryQueryIterator, StateQueryIterator};
pub use response::{Response, ERROR, ERROR_THRESHOLD, OK};
/// Re-exported for macro-generated code; also handy for chaincode authors.
pub use serde_json;
pub use server::{Server, DEFAULT_MAX_MESSAGE_SIZE};
pub use stub::{create_composite_key, split_composite_key, ChaincodeStub};

/// Re-exported generated protos for advanced use (decoding creator
/// identities, query results, proposals).
pub use fabric_shim_protos as protos;

/// A chaincode implementation. `invoke` handles every transaction; `init`
/// defaults to delegating to `invoke` and only matters for legacy
/// `--init-required` deployments.
#[async_trait]
pub trait Chaincode: Send + Sync + 'static {
    /// Contract metadata served for `org.hyperledger.fabric:GetMetadata`
    /// (the system function `fabric-contract-api-go`/`-node` expose, used by
    /// ChainLaunch's metadata endpoint and UI explorer). Return `Some` and
    /// the shim answers that function itself, before `invoke` is reached;
    /// with the default `None` the call falls through to `invoke`.
    fn metadata(&self) -> Option<metadata::Metadata> {
        None
    }

    async fn init(&self, stub: ChaincodeStub) -> Response {
        self.invoke(stub).await
    }

    async fn invoke(&self, stub: ChaincodeStub) -> Response;
}
