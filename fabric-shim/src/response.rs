use fabric_shim_protos::peer as pb;

/// Status code for a successful transaction (mirrors Go shim `shim.OK`).
pub const OK: i32 = 200;
/// Status codes >= this threshold are treated as errors by the peer.
pub const ERROR_THRESHOLD: i32 = 400;
/// Status code for a failed transaction (mirrors Go shim `shim.ERROR`).
pub const ERROR: i32 = 500;

/// The result of a chaincode transaction, serialized into the peer's
/// `Response` protobuf on COMPLETED.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Response {
    pub status: i32,
    pub message: String,
    pub payload: Vec<u8>,
}

impl Response {
    /// A 200 response carrying a payload.
    pub fn success(payload: impl Into<Vec<u8>>) -> Self {
        Self {
            status: OK,
            message: String::new(),
            payload: payload.into(),
        }
    }

    /// A 200 response with no payload.
    pub fn success_empty() -> Self {
        Self::success(Vec::new())
    }

    /// A 500 response with an error message.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            status: ERROR,
            message: message.into(),
            payload: Vec::new(),
        }
    }

    /// Whether the peer will treat this response as an error.
    pub fn is_error(&self) -> bool {
        self.status >= ERROR_THRESHOLD
    }

    pub(crate) fn into_proto(self) -> pb::Response {
        pb::Response {
            status: self.status,
            message: self.message,
            payload: self.payload,
        }
    }

    pub(crate) fn from_proto(p: pb::Response) -> Self {
        Self {
            status: p.status,
            message: p.message,
            payload: p.payload,
        }
    }
}

impl<E: std::fmt::Display> From<std::result::Result<Response, E>> for Response {
    fn from(r: std::result::Result<Response, E>) -> Self {
        match r {
            Ok(resp) => resp,
            Err(e) => Response::error(e.to_string()),
        }
    }
}
