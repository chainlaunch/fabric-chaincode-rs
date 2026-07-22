//! Fuzzes decoding of the top-level `ChaincodeMessage` envelope — the very
//! first thing the shim parses off the wire from a (potentially malicious
//! or buggy) peer. Must never panic, regardless of input.

#![no_main]

use fabric_shim_protos::peer::ChaincodeMessage;
use fabric_shim_protos::prost::Message;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = ChaincodeMessage::decode(data);
});
