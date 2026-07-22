//! Generated protobuf/gRPC bindings for the Hyperledger Fabric chaincode
//! shim protocol, vendored from `hyperledger/fabric-protos` v0.3.7 (Fabric 3.x).
//!
//! Module names follow the protobuf package names, except the Fabric peer
//! protos, whose package is `protos` but which are exposed here as [`peer`].
//!
//! The bindings are committed under `src/generated/` and used by default, so
//! building this crate never requires `protoc`. Enable the
//! `regenerate-protos` feature (and have `protoc` + the well-known types
//! installed) only when the vendored `.proto` files under
//! `fabric-shim-protos/protos/` change; then copy the freshly generated
//! `OUT_DIR` files over `src/generated/` and commit the diff.

#![forbid(unsafe_code)]

/// Fabric `common` package: block/envelope headers, policies, MSP principals.
pub mod common {
    #[cfg(feature = "regenerate-protos")]
    tonic::include_proto!("common");
    #[cfg(not(feature = "regenerate-protos"))]
    include!("generated/common.rs");
}

/// Fabric `msp` package: serialized identities.
pub mod msp {
    #[cfg(feature = "regenerate-protos")]
    tonic::include_proto!("msp");
    #[cfg(not(feature = "regenerate-protos"))]
    include!("generated/msp.rs");
}

/// Fabric `queryresult` package: KV and history query records.
pub mod queryresult {
    #[cfg(feature = "regenerate-protos")]
    tonic::include_proto!("queryresult");
    #[cfg(not(feature = "regenerate-protos"))]
    include!("generated/queryresult.rs");
}

/// Fabric peer package (`protos`): chaincode shim messages and services.
pub mod peer {
    #[cfg(feature = "regenerate-protos")]
    tonic::include_proto!("protos");
    #[cfg(not(feature = "regenerate-protos"))]
    include!("generated/protos.rs");
}

pub use prost;
pub use prost_types;
