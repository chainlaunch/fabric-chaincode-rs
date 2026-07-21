use std::net::SocketAddr;
use std::sync::Arc;

use fabric_shim_protos::peer as pb;
use fabric_shim_protos::peer::chaincode_server::{
    Chaincode as GrpcChaincode, ChaincodeServer as GrpcChaincodeServer,
};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Status, Streaming};
use tracing::info;

use crate::handler::run_connection;
use crate::{Chaincode, Error, Result};

/// Default max gRPC message size, matching the Fabric peer (100 MiB).
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 100 * 1024 * 1024;

/// CCaaS chaincode server: listens for peer connections and runs the shim
/// protocol for each.
pub struct Server {
    chaincode_id: String,
    address: SocketAddr,
    max_message_size: usize,
}

impl Server {
    /// Configure from the environment ChainLaunch (and Fabric CCaaS builders)
    /// inject into the container:
    /// `CHAINCODE_ID`/`CORE_CHAINCODE_ID` and
    /// `CHAINCODE_SERVER_ADDRESS`/`CORE_CHAINCODE_ADDRESS` (default `0.0.0.0:7052`).
    pub fn from_env() -> Result<Self> {
        let chaincode_id = std::env::var("CHAINCODE_ID")
            .or_else(|_| std::env::var("CORE_CHAINCODE_ID"))
            .map_err(|_| Error::Config("CHAINCODE_ID (or CORE_CHAINCODE_ID) is not set".into()))?;
        let address = std::env::var("CHAINCODE_SERVER_ADDRESS")
            .or_else(|_| std::env::var("CORE_CHAINCODE_ADDRESS"))
            .unwrap_or_else(|_| "0.0.0.0:7052".to_string());
        Self::new(chaincode_id, &address)
    }

    /// `chaincode_id` is the installed package ID (e.g. `basic_1.0:abc123...`);
    /// `address` is a `host:port` to bind.
    pub fn new(chaincode_id: impl Into<String>, address: &str) -> Result<Self> {
        use std::net::ToSocketAddrs;
        let chaincode_id = chaincode_id.into();
        if chaincode_id.is_empty() {
            return Err(Error::Config("chaincode id must not be empty".into()));
        }
        let address = address
            .to_socket_addrs()
            .map_err(|e| Error::Config(format!("invalid address {address:?}: {e}")))?
            .next()
            .ok_or_else(|| Error::Config(format!("address {address:?} resolved to nothing")))?;
        Ok(Self {
            chaincode_id,
            address,
            max_message_size: DEFAULT_MAX_MESSAGE_SIZE,
        })
    }

    pub fn max_message_size(mut self, bytes: usize) -> Self {
        self.max_message_size = bytes;
        self
    }

    /// Serve until SIGTERM/SIGINT (docker stop) or ctrl-c.
    pub async fn start<C: Chaincode>(self, chaincode: C) -> Result<()> {
        let listener = TcpListener::bind(self.address)
            .await
            .map_err(|e| Error::Transport(format!("bind {}: {e}", self.address)))?;
        self.serve_with_listener(chaincode, listener, shutdown_signal())
            .await
    }

    /// Serve on an existing listener until `shutdown` resolves. Useful for
    /// tests (bind port 0, read the local addr, pass the listener in).
    pub async fn serve_with_listener<C: Chaincode>(
        self,
        chaincode: C,
        listener: TcpListener,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> Result<()> {
        let addr = listener.local_addr().ok();
        info!(chaincode_id = %self.chaincode_id, ?addr, "chaincode server listening");

        let service = ChaincodeService {
            chaincode: Arc::new(chaincode),
            chaincode_id: self.chaincode_id,
        };
        let grpc = GrpcChaincodeServer::new(service)
            .max_decoding_message_size(self.max_message_size)
            .max_encoding_message_size(self.max_message_size);

        tonic::transport::Server::builder()
            .add_service(grpc)
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), shutdown)
            .await
            .map_err(|e| Error::Transport(e.to_string()))
    }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => return std::future::pending().await,
    };
    tokio::select! {
        _ = sigterm.recv() => info!("SIGTERM received, shutting down"),
        _ = tokio::signal::ctrl_c() => info!("interrupt received, shutting down"),
    }
}

struct ChaincodeService {
    chaincode: Arc<dyn Chaincode>,
    chaincode_id: String,
}

#[tonic::async_trait]
impl GrpcChaincode for ChaincodeService {
    type ConnectStream = ReceiverStream<std::result::Result<pb::ChaincodeMessage, Status>>;

    async fn connect(
        &self,
        request: Request<Streaming<pb::ChaincodeMessage>>,
    ) -> std::result::Result<tonic::Response<Self::ConnectStream>, Status> {
        info!(peer = ?request.remote_addr(), "peer connected");
        let inbound = request.into_inner();
        let (out_tx, out_rx) = mpsc::channel(64);
        tokio::spawn(run_connection(
            self.chaincode.clone(),
            self.chaincode_id.clone(),
            inbound,
            out_tx,
        ));
        Ok(tonic::Response::new(ReceiverStream::new(out_rx)))
    }
}
