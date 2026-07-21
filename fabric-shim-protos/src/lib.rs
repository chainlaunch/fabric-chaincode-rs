//! Generated protobuf/gRPC bindings for the Hyperledger Fabric chaincode
//! shim protocol, vendored from `hyperledger/fabric-protos` v0.3.7 (Fabric 3.x).
//!
//! Module names follow the protobuf package names, except the Fabric peer
//! protos, whose package is `protos` but which are exposed here as [`peer`].

/// Fabric `common` package: block/envelope headers, policies, MSP principals.
pub mod common {
    tonic::include_proto!("common");
}

/// Fabric `msp` package: serialized identities.
pub mod msp {
    tonic::include_proto!("msp");
}

/// Fabric `queryresult` package: KV and history query records.
pub mod queryresult {
    tonic::include_proto!("queryresult");
}

/// Fabric peer package (`protos`): chaincode shim messages and services.
pub mod peer {
    tonic::include_proto!("protos");
}

pub use prost;
pub use prost_types;
