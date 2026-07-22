//! Fuzzes the exact nested-decode chain `ChaincodeStub::new` runs over
//! peer-supplied proposal bytes: SignedProposal -> Proposal -> (Header,
//! ChaincodeProposalPayload) -> (ChannelHeader, SignatureHeader). This is
//! the most complex untrusted-input parsing path in the shim, and the one
//! most worth fuzzing: a crafted proposal must never panic the connection.

#![no_main]

use fabric_shim_protos::common::{ChannelHeader, Header, SignatureHeader};
use fabric_shim_protos::peer::{ChaincodeProposalPayload, Proposal, SignedProposal};
use fabric_shim_protos::prost::Message;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(signed_proposal) = SignedProposal::decode(data) else {
        return;
    };
    let Ok(proposal) = Proposal::decode(signed_proposal.proposal_bytes.as_ref()) else {
        return;
    };
    let _ = ChaincodeProposalPayload::decode(proposal.payload.as_ref());
    let Ok(header) = Header::decode(proposal.header.as_ref()) else {
        return;
    };
    let _ = ChannelHeader::decode(header.channel_header.as_ref());
    let _ = SignatureHeader::decode(header.signature_header.as_ref());
});
