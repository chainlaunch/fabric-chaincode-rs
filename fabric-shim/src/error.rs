/// Errors surfaced by the shim to user chaincode and server callers.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The peer answered a ledger request with an ERROR message.
    #[error("peer error: {0}")]
    Peer(String),

    /// The shim protocol was violated (unexpected message type/state).
    #[error("protocol error: {0}")]
    Protocol(String),

    /// A protobuf payload could not be decoded.
    #[error("decode error: {0}")]
    Decode(#[from] prost::DecodeError),

    /// The peer stream ended while a request was in flight.
    #[error("connection to peer closed")]
    ConnectionClosed,

    /// Invalid server configuration (env vars, addresses).
    #[error("configuration error: {0}")]
    Config(String),

    /// Invalid argument passed to a stub method (e.g. bad composite key).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// Transport-level failure while serving.
    #[error("transport error: {0}")]
    Transport(String),
}

pub type Result<T> = std::result::Result<T, Error>;
