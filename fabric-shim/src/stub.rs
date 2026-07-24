use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use fabric_shim_protos::common as pb_common;
use fabric_shim_protos::msp as pb_msp;
use fabric_shim_protos::peer as pb;
use fabric_shim_protos::peer::chaincode_message::Type as MsgType;
use prost::Message;

use crate::handler::PeerLink;
use crate::iterators::{HistoryQueryIterator, StateQueryIterator};
use crate::response::Response;
use crate::{Error, Result};

/// Namespace prefix for composite keys (U+0000), mirroring the Go shim.
const COMPOSITE_KEY_NAMESPACE: char = '\u{0}';
/// Largest unicode scalar value; terminates partial composite key ranges.
const MAX_UNICODE_RUNE: char = '\u{10FFFF}';

/// Shared per-transaction plumbing, also held by open query iterators.
pub(crate) struct StubCore {
    pub(crate) link: PeerLink,
    pub(crate) channel_id: String,
    pub(crate) tx_id: String,
    /// Serializes ledger calls within a transaction: the peer allows only one
    /// in-flight shim request per (channel, tx).
    pub(crate) gate: tokio::sync::Mutex<()>,
}

impl StubCore {
    pub(crate) async fn request(
        &self,
        msg_type: MsgType,
        payload: Vec<u8>,
    ) -> Result<pb::ChaincodeMessage> {
        let _serialize = self.gate.lock().await;
        self.link
            .request(msg_type, payload, &self.channel_id, &self.tx_id)
            .await
    }
}

/// Per-transaction API handed to [`crate::Chaincode`] implementations.
///
/// Equivalent to `shim.ChaincodeStubInterface` in `fabric-chaincode-go`.
pub struct ChaincodeStub {
    core: Arc<StubCore>,
    args: Vec<Vec<u8>>,
    decorations: HashMap<String, Vec<u8>>,
    signed_proposal: Option<pb::SignedProposal>,
    creator: Vec<u8>,
    transient: HashMap<String, Vec<u8>>,
    tx_timestamp: Option<prost_types::Timestamp>,
    event: Arc<Mutex<Option<pb::ChaincodeEvent>>>,
}

impl ChaincodeStub {
    pub(crate) fn new(
        link: PeerLink,
        channel_id: String,
        tx_id: String,
        input: pb::ChaincodeInput,
        signed_proposal: Option<pb::SignedProposal>,
    ) -> Result<Self> {
        let mut creator = Vec::new();
        let mut transient = HashMap::new();
        let mut tx_timestamp = None;

        if let Some(sp) = &signed_proposal {
            let proposal = pb::Proposal::decode(sp.proposal_bytes.as_ref())?;
            let header = pb_common::Header::decode(proposal.header.as_ref())?;

            let channel_header = pb_common::ChannelHeader::decode(header.channel_header.as_ref())?;
            tx_timestamp = channel_header.timestamp;

            let signature_header =
                pb_common::SignatureHeader::decode(header.signature_header.as_ref())?;
            creator = signature_header.creator;

            let payload = pb::ChaincodeProposalPayload::decode(proposal.payload.as_ref())?;
            transient = payload.transient_map;
        }

        let pb::ChaincodeInput {
            args, decorations, ..
        } = input;

        Ok(Self {
            core: Arc::new(StubCore {
                link,
                channel_id,
                tx_id,
                gate: tokio::sync::Mutex::new(()),
            }),
            args,
            decorations,
            signed_proposal,
            creator,
            transient,
            tx_timestamp,
            event: Arc::new(Mutex::new(None)),
        })
    }

    pub(crate) fn event_slot(&self) -> Arc<Mutex<Option<pb::ChaincodeEvent>>> {
        self.event.clone()
    }

    // ---- transaction identity ------------------------------------------------

    pub fn get_tx_id(&self) -> &str {
        &self.core.tx_id
    }

    pub fn get_channel_id(&self) -> &str {
        &self.core.channel_id
    }

    /// Timestamp from the transaction's channel header (client-set, identical
    /// on all endorsers — safe to use in chaincode logic).
    pub fn get_tx_timestamp(&self) -> Result<prost_types::Timestamp> {
        self.tx_timestamp
            .ok_or_else(|| Error::Protocol("transaction has no timestamp (no proposal)".into()))
    }

    // ---- arguments -----------------------------------------------------------

    pub fn get_args(&self) -> &[Vec<u8>] {
        &self.args
    }

    pub fn get_string_args(&self) -> Vec<String> {
        self.args
            .iter()
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect()
    }

    /// First arg as the function name, rest as parameters.
    pub fn get_function_and_args(&self) -> (String, Vec<String>) {
        let mut args = self.get_string_args();
        if args.is_empty() {
            (String::new(), args)
        } else {
            let function = args.remove(0);
            (function, args)
        }
    }

    // ---- identity & proposal -------------------------------------------------

    /// Raw creator bytes (a marshaled `msp.SerializedIdentity`).
    pub fn get_creator(&self) -> &[u8] {
        &self.creator
    }

    /// Creator decoded into MSP ID + identity (certificate) bytes.
    pub fn get_creator_identity(&self) -> Result<pb_msp::SerializedIdentity> {
        Ok(pb_msp::SerializedIdentity::decode(self.creator.as_slice())?)
    }

    pub fn get_transient(&self) -> &HashMap<String, Vec<u8>> {
        &self.transient
    }

    pub fn get_decorations(&self) -> &HashMap<String, Vec<u8>> {
        &self.decorations
    }

    pub fn get_signed_proposal(&self) -> Option<&pb::SignedProposal> {
        self.signed_proposal.as_ref()
    }

    // ---- world state ---------------------------------------------------------

    /// Value for `key` from the world state; empty if the key does not exist
    /// (Fabric does not distinguish missing keys from empty values here).
    pub async fn get_state(&self, key: &str) -> Result<Vec<u8>> {
        self.get_data(MsgType::GetState, key, "").await
    }

    pub async fn put_state(&self, key: &str, value: impl Into<Vec<u8>>) -> Result<()> {
        self.put_data("", key, value.into()).await
    }

    pub async fn del_state(&self, key: &str) -> Result<()> {
        self.del_data("", key).await
    }

    /// Range query over `[start_key, end_key)`; empty strings mean unbounded.
    pub async fn get_state_by_range(
        &self,
        start_key: &str,
        end_key: &str,
    ) -> Result<StateQueryIterator> {
        let (iter, _) = self.range_query("", start_key, end_key, None).await?;
        Ok(iter)
    }

    /// Paginated range query. Only valid in read-only (evaluate) transactions.
    pub async fn get_state_by_range_with_pagination(
        &self,
        start_key: &str,
        end_key: &str,
        page_size: i32,
        bookmark: &str,
    ) -> Result<(StateQueryIterator, pb::QueryResponseMetadata)> {
        let meta = pb::QueryMetadata {
            page_size,
            bookmark: bookmark.to_string(),
        };
        let (iter, response_meta) = self.range_query("", start_key, end_key, Some(meta)).await?;
        Ok((iter, response_meta.unwrap_or_default()))
    }

    /// Rich query (CouchDB state databases only).
    pub async fn get_query_result(&self, query: &str) -> Result<StateQueryIterator> {
        let payload = pb::GetQueryResult {
            query: query.to_string(),
            collection: String::new(),
            metadata: Default::default(),
        }
        .encode_to_vec();
        let reply = self.core.request(MsgType::GetQueryResult, payload).await?;
        let resp = pb::QueryResponse::decode(reply.payload.as_ref())?;
        Ok(StateQueryIterator::new(self.core.clone(), resp))
    }

    /// History of a key's values across blocks (peer must have history db enabled).
    pub async fn get_history_for_key(&self, key: &str) -> Result<HistoryQueryIterator> {
        let payload = pb::GetHistoryForKey {
            key: key.to_string(),
        }
        .encode_to_vec();
        let reply = self
            .core
            .request(MsgType::GetHistoryForKey, payload)
            .await?;
        let resp = pb::QueryResponse::decode(reply.payload.as_ref())?;
        Ok(HistoryQueryIterator::new(self.core.clone(), resp))
    }

    // ---- composite keys ------------------------------------------------------

    pub fn create_composite_key(&self, object_type: &str, attributes: &[&str]) -> Result<String> {
        create_composite_key(object_type, attributes)
    }

    pub fn split_composite_key(&self, composite_key: &str) -> Result<(String, Vec<String>)> {
        split_composite_key(composite_key)
    }

    pub async fn get_state_by_partial_composite_key(
        &self,
        object_type: &str,
        attributes: &[&str],
    ) -> Result<StateQueryIterator> {
        let prefix = create_composite_key(object_type, attributes)?;
        let end = format!("{prefix}{MAX_UNICODE_RUNE}");
        self.get_state_by_range(&prefix, &end).await
    }

    // ---- private data --------------------------------------------------------

    pub async fn get_private_data(&self, collection: &str, key: &str) -> Result<Vec<u8>> {
        require_collection(collection)?;
        self.get_data(MsgType::GetState, key, collection).await
    }

    /// SHA-256 hash of a private value — callable from any org, unlike
    /// `get_private_data` which requires collection membership.
    pub async fn get_private_data_hash(&self, collection: &str, key: &str) -> Result<Vec<u8>> {
        require_collection(collection)?;
        self.get_data(MsgType::GetPrivateDataHash, key, collection)
            .await
    }

    pub async fn put_private_data(
        &self,
        collection: &str,
        key: &str,
        value: impl Into<Vec<u8>>,
    ) -> Result<()> {
        require_collection(collection)?;
        self.put_data(collection, key, value.into()).await
    }

    pub async fn del_private_data(&self, collection: &str, key: &str) -> Result<()> {
        require_collection(collection)?;
        self.del_data(collection, key).await
    }

    pub async fn purge_private_data(&self, collection: &str, key: &str) -> Result<()> {
        require_collection(collection)?;
        let payload = pb::PurgePrivateState {
            key: key.to_string(),
            collection: collection.to_string(),
        }
        .encode_to_vec();
        self.core
            .request(MsgType::PurgePrivateData, payload)
            .await?;
        Ok(())
    }

    pub async fn get_private_data_by_range(
        &self,
        collection: &str,
        start_key: &str,
        end_key: &str,
    ) -> Result<StateQueryIterator> {
        require_collection(collection)?;
        let (iter, _) = self
            .range_query(collection, start_key, end_key, None)
            .await?;
        Ok(iter)
    }

    // ---- events --------------------------------------------------------------

    /// Set the single chaincode event emitted with this transaction. A second
    /// call replaces the first (Go shim semantics).
    pub fn set_event(&self, name: &str, payload: impl Into<Vec<u8>>) -> Result<()> {
        if name.is_empty() {
            return Err(Error::InvalidArgument(
                "event name must not be empty".into(),
            ));
        }
        *self.event.lock().unwrap() = Some(pb::ChaincodeEvent {
            chaincode_id: String::new(), // filled in by the peer
            tx_id: self.core.tx_id.clone(),
            event_name: name.to_string(),
            payload: payload.into(),
        });
        Ok(())
    }

    // ---- chaincode-to-chaincode ----------------------------------------------

    /// Invoke another chaincode on this peer. Same-channel calls join this
    /// transaction's read/write set; cross-channel calls are read-only.
    pub async fn invoke_chaincode(
        &self,
        chaincode_name: &str,
        args: Vec<Vec<u8>>,
        channel: &str,
    ) -> Result<Response> {
        let name = if channel.is_empty() {
            chaincode_name.to_string()
        } else {
            format!("{chaincode_name}/{channel}")
        };
        let payload = pb::ChaincodeSpec {
            r#type: pb::chaincode_spec::Type::Undefined as i32,
            chaincode_id: Some(pb::ChaincodeId {
                name,
                ..Default::default()
            }),
            input: Some(pb::ChaincodeInput {
                args: args.into_iter().collect(),
                decorations: HashMap::new(),
                is_init: false,
            }),
            timeout: 0,
        }
        .encode_to_vec();

        let reply = self.core.request(MsgType::InvokeChaincode, payload).await?;
        // The RESPONSE payload nests the callee's terminal ChaincodeMessage.
        let inner = pb::ChaincodeMessage::decode(reply.payload.as_ref())?;
        match inner.r#type() {
            MsgType::Completed => Ok(Response::from_proto(pb::Response::decode(
                inner.payload.as_ref(),
            )?)),
            MsgType::Error => Err(Error::Peer(
                String::from_utf8_lossy(&inner.payload).into_owned(),
            )),
            other => Err(Error::Protocol(format!(
                "unexpected nested message type {other:?} from invoked chaincode"
            ))),
        }
    }

    // ---- shared helpers ------------------------------------------------------

    async fn get_data(&self, msg_type: MsgType, key: &str, collection: &str) -> Result<Vec<u8>> {
        let payload = pb::GetState {
            key: key.to_string(),
            collection: collection.to_string(),
        }
        .encode_to_vec();
        let reply = self.core.request(msg_type, payload).await?;
        Ok(reply.payload)
    }

    async fn put_data(&self, collection: &str, key: &str, value: Vec<u8>) -> Result<()> {
        if key.is_empty() {
            return Err(Error::InvalidArgument("key must not be empty".into()));
        }
        let payload = pb::PutState {
            key: key.to_string(),
            value,
            collection: collection.to_string(),
        }
        .encode_to_vec();
        self.core.request(MsgType::PutState, payload).await?;
        Ok(())
    }

    async fn del_data(&self, collection: &str, key: &str) -> Result<()> {
        let payload = pb::DelState {
            key: key.to_string(),
            collection: collection.to_string(),
        }
        .encode_to_vec();
        self.core.request(MsgType::DelState, payload).await?;
        Ok(())
    }

    async fn range_query(
        &self,
        collection: &str,
        start_key: &str,
        end_key: &str,
        metadata: Option<pb::QueryMetadata>,
    ) -> Result<(StateQueryIterator, Option<pb::QueryResponseMetadata>)> {
        let payload = pb::GetStateByRange {
            start_key: start_key.to_string(),
            end_key: end_key.to_string(),
            collection: collection.to_string(),
            metadata: metadata.map(|m| m.encode_to_vec()).unwrap_or_default(),
        }
        .encode_to_vec();
        let reply = self.core.request(MsgType::GetStateByRange, payload).await?;
        let resp = pb::QueryResponse::decode(reply.payload.as_ref())?;
        let response_meta = if resp.metadata.is_empty() {
            None
        } else {
            Some(pb::QueryResponseMetadata::decode(resp.metadata.as_ref())?)
        };
        Ok((
            StateQueryIterator::new(self.core.clone(), resp),
            response_meta,
        ))
    }
}

fn require_collection(collection: &str) -> Result<()> {
    if collection.is_empty() {
        return Err(Error::InvalidArgument(
            "collection must not be empty".into(),
        ));
    }
    Ok(())
}

/// Build a composite key: `\x00{object_type}\x00{attr1}\x00{attr2}\x00...`
///
/// `object_type` and every attribute must be non-empty and free of U+0000 /
/// U+10FFFF.
pub fn create_composite_key(object_type: &str, attributes: &[&str]) -> Result<String> {
    validate_composite_component(object_type)?;
    let mut key = String::with_capacity(object_type.len() + 2);
    key.push(COMPOSITE_KEY_NAMESPACE);
    key.push_str(object_type);
    key.push(COMPOSITE_KEY_NAMESPACE);
    for attr in attributes {
        validate_composite_component(attr)?;
        key.push_str(attr);
        key.push(COMPOSITE_KEY_NAMESPACE);
    }
    Ok(key)
}

/// Split a composite key back into `(object_type, attributes)`.
pub fn split_composite_key(composite_key: &str) -> Result<(String, Vec<String>)> {
    let stripped = composite_key
        .strip_prefix(COMPOSITE_KEY_NAMESPACE)
        .ok_or_else(|| Error::InvalidArgument("not a composite key".into()))?;
    let mut parts = stripped
        .split(COMPOSITE_KEY_NAMESPACE)
        .filter(|p| !p.is_empty())
        .map(String::from);
    let object_type = parts
        .next()
        .ok_or_else(|| Error::InvalidArgument("composite key has no object type".into()))?;
    Ok((object_type, parts.collect()))
}

fn validate_composite_component(s: &str) -> Result<()> {
    // Rejecting empty components isn't just a style choice: split_composite_key
    // filters empty segments to drop the trailing one that a well-formed key's
    // final delimiter always produces (see its doc comment). An empty
    // object_type or attribute would be silently swallowed by that same
    // filter, breaking the create/split round trip for exactly those inputs
    // (found by fuzzing: `composite_key_roundtrip` on an empty object_type).
    if s.is_empty() {
        return Err(Error::InvalidArgument(
            "composite key component must not be empty".into(),
        ));
    }
    if s.contains(COMPOSITE_KEY_NAMESPACE) || s.contains(MAX_UNICODE_RUNE) {
        return Err(Error::InvalidArgument(format!(
            "composite key component {s:?} contains U+0000 or U+10FFFF"
        )));
    }
    Ok(())
}
