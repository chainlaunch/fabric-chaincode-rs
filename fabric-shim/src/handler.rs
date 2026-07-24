use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use fabric_shim_protos::peer as pb;
use fabric_shim_protos::peer::chaincode_message::Type as MsgType;
use prost::Message;
use tokio::sync::{mpsc, oneshot};
use tonic::{Status, Streaming};
use tracing::{debug, error, warn};

use crate::response::Response;
use crate::stub::ChaincodeStub;
use crate::{Chaincode, Error, Result};

/// Correlates peer responses to in-flight ledger requests: `(channel_id, txid)`.
pub(crate) type TxKey = (String, String);

/// Shared handle from stubs/iterators back to the peer connection.
#[derive(Clone)]
pub(crate) struct PeerLink {
    out: mpsc::Sender<std::result::Result<pb::ChaincodeMessage, Status>>,
    pending: Arc<Mutex<HashMap<TxKey, oneshot::Sender<pb::ChaincodeMessage>>>>,
}

impl PeerLink {
    /// Send a ledger request and await the peer's RESPONSE/ERROR for this tx.
    ///
    /// The caller (stub) guarantees only one request per tx is in flight.
    pub(crate) async fn request(
        &self,
        msg_type: MsgType,
        payload: Vec<u8>,
        channel_id: &str,
        tx_id: &str,
    ) -> Result<pb::ChaincodeMessage> {
        let key: TxKey = (channel_id.to_string(), tx_id.to_string());
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.contains_key(&key) {
                return Err(Error::Protocol(format!(
                    "another ledger request is already in flight for tx {tx_id}"
                )));
            }
            pending.insert(key, tx);
        }

        let msg = new_message(msg_type, payload, channel_id, tx_id);
        if self.out.send(Ok(msg)).await.is_err() {
            self.drop_pending(&(channel_id.to_string(), tx_id.to_string()));
            return Err(Error::ConnectionClosed);
        }

        let reply = rx.await.map_err(|_| Error::ConnectionClosed)?;
        match reply.r#type() {
            MsgType::Response => Ok(reply),
            MsgType::Error => Err(Error::Peer(
                String::from_utf8_lossy(&reply.payload).into_owned(),
            )),
            other => Err(Error::Protocol(format!(
                "unexpected message type {other:?} routed as tx response"
            ))),
        }
    }

    /// Fire-and-forget message (COMPLETED, ERROR, KEEPALIVE echo).
    pub(crate) async fn send(&self, msg: pb::ChaincodeMessage) -> Result<()> {
        self.out
            .send(Ok(msg))
            .await
            .map_err(|_| Error::ConnectionClosed)
    }

    fn route_response(&self, msg: pb::ChaincodeMessage) {
        let key: TxKey = (msg.channel_id.clone(), msg.txid.clone());
        let sender = self.pending.lock().unwrap().remove(&key);
        match sender {
            Some(tx) => {
                let _ = tx.send(msg);
            }
            None => warn!(
                txid = %key.1,
                channel = %key.0,
                "peer response with no in-flight request; dropping"
            ),
        }
    }

    fn drop_pending(&self, key: &TxKey) {
        self.pending.lock().unwrap().remove(key);
    }
}

pub(crate) fn new_message(
    msg_type: MsgType,
    payload: Vec<u8>,
    channel_id: &str,
    tx_id: &str,
) -> pb::ChaincodeMessage {
    pb::ChaincodeMessage {
        r#type: msg_type as i32,
        timestamp: Some(now()),
        payload,
        txid: tx_id.to_string(),
        proposal: None,
        chaincode_event: None,
        channel_id: channel_id.to_string(),
    }
}

fn now() -> prost_types::Timestamp {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    prost_types::Timestamp {
        seconds: d.as_secs() as i64,
        nanos: d.subsec_nanos() as i32,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnState {
    Created,
    Established,
    Ready,
}

/// Drive one peer connection: send REGISTER, run the handshake, then dispatch
/// transactions and route responses until the stream ends.
pub(crate) async fn run_connection(
    chaincode: Arc<dyn Chaincode>,
    chaincode_id: String,
    mut inbound: Streaming<pb::ChaincodeMessage>,
    out: mpsc::Sender<std::result::Result<pb::ChaincodeMessage, Status>>,
) {
    let link = PeerLink {
        out,
        pending: Arc::new(Mutex::new(HashMap::new())),
    };

    // Registration is chaincode-initiated even in CCaaS mode: the peer dialed
    // us, but the first message on the stream is our REGISTER.
    let register = pb::ChaincodeMessage {
        payload: pb::ChaincodeId {
            name: chaincode_id.clone(),
            ..Default::default()
        }
        .encode_to_vec(),
        ..new_message(MsgType::Register, Vec::new(), "", "")
    };
    if link.send(register).await.is_err() {
        return;
    }
    debug!(chaincode_id = %chaincode_id, "sent REGISTER");

    let mut state = ConnState::Created;
    loop {
        let msg = match inbound.message().await {
            Ok(Some(msg)) => msg,
            Ok(None) => {
                debug!("peer closed the stream");
                return;
            }
            Err(status) => {
                warn!(%status, "peer stream error");
                return;
            }
        };

        match (state, msg.r#type()) {
            (ConnState::Created, MsgType::Registered) => {
                debug!("REGISTERED received");
                state = ConnState::Established;
            }
            (ConnState::Established, MsgType::Ready) => {
                debug!("READY received");
                state = ConnState::Ready;
            }
            (_, MsgType::Keepalive) => {
                // Echo keepalives in any state.
                let _ = link
                    .send(new_message(
                        MsgType::Keepalive,
                        Vec::new(),
                        &msg.channel_id,
                        &msg.txid,
                    ))
                    .await;
            }
            (ConnState::Ready, MsgType::Init | MsgType::Transaction) => {
                spawn_transaction(chaincode.clone(), link.clone(), msg);
            }
            (ConnState::Ready, MsgType::Response | MsgType::Error) => {
                link.route_response(msg);
            }
            (_, other) => {
                error!(state = ?state, msg_type = ?other, "invalid message for state; closing connection");
                let _ = link
                    .send(pb::ChaincodeMessage {
                        payload: format!("received {other:?} in state {state:?}").into_bytes(),
                        ..new_message(MsgType::Error, Vec::new(), &msg.channel_id, &msg.txid)
                    })
                    .await;
                return;
            }
        }
    }
}

/// Handle one INIT/TRANSACTION message on its own task so transactions run
/// concurrently and a panicking chaincode cannot take down the connection.
fn spawn_transaction(chaincode: Arc<dyn Chaincode>, link: PeerLink, msg: pb::ChaincodeMessage) {
    tokio::spawn(async move {
        let is_init = msg.r#type() == MsgType::Init;
        let channel_id = msg.channel_id.clone();
        let tx_id = msg.txid.clone();
        let tx_key: TxKey = (channel_id.clone(), tx_id.clone());

        let input = match pb::ChaincodeInput::decode(msg.payload.as_ref()) {
            Ok(input) => input,
            Err(e) => {
                let _ = link
                    .send(pb::ChaincodeMessage {
                        payload: format!("failed to decode ChaincodeInput: {e}").into_bytes(),
                        ..new_message(MsgType::Error, Vec::new(), &channel_id, &tx_id)
                    })
                    .await;
                return;
            }
        };

        // Serve the GetMetadata system function from the shim itself (as the
        // Go/Node contract API layers do) when the chaincode declares
        // metadata; otherwise it falls through to the user's invoke.
        if input.args.first().map(|a| a.as_slice())
            == Some(crate::metadata::METADATA_FUNCTION.as_bytes())
        {
            if let Some(md) = chaincode.metadata() {
                let response = match serde_json::to_vec(&md) {
                    Ok(json) => Response::success(json),
                    Err(e) => Response::error(format!("failed to serialize metadata: {e}")),
                };
                let completed = pb::ChaincodeMessage {
                    payload: response.into_proto().encode_to_vec(),
                    ..new_message(MsgType::Completed, Vec::new(), &channel_id, &tx_id)
                };
                let _ = link.send(completed).await;
                return;
            }
        }

        let stub = ChaincodeStub::new(
            link.clone(),
            channel_id.clone(),
            tx_id.clone(),
            input,
            msg.proposal,
        );
        let stub = match stub {
            Ok(stub) => stub,
            Err(e) => {
                let _ = link
                    .send(pb::ChaincodeMessage {
                        payload: format!("failed to build stub: {e}").into_bytes(),
                        ..new_message(MsgType::Error, Vec::new(), &channel_id, &tx_id)
                    })
                    .await;
                return;
            }
        };
        let event_slot = stub.event_slot();

        // Isolate user code in its own task so a panic surfaces as a
        // JoinError instead of unwinding through the connection handler.
        let response = match tokio::spawn(async move {
            if is_init {
                chaincode.init(stub).await
            } else {
                chaincode.invoke(stub).await
            }
        })
        .await
        {
            Ok(resp) => resp,
            Err(join_err) => {
                error!(txid = %tx_id, "chaincode handler panicked: {join_err}");
                Response::error(format!("chaincode handler panicked: {join_err}"))
            }
        };

        // A tx that never got its ledger reply must not leak a pending entry.
        link.drop_pending(&tx_key);

        let event = event_slot.lock().unwrap().take();
        let completed = pb::ChaincodeMessage {
            payload: response.into_proto().encode_to_vec(),
            chaincode_event: event,
            ..new_message(MsgType::Completed, Vec::new(), &channel_id, &tx_id)
        };
        let _ = link.send(completed).await;
    });
}
