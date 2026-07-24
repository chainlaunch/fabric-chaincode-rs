use std::collections::VecDeque;
use std::sync::Arc;

use fabric_shim_protos::peer as pb;
use fabric_shim_protos::peer::chaincode_message::Type as MsgType;
use fabric_shim_protos::queryresult;
use prost::Message;

use crate::stub::StubCore;
use crate::Result;

/// Shared paging machinery for state/history iterators: drains the current
/// `QueryResponse` batch, then fetches more with QUERY_STATE_NEXT.
struct QueryIter {
    core: Arc<StubCore>,
    id: String,
    batch: VecDeque<pb::QueryResultBytes>,
    has_more: bool,
    closed: bool,
}

impl QueryIter {
    fn new(core: Arc<StubCore>, resp: pb::QueryResponse) -> Self {
        Self {
            core,
            id: resp.id,
            batch: resp.results.into(),
            has_more: resp.has_more,
            closed: false,
        }
    }

    async fn next_raw(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            if let Some(qrb) = self.batch.pop_front() {
                return Ok(Some(qrb.result_bytes));
            }
            if !self.has_more || self.closed {
                return Ok(None);
            }
            let payload = pb::QueryStateNext {
                id: self.id.clone(),
            }
            .encode_to_vec();
            let reply = self.core.request(MsgType::QueryStateNext, payload).await?;
            let resp = pb::QueryResponse::decode(reply.payload.as_ref())?;
            self.batch = resp.results.into();
            self.has_more = resp.has_more;
        }
    }

    async fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let payload = pb::QueryStateClose {
            id: self.id.clone(),
        }
        .encode_to_vec();
        self.core.request(MsgType::QueryStateClose, payload).await?;
        Ok(())
    }
}

/// Iterator over `queryresult::Kv` records from range, partial-composite-key,
/// and rich queries.
///
/// Call [`close`](Self::close) when done; the peer also cleans up open
/// iterators when the transaction completes.
pub struct StateQueryIterator {
    inner: QueryIter,
}

impl StateQueryIterator {
    pub(crate) fn new(core: Arc<StubCore>, resp: pb::QueryResponse) -> Self {
        Self {
            inner: QueryIter::new(core, resp),
        }
    }

    pub async fn next(&mut self) -> Result<Option<queryresult::Kv>> {
        match self.inner.next_raw().await? {
            Some(bytes) => Ok(Some(queryresult::Kv::decode(bytes.as_slice())?)),
            None => Ok(None),
        }
    }

    /// Drain the remaining records into a vector (and close the iterator).
    pub async fn collect_remaining(mut self) -> Result<Vec<queryresult::Kv>> {
        let mut out = Vec::new();
        while let Some(kv) = self.next().await? {
            out.push(kv);
        }
        self.close().await?;
        Ok(out)
    }

    pub async fn close(&mut self) -> Result<()> {
        self.inner.close().await
    }
}

/// Iterator over `queryresult::KeyModification` records from
/// [`crate::ChaincodeStub::get_history_for_key`].
pub struct HistoryQueryIterator {
    inner: QueryIter,
}

impl HistoryQueryIterator {
    pub(crate) fn new(core: Arc<StubCore>, resp: pb::QueryResponse) -> Self {
        Self {
            inner: QueryIter::new(core, resp),
        }
    }

    pub async fn next(&mut self) -> Result<Option<queryresult::KeyModification>> {
        match self.inner.next_raw().await? {
            Some(bytes) => Ok(Some(queryresult::KeyModification::decode(
                bytes.as_slice(),
            )?)),
            None => Ok(None),
        }
    }

    pub async fn close(&mut self) -> Result<()> {
        self.inner.close().await
    }
}
