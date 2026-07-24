//! Integration tests driving the shim with an in-process mock peer.
//!
//! The mock peer is a real gRPC client (as the Fabric peer is in CCaaS mode):
//! it dials the shim server, completes the REGISTER/REGISTERED/READY
//! handshake, then serves ledger requests from an in-memory KV store.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::time::Duration;

use fabric_shim::protos::peer as pb;
use fabric_shim::protos::peer::chaincode_client::ChaincodeClient;
use fabric_shim::protos::peer::chaincode_message::Type as MsgType;
use fabric_shim::protos::{common as pb_common, msp as pb_msp, queryresult};
use fabric_shim::{async_trait, Chaincode, ChaincodeStub, Response, Server};
use prost::Message;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

const CHAINCODE_ID: &str = "testcc_1.0:cafebabe";
const RECV_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Test chaincode
// ---------------------------------------------------------------------------

#[derive(Default)]
struct TestKv;

#[async_trait]
impl Chaincode for TestKv {
    async fn invoke(&self, stub: ChaincodeStub) -> Response {
        let (function, args) = stub.get_function_and_args();
        let result: Result<Response, fabric_shim::Error> = match function.as_str() {
            "put" => stub
                .put_state(&args[0], args[1].as_bytes().to_vec())
                .await
                .map(|_| Response::success_empty()),
            "get" => stub.get_state(&args[0]).await.map(Response::success),
            "del" => stub
                .del_state(&args[0])
                .await
                .map(|_| Response::success_empty()),
            "range" => match stub.get_state_by_range("", "").await {
                Ok(iter) => iter.collect_remaining().await.map(|kvs| {
                    let keys: Vec<String> = kvs.into_iter().map(|kv| kv.key).collect();
                    Response::success(keys.join(",").into_bytes())
                }),
                Err(e) => Err(e),
            },
            "event" => stub
                .set_event("AssetTransferred", b"asset1".to_vec())
                .map(|_| Response::success_empty()),
            "put_pd" => stub
                .put_private_data(&args[0], &args[1], args[2].as_bytes().to_vec())
                .await
                .map(|_| Response::success_empty()),
            "get_pd" => stub
                .get_private_data(&args[0], &args[1])
                .await
                .map(Response::success),
            "del_pd" => stub
                .del_private_data(&args[0], &args[1])
                .await
                .map(|_| Response::success_empty()),
            "purge_pd" => stub
                .purge_private_data(&args[0], &args[1])
                .await
                .map(|_| Response::success_empty()),
            "pd_hash" => stub
                .get_private_data_hash(&args[0], &args[1])
                .await
                .map(Response::success),
            "pd_range" => match stub.get_private_data_by_range(&args[0], "", "").await {
                Ok(iter) => iter.collect_remaining().await.map(|kvs| {
                    let keys: Vec<String> = kvs.into_iter().map(|kv| kv.key).collect();
                    Response::success(keys.join(",").into_bytes())
                }),
                Err(e) => Err(e),
            },
            "pkey_put" => {
                match stub.create_composite_key(&args[0], &[args[1].as_str(), args[2].as_str()]) {
                    Ok(key) => stub
                        .put_state(&key, args[3].as_bytes().to_vec())
                        .await
                        .map(|_| Response::success_empty()),
                    Err(e) => Err(e),
                }
            }
            "pkey_range" => match stub
                .get_state_by_partial_composite_key(&args[0], &[args[1].as_str()])
                .await
            {
                Ok(iter) => iter.collect_remaining().await.map(|kvs| {
                    let parts: Result<Vec<String>, fabric_shim::Error> = kvs
                        .into_iter()
                        .map(|kv| {
                            stub.split_composite_key(&kv.key)
                                .map(|(_, attrs)| attrs.join("|"))
                        })
                        .collect();
                    match parts {
                        Ok(parts) => Response::success(parts.join(",").into_bytes()),
                        Err(e) => Response::error(e.to_string()),
                    }
                }),
                Err(e) => Err(e),
            },
            "transient" => Ok(Response::success(
                stub.get_transient()
                    .get("secret")
                    .cloned()
                    .unwrap_or_default(),
            )),
            "whoami" => stub
                .get_creator_identity()
                .map(|id| Response::success(id.mspid.into_bytes())),
            "ts" => stub
                .get_tx_timestamp()
                .map(|ts| Response::success(ts.seconds.to_string().into_bytes())),
            "panic" => panic!("intentional test panic"),
            other => Ok(Response::error(format!("unknown function {other}"))),
        };
        result.into()
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

async fn start_shim() -> (SocketAddr, oneshot::Sender<()>) {
    start_shim_with(TestKv).await
}

async fn start_shim_with<C: Chaincode>(chaincode: C) -> (SocketAddr, oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let server = Server::new(CHAINCODE_ID, &addr.to_string()).unwrap();
    tokio::spawn(server.serve_with_listener(chaincode, listener, async {
        let _ = stop_rx.await;
    }));
    (addr, stop_tx)
}

struct MockPeer {
    to_shim: mpsc::Sender<pb::ChaincodeMessage>,
    from_shim: tonic::Streaming<pb::ChaincodeMessage>,
    state: BTreeMap<String, Vec<u8>>,
    /// Private data collections: (collection, key) -> value. A separate map
    /// (not just a namespaced key in `state`) mirrors a real peer, where
    /// private data collections are isolated key spaces from world state and
    /// from each other.
    private_state: BTreeMap<(String, String), Vec<u8>>,
    /// Open iterators: id -> remaining pre-encoded results.
    iterators: HashMap<String, Vec<Vec<u8>>>,
    next_iter: u64,
    /// Records per QueryResponse batch (small to force QUERY_STATE_NEXT).
    range_batch: usize,
    /// Counters asserted by tests.
    state_next_calls: usize,
    state_close_calls: usize,
}

impl MockPeer {
    /// Dial the shim and complete the handshake, asserting the REGISTER.
    async fn connect(addr: SocketAddr) -> Self {
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = ChaincodeClient::new(channel);
        let (to_shim, rx) = mpsc::channel(16);
        let from_shim = client
            .connect(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();

        let mut peer = Self {
            to_shim,
            from_shim,
            state: BTreeMap::new(),
            private_state: BTreeMap::new(),
            iterators: HashMap::new(),
            next_iter: 0,
            range_batch: 2,
            state_next_calls: 0,
            state_close_calls: 0,
        };

        let register = peer.recv().await;
        assert_eq!(register.r#type(), MsgType::Register);
        let id = pb::ChaincodeId::decode(register.payload.as_ref()).unwrap();
        assert_eq!(id.name, CHAINCODE_ID);

        peer.send(MsgType::Registered, Vec::new(), "", "").await;
        peer.send(MsgType::Ready, Vec::new(), "", "").await;
        peer
    }

    /// Dial without completing the handshake (for pre-READY tests).
    async fn connect_raw(addr: SocketAddr) -> Self {
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = ChaincodeClient::new(channel);
        let (to_shim, rx) = mpsc::channel(16);
        let from_shim = client
            .connect(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();
        Self {
            to_shim,
            from_shim,
            state: BTreeMap::new(),
            private_state: BTreeMap::new(),
            iterators: HashMap::new(),
            next_iter: 0,
            range_batch: 2,
            state_next_calls: 0,
            state_close_calls: 0,
        }
    }

    async fn send(&self, ty: MsgType, payload: Vec<u8>, channel_id: &str, txid: &str) {
        self.to_shim
            .send(pb::ChaincodeMessage {
                r#type: ty as i32,
                payload,
                txid: txid.to_string(),
                channel_id: channel_id.to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
    }

    async fn recv(&mut self) -> pb::ChaincodeMessage {
        tokio::time::timeout(RECV_TIMEOUT, self.from_shim.message())
            .await
            .expect("timed out waiting for shim message")
            .expect("stream error")
            .expect("stream closed unexpectedly")
    }

    async fn recv_end(&mut self) -> Option<pb::ChaincodeMessage> {
        tokio::time::timeout(RECV_TIMEOUT, self.from_shim.message())
            .await
            .expect("timed out waiting for stream end")
            .expect("stream error")
    }

    async fn start_tx(&self, txid: &str, args: &[&str]) {
        self.start_tx_with_proposal(txid, args, None).await;
    }

    async fn start_tx_with_proposal(
        &self,
        txid: &str,
        args: &[&str],
        proposal: Option<pb::SignedProposal>,
    ) {
        let input = pb::ChaincodeInput {
            args: args.iter().map(|a| a.as_bytes().to_vec()).collect(),
            decorations: HashMap::new(),
            is_init: false,
        };
        self.to_shim
            .send(pb::ChaincodeMessage {
                r#type: MsgType::Transaction as i32,
                payload: input.encode_to_vec(),
                txid: txid.to_string(),
                channel_id: "testchannel".to_string(),
                proposal,
                ..Default::default()
            })
            .await
            .unwrap();
    }

    /// Answer one ledger request from the in-memory store.
    async fn serve_request(&mut self, msg: pb::ChaincodeMessage) {
        let (channel_id, txid) = (msg.channel_id.clone(), msg.txid.clone());
        let reply: Vec<u8> = match msg.r#type() {
            MsgType::GetState => {
                let req = pb::GetState::decode(msg.payload.as_ref()).unwrap();
                if req.collection.is_empty() {
                    self.state.get(&req.key).cloned().unwrap_or_default()
                } else {
                    self.private_state
                        .get(&(req.collection, req.key))
                        .cloned()
                        .unwrap_or_default()
                }
            }
            // Real peers return sha256(value) here; the shim only relays
            // whatever bytes the peer sends back, so proving wire
            // correctness doesn't need a real hash -- echo the stored value.
            MsgType::GetPrivateDataHash => {
                let req = pb::GetState::decode(msg.payload.as_ref()).unwrap();
                self.private_state
                    .get(&(req.collection, req.key))
                    .cloned()
                    .unwrap_or_default()
            }
            MsgType::PutState => {
                let req = pb::PutState::decode(msg.payload.as_ref()).unwrap();
                if req.collection.is_empty() {
                    self.state.insert(req.key, req.value.to_vec());
                } else {
                    self.private_state
                        .insert((req.collection, req.key), req.value.to_vec());
                }
                Vec::new()
            }
            MsgType::DelState => {
                let req = pb::DelState::decode(msg.payload.as_ref()).unwrap();
                if req.collection.is_empty() {
                    self.state.remove(&req.key);
                } else {
                    self.private_state.remove(&(req.collection, req.key));
                }
                Vec::new()
            }
            MsgType::PurgePrivateData => {
                let req = pb::PurgePrivateState::decode(msg.payload.as_ref()).unwrap();
                self.private_state.remove(&(req.collection, req.key));
                Vec::new()
            }
            MsgType::GetStateByRange => {
                let req = pb::GetStateByRange::decode(msg.payload.as_ref()).unwrap();
                let in_range = |k: &str| {
                    (req.start_key.is_empty() || k >= req.start_key.as_str())
                        && (req.end_key.is_empty() || k < req.end_key.as_str())
                };
                let results: Vec<Vec<u8>> = if req.collection.is_empty() {
                    self.state
                        .iter()
                        .filter(|(k, _)| in_range(k))
                        .map(|(k, v)| {
                            queryresult::Kv {
                                namespace: "testcc".into(),
                                key: k.clone(),
                                value: v.clone(),
                            }
                            .encode_to_vec()
                        })
                        .collect()
                } else {
                    self.private_state
                        .iter()
                        .filter(|((c, k), _)| *c == req.collection && in_range(k))
                        .map(|((_, k), v)| {
                            queryresult::Kv {
                                namespace: "testcc".into(),
                                key: k.clone(),
                                value: v.clone(),
                            }
                            .encode_to_vec()
                        })
                        .collect()
                };
                self.next_iter += 1;
                let id = format!("iter-{}", self.next_iter);
                self.query_response(id, results)
            }
            MsgType::QueryStateNext => {
                self.state_next_calls += 1;
                let req = pb::QueryStateNext::decode(msg.payload.as_ref()).unwrap();
                let remaining = self.iterators.remove(&req.id).unwrap_or_default();
                self.query_response(req.id, remaining)
            }
            MsgType::QueryStateClose => {
                self.state_close_calls += 1;
                let req = pb::QueryStateClose::decode(msg.payload.as_ref()).unwrap();
                self.iterators.remove(&req.id);
                Vec::new()
            }
            other => panic!("mock peer cannot serve request type {other:?}"),
        };
        self.send(MsgType::Response, reply, &channel_id, &txid)
            .await;
    }

    /// Build a QueryResponse batch of up to `range_batch` records, stashing
    /// the remainder under the iterator id.
    fn query_response(&mut self, id: String, mut results: Vec<Vec<u8>>) -> Vec<u8> {
        let batch: Vec<pb::QueryResultBytes> = results
            .drain(..results.len().min(self.range_batch))
            .map(|b| pb::QueryResultBytes { result_bytes: b })
            .collect();
        let has_more = !results.is_empty();
        if has_more {
            self.iterators.insert(id.clone(), results);
        }
        pb::QueryResponse {
            results: batch,
            has_more,
            id,
            metadata: Default::default(),
        }
        .encode_to_vec()
    }

    /// Serve ledger requests until this tx's COMPLETED arrives.
    async fn run_tx_to_completion(&mut self, txid: &str) -> (Response, Option<pb::ChaincodeEvent>) {
        loop {
            let msg = self.recv().await;
            match msg.r#type() {
                MsgType::Completed => {
                    assert_eq!(msg.txid, txid, "COMPLETED for unexpected tx");
                    let proto = pb::Response::decode(msg.payload.as_ref()).unwrap();
                    return (
                        Response {
                            status: proto.status,
                            message: proto.message,
                            payload: proto.payload.to_vec(),
                        },
                        msg.chaincode_event,
                    );
                }
                MsgType::Error => panic!("shim sent ERROR: {:?}", msg.payload),
                _ => self.serve_request(msg).await,
            }
        }
    }
}

fn make_signed_proposal(
    mspid: &str,
    cert: &[u8],
    transient: HashMap<String, Vec<u8>>,
    ts_seconds: i64,
) -> pb::SignedProposal {
    let creator = pb_msp::SerializedIdentity {
        mspid: mspid.to_string(),
        id_bytes: cert.to_vec(),
    }
    .encode_to_vec();
    let header = pb_common::Header {
        channel_header: pb_common::ChannelHeader {
            timestamp: Some(prost_types::Timestamp {
                seconds: ts_seconds,
                nanos: 0,
            }),
            ..Default::default()
        }
        .encode_to_vec(),
        signature_header: pb_common::SignatureHeader {
            creator,
            nonce: Default::default(),
        }
        .encode_to_vec(),
    };
    let payload = pb::ChaincodeProposalPayload {
        input: Default::default(),
        transient_map: transient.into_iter().collect(),
    };
    let proposal = pb::Proposal {
        header: header.encode_to_vec(),
        payload: payload.encode_to_vec(),
        extension: Default::default(),
    };
    pb::SignedProposal {
        proposal_bytes: proposal.encode_to_vec(),
        signature: Default::default(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn handshake_registers_with_chaincode_id() {
    let (addr, _stop) = start_shim().await;
    // MockPeer::connect asserts REGISTER carries the configured package ID.
    let _peer = MockPeer::connect(addr).await;
}

#[tokio::test]
async fn keepalive_is_echoed() {
    let (addr, _stop) = start_shim().await;
    let mut peer = MockPeer::connect(addr).await;
    peer.send(MsgType::Keepalive, Vec::new(), "", "").await;
    let echo = peer.recv().await;
    assert_eq!(echo.r#type(), MsgType::Keepalive);
}

#[tokio::test]
async fn put_get_roundtrip() {
    let (addr, _stop) = start_shim().await;
    let mut peer = MockPeer::connect(addr).await;

    peer.start_tx("tx1", &["put", "asset1", "blue"]).await;
    let (resp, _) = peer.run_tx_to_completion("tx1").await;
    assert_eq!(resp.status, fabric_shim::OK, "{}", resp.message);
    assert_eq!(peer.state.get("asset1").unwrap(), b"blue");

    peer.start_tx("tx2", &["get", "asset1"]).await;
    let (resp, _) = peer.run_tx_to_completion("tx2").await;
    assert_eq!(resp.status, fabric_shim::OK);
    assert_eq!(resp.payload, b"blue");
}

#[tokio::test]
async fn peer_error_surfaces_as_500_completed() {
    let (addr, _stop) = start_shim().await;
    let mut peer = MockPeer::connect(addr).await;

    peer.start_tx("tx1", &["get", "asset1"]).await;
    let req = peer.recv().await;
    assert_eq!(req.r#type(), MsgType::GetState);
    peer.send(
        MsgType::Error,
        b"ledger on fire".to_vec(),
        &req.channel_id,
        &req.txid,
    )
    .await;

    let (resp, _) = peer.run_tx_to_completion("tx1").await;
    assert_eq!(resp.status, fabric_shim::ERROR);
    assert!(
        resp.message.contains("ledger on fire"),
        "message: {}",
        resp.message
    );
}

#[tokio::test]
async fn range_query_pages_through_batches() {
    let (addr, _stop) = start_shim().await;
    let mut peer = MockPeer::connect(addr).await;
    for i in 1..=5 {
        peer.state
            .insert(format!("k{i}"), format!("v{i}").into_bytes());
    }

    peer.start_tx("tx1", &["range"]).await;
    let (resp, _) = peer.run_tx_to_completion("tx1").await;
    assert_eq!(resp.status, fabric_shim::OK, "{}", resp.message);
    assert_eq!(resp.payload, b"k1,k2,k3,k4,k5");
    // 5 records in batches of 2 → initial response + 2 fetches, then a close.
    assert_eq!(peer.state_next_calls, 2);
    assert_eq!(peer.state_close_calls, 1);
}

#[tokio::test]
async fn interleaved_transactions_route_by_txid() {
    let (addr, _stop) = start_shim().await;
    let mut peer = MockPeer::connect(addr).await;
    peer.state.insert("a".into(), b"valA".to_vec());
    peer.state.insert("b".into(), b"valB".to_vec());

    peer.start_tx("txA", &["get", "a"]).await;
    peer.start_tx("txB", &["get", "b"]).await;

    // Collect both GET_STATE requests before answering either.
    let r1 = peer.recv().await;
    let r2 = peer.recv().await;
    assert_eq!(r1.r#type(), MsgType::GetState);
    assert_eq!(r2.r#type(), MsgType::GetState);
    let mut requests = vec![r1, r2];
    // Answer in reverse arrival order to prove routing is by txid.
    requests.reverse();
    for req in requests {
        let key = pb::GetState::decode(req.payload.as_ref()).unwrap().key;
        let value = peer.state.get(&key).cloned().unwrap();
        peer.send(MsgType::Response, value, &req.channel_id, &req.txid)
            .await;
    }

    let mut results = HashMap::new();
    for _ in 0..2 {
        let msg = peer.recv().await;
        assert_eq!(msg.r#type(), MsgType::Completed);
        let resp = pb::Response::decode(msg.payload.as_ref()).unwrap();
        results.insert(msg.txid.clone(), resp.payload.to_vec());
    }
    assert_eq!(results["txA"], b"valA");
    assert_eq!(results["txB"], b"valB");
}

#[tokio::test]
async fn panicking_chaincode_returns_500_and_stream_survives() {
    let (addr, _stop) = start_shim().await;
    let mut peer = MockPeer::connect(addr).await;

    peer.start_tx("tx1", &["panic"]).await;
    let (resp, _) = peer.run_tx_to_completion("tx1").await;
    assert_eq!(resp.status, fabric_shim::ERROR);
    assert!(resp.message.contains("panic"), "message: {}", resp.message);

    // The connection must still work after the panic.
    peer.start_tx("tx2", &["put", "k", "v"]).await;
    let (resp, _) = peer.run_tx_to_completion("tx2").await;
    assert_eq!(resp.status, fabric_shim::OK);
}

#[tokio::test]
async fn chaincode_event_rides_on_completed() {
    let (addr, _stop) = start_shim().await;
    let mut peer = MockPeer::connect(addr).await;

    peer.start_tx("tx1", &["event"]).await;
    let (resp, event) = peer.run_tx_to_completion("tx1").await;
    assert_eq!(resp.status, fabric_shim::OK);
    let event = event.expect("COMPLETED should carry the event");
    assert_eq!(event.event_name, "AssetTransferred");
    assert_eq!(event.payload, b"asset1".to_vec());
}

#[tokio::test]
async fn proposal_fields_are_decoded() {
    let (addr, _stop) = start_shim().await;
    let mut peer = MockPeer::connect(addr).await;

    let transient = HashMap::from([("secret".to_string(), b"hunter2".to_vec())]);
    let proposal = make_signed_proposal("Org1MSP", b"-----CERT-----", transient, 1_700_000_000);

    peer.start_tx_with_proposal("tx1", &["transient"], Some(proposal.clone()))
        .await;
    let (resp, _) = peer.run_tx_to_completion("tx1").await;
    assert_eq!(resp.payload, b"hunter2");

    peer.start_tx_with_proposal("tx2", &["whoami"], Some(proposal.clone()))
        .await;
    let (resp, _) = peer.run_tx_to_completion("tx2").await;
    assert_eq!(resp.payload, b"Org1MSP");

    peer.start_tx_with_proposal("tx3", &["ts"], Some(proposal))
        .await;
    let (resp, _) = peer.run_tx_to_completion("tx3").await;
    assert_eq!(resp.payload, b"1700000000");
}

#[tokio::test]
async fn transaction_before_ready_is_rejected() {
    let (addr, _stop) = start_shim().await;
    let mut peer = MockPeer::connect_raw(addr).await;

    let register = peer.recv().await;
    assert_eq!(register.r#type(), MsgType::Register);

    // Skip REGISTERED/READY and send a transaction straight away.
    peer.start_tx("tx1", &["get", "a"]).await;
    let msg = peer.recv().await;
    assert_eq!(msg.r#type(), MsgType::Error);
    // The shim closes the connection after the protocol violation.
    assert!(peer.recv_end().await.is_none());
}

/// TestKv plus declared metadata, for GetMetadata interception tests.
struct MetaKv;

#[async_trait]
impl Chaincode for MetaKv {
    fn metadata(&self) -> Option<fabric_shim::metadata::Metadata> {
        use fabric_shim::metadata::{Contract, Metadata, Transaction};
        Some(
            Metadata::new("meta-kv", "1.2.3").contract(
                Contract::new("MetaKv")
                    .transaction(
                        Transaction::submit("put")
                            .parameter("key", serde_json::json!({"type": "string"}))
                            .parameter("value", serde_json::json!({"type": "string"})),
                    )
                    .transaction(
                        Transaction::evaluate("get")
                            .parameter("key", serde_json::json!({"type": "string"})),
                    ),
            ),
        )
    }

    async fn invoke(&self, stub: ChaincodeStub) -> Response {
        TestKv.invoke(stub).await
    }
}

#[tokio::test]
async fn get_metadata_served_by_shim() {
    let (addr, _stop) = start_shim_with(MetaKv).await;
    let mut peer = MockPeer::connect(addr).await;

    peer.start_tx("tx1", &["org.hyperledger.fabric:GetMetadata"])
        .await;
    let (resp, _) = peer.run_tx_to_completion("tx1").await;
    assert_eq!(resp.status, fabric_shim::OK, "{}", resp.message);

    let md: serde_json::Value = serde_json::from_slice(&resp.payload).unwrap();
    assert_eq!(md["info"]["title"], "meta-kv");
    assert_eq!(md["info"]["version"], "1.2.3");
    assert_eq!(md["contracts"]["MetaKv"]["transactions"][0]["name"], "put");
    assert_eq!(
        md["contracts"]["MetaKv"]["transactions"][0]["tag"][0],
        "submit"
    );
    assert_eq!(
        md["contracts"]["MetaKv"]["transactions"][1]["parameters"][0]["schema"]["type"],
        "string"
    );

    // The interception must not shadow ordinary functions.
    peer.start_tx("tx2", &["put", "k", "v"]).await;
    let (resp, _) = peer.run_tx_to_completion("tx2").await;
    assert_eq!(resp.status, fabric_shim::OK);
}

#[tokio::test]
async fn get_metadata_falls_through_when_undeclared() {
    // TestKv has no metadata(): the call must reach invoke, which reports an
    // unknown function, instead of being answered by the shim.
    let (addr, _stop) = start_shim().await;
    let mut peer = MockPeer::connect(addr).await;

    peer.start_tx("tx1", &["org.hyperledger.fabric:GetMetadata"])
        .await;
    let (resp, _) = peer.run_tx_to_completion("tx1").await;
    assert_eq!(resp.status, fabric_shim::ERROR);
    assert!(
        resp.message.contains("unknown function"),
        "{}",
        resp.message
    );
}

// ---------------------------------------------------------------------------
// #[contract] macro end-to-end
// ---------------------------------------------------------------------------

#[derive(fabric_shim::DataType, serde::Serialize, serde::Deserialize)]
struct Item {
    #[serde(rename = "ID")]
    id: String,
    qty: u32,
}

#[derive(Default)]
struct Inventory;

#[fabric_shim::contract(name = "Inventory", version = "9.9.9")]
impl Inventory {
    #[transaction]
    async fn add_item(
        &self,
        ctx: &ChaincodeStub,
        id: String,
        qty: u32,
    ) -> Result<(), fabric_shim::Error> {
        let item = Item {
            id: id.clone(),
            qty,
        };
        ctx.put_state(&id, serde_json::to_vec(&item).unwrap()).await
    }

    #[transaction(evaluate)]
    async fn get_item(&self, ctx: &ChaincodeStub, id: String) -> Result<Item, fabric_shim::Error> {
        let bytes = ctx.get_state(&id).await?;
        serde_json::from_slice(&bytes)
            .map_err(|e| fabric_shim::Error::InvalidArgument(e.to_string()))
    }

    #[transaction(evaluate)]
    async fn double(&self, _ctx: &ChaincodeStub, n: u64) -> Result<u64, fabric_shim::Error> {
        Ok(n * 2)
    }
}

#[tokio::test]
async fn contract_macro_routes_and_parses_typed_args() {
    let (addr, _stop) = start_shim_with(Inventory).await;
    let mut peer = MockPeer::connect(addr).await;

    // snake_case method exposed as PascalCase, u32 parsed from string arg.
    peer.start_tx("tx1", &["AddItem", "widget", "42"]).await;
    let (resp, _) = peer.run_tx_to_completion("tx1").await;
    assert_eq!(resp.status, fabric_shim::OK, "{}", resp.message);
    assert_eq!(
        peer.state.get("widget").unwrap(),
        br#"{"ID":"widget","qty":42}"#
    );

    // Struct return serialized as JSON; namespaced call accepted.
    peer.start_tx("tx2", &["Inventory:GetItem", "widget"]).await;
    let (resp, _) = peer.run_tx_to_completion("tx2").await;
    assert_eq!(resp.status, fabric_shim::OK);
    assert_eq!(resp.payload, br#"{"ID":"widget","qty":42}"#);

    // Numeric return.
    peer.start_tx("tx3", &["Double", "21"]).await;
    let (resp, _) = peer.run_tx_to_completion("tx3").await;
    assert_eq!(resp.payload, b"42");

    // Bad numeric arg → clear error naming the parameter.
    peer.start_tx("tx4", &["AddItem", "widget", "not-a-number"])
        .await;
    let (resp, _) = peer.run_tx_to_completion("tx4").await;
    assert_eq!(resp.status, fabric_shim::ERROR);
    assert!(resp.message.contains("qty"), "{}", resp.message);

    // Wrong arg count and unknown function.
    peer.start_tx("tx5", &["AddItem", "widget"]).await;
    let (resp, _) = peer.run_tx_to_completion("tx5").await;
    assert!(
        resp.message.contains("expects 2 argument"),
        "{}",
        resp.message
    );
    peer.start_tx("tx6", &["Vanish"]).await;
    let (resp, _) = peer.run_tx_to_completion("tx6").await;
    assert!(
        resp.message.contains("unknown function"),
        "{}",
        resp.message
    );
}

#[tokio::test]
async fn contract_macro_serves_get_metadata() {
    let (addr, _stop) = start_shim_with(Inventory).await;
    let mut peer = MockPeer::connect(addr).await;

    peer.start_tx("tx1", &["org.hyperledger.fabric:GetMetadata"])
        .await;
    let (resp, _) = peer.run_tx_to_completion("tx1").await;
    assert_eq!(resp.status, fabric_shim::OK, "{}", resp.message);

    let md: serde_json::Value = serde_json::from_slice(&resp.payload).unwrap();
    assert_eq!(md["info"]["version"], "9.9.9");
    let txs = md["contracts"]["Inventory"]["transactions"]
        .as_array()
        .unwrap();
    assert_eq!(txs[0]["name"], "AddItem");
    assert_eq!(txs[1]["returns"]["$ref"], "#/components/schemas/Item");
    assert_eq!(
        md["components"]["schemas"]["Item"]["properties"]["qty"]["type"],
        "integer"
    );
}

#[tokio::test]
async fn composite_key_roundtrip() {
    let key = fabric_shim::create_composite_key("Asset", &["blue", "asset1"]).unwrap();
    assert!(key.starts_with('\u{0}'));
    let (object_type, attrs) = fabric_shim::split_composite_key(&key).unwrap();
    assert_eq!(object_type, "Asset");
    assert_eq!(attrs, vec!["blue", "asset1"]);

    assert!(fabric_shim::create_composite_key("bad\u{0}type", &[]).is_err());
    assert!(fabric_shim::create_composite_key("Asset", &["bad\u{10FFFF}attr"]).is_err());

    // Regression (found by fuzzing composite_key_roundtrip): an empty
    // object_type or attribute used to be accepted by create_composite_key
    // but silently dropped by split_composite_key's trailing-delimiter
    // filter, breaking the round trip. Both must now be rejected up front.
    assert!(fabric_shim::create_composite_key("", &[]).is_err());
    assert!(fabric_shim::create_composite_key("Asset", &[""]).is_err());
    assert!(fabric_shim::create_composite_key("Asset", &["blue", ""]).is_err());
}

// ---------------------------------------------------------------------------
// Private data (unlike composite_key_roundtrip above, this covers the actual
// wire protocol -- PutState/GetState/DelState with a collection set, plus the
// GetPrivateDataHash/PurgePrivateData message types -- not just pure encoding)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn private_data_roundtrip_and_isolation() {
    let (addr, _stop) = start_shim().await;
    let mut peer = MockPeer::connect(addr).await;

    peer.start_tx("tx1", &["put_pd", "collectionA", "k1", "secret1"])
        .await;
    let (resp, _) = peer.run_tx_to_completion("tx1").await;
    assert_eq!(resp.status, fabric_shim::OK, "{}", resp.message);
    assert_eq!(
        peer.private_state
            .get(&("collectionA".to_string(), "k1".to_string()))
            .unwrap(),
        b"secret1"
    );

    peer.start_tx("tx2", &["get_pd", "collectionA", "k1"]).await;
    let (resp, _) = peer.run_tx_to_completion("tx2").await;
    assert_eq!(resp.status, fabric_shim::OK);
    assert_eq!(resp.payload, b"secret1");

    // A different collection with the same key sees nothing -- private data
    // collections are isolated key spaces from each other.
    peer.start_tx("tx3", &["get_pd", "collectionB", "k1"]).await;
    let (resp, _) = peer.run_tx_to_completion("tx3").await;
    assert_eq!(resp.status, fabric_shim::OK);
    assert_eq!(resp.payload, Vec::<u8>::new());

    // World state (no collection) also sees nothing -- private data must
    // never leak into the world state key space.
    peer.start_tx("tx4", &["get", "k1"]).await;
    let (resp, _) = peer.run_tx_to_completion("tx4").await;
    assert_eq!(resp.status, fabric_shim::OK);
    assert_eq!(resp.payload, Vec::<u8>::new());

    peer.start_tx("tx5", &["del_pd", "collectionA", "k1"]).await;
    let (resp, _) = peer.run_tx_to_completion("tx5").await;
    assert_eq!(resp.status, fabric_shim::OK, "{}", resp.message);
    peer.start_tx("tx6", &["get_pd", "collectionA", "k1"]).await;
    let (resp, _) = peer.run_tx_to_completion("tx6").await;
    assert_eq!(resp.payload, Vec::<u8>::new());
}

#[tokio::test]
async fn private_data_hash_and_purge() {
    let (addr, _stop) = start_shim().await;
    let mut peer = MockPeer::connect(addr).await;

    peer.start_tx("tx1", &["put_pd", "collectionA", "k2", "v2"])
        .await;
    let (resp, _) = peer.run_tx_to_completion("tx1").await;
    assert_eq!(resp.status, fabric_shim::OK, "{}", resp.message);

    // GetPrivateDataHash is a distinct message type from GetState -- confirm
    // the shim actually sends it (not silently falling back to GetState) and
    // correctly relays whatever bytes the peer replies with. A real peer
    // computes sha256(value) here; the mock echoes the value; either way
    // this proves wire correctness, not hash correctness (that's the peer's
    // job in production, not the shim's).
    peer.start_tx("tx2", &["pd_hash", "collectionA", "k2"])
        .await;
    let req = peer.recv().await;
    assert_eq!(req.r#type(), MsgType::GetPrivateDataHash);
    peer.serve_request(req).await;
    let (resp, _) = peer.run_tx_to_completion("tx2").await;
    assert_eq!(resp.status, fabric_shim::OK, "{}", resp.message);
    assert_eq!(resp.payload, b"v2");

    peer.start_tx("tx3", &["purge_pd", "collectionA", "k2"])
        .await;
    let req = peer.recv().await;
    assert_eq!(req.r#type(), MsgType::PurgePrivateData);
    peer.serve_request(req).await;
    let (resp, _) = peer.run_tx_to_completion("tx3").await;
    assert_eq!(resp.status, fabric_shim::OK, "{}", resp.message);

    peer.start_tx("tx4", &["get_pd", "collectionA", "k2"]).await;
    let (resp, _) = peer.run_tx_to_completion("tx4").await;
    assert_eq!(resp.payload, Vec::<u8>::new());
}

// ---------------------------------------------------------------------------
// Composite key range queries -- composite_key_roundtrip above only checks
// create/split are inverses; this drives get_state_by_partial_composite_key
// through the real GetStateByRange wire path and checks the prefix filter
// actually separates unrelated object instances.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn composite_key_partial_range_query() {
    let (addr, _stop) = start_shim().await;
    let mut peer = MockPeer::connect(addr).await;

    peer.start_tx("tx1", &["pkey_put", "Asset", "blue", "asset1", "100"])
        .await;
    let (resp, _) = peer.run_tx_to_completion("tx1").await;
    assert_eq!(resp.status, fabric_shim::OK, "{}", resp.message);
    peer.start_tx("tx2", &["pkey_put", "Asset", "blue", "asset2", "200"])
        .await;
    let (resp, _) = peer.run_tx_to_completion("tx2").await;
    assert_eq!(resp.status, fabric_shim::OK, "{}", resp.message);
    peer.start_tx("tx3", &["pkey_put", "Asset", "red", "asset3", "300"])
        .await;
    let (resp, _) = peer.run_tx_to_completion("tx3").await;
    assert_eq!(resp.status, fabric_shim::OK, "{}", resp.message);

    // Partial key ["blue"] must match only the two "blue" assets, not the
    // "red" one -- proving the composite-key prefix range actually filters
    // by the shared attribute, not just returning everything under "Asset".
    peer.start_tx("tx4", &["pkey_range", "Asset", "blue"]).await;
    let (resp, _) = peer.run_tx_to_completion("tx4").await;
    assert_eq!(resp.status, fabric_shim::OK, "{}", resp.message);
    assert_eq!(resp.payload, b"blue|asset1,blue|asset2");
}
