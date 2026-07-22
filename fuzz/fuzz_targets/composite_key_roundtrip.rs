//! Fuzzes composite-key construction and parsing with arbitrary (including
//! invalid-UTF-8-derived, empty, and adversarial-delimiter) input. Chaincode
//! authors build these from user-controlled arguments, so this boundary must
//! reject bad input with an `Err`, never panic.

#![no_main]

use fabric_shim::{create_composite_key, split_composite_key};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Split the fuzzer's bytes into an object_type and 0+ attributes on
    // newlines so multi-attribute keys get exercised too.
    let text = String::from_utf8_lossy(data);
    let mut parts = text.split('\n');
    let object_type = parts.next().unwrap_or("");
    let attributes: Vec<&str> = parts.collect();

    if let Ok(key) = create_composite_key(object_type, &attributes) {
        // Whatever we can successfully construct must also parse back.
        let (parsed_type, parsed_attrs) =
            split_composite_key(&key).expect("a composite key we just built must be splittable");
        assert_eq!(parsed_type, object_type);
        assert_eq!(parsed_attrs, attributes);
    }

    // Splitting arbitrary text (not necessarily one we built) must never
    // panic either, regardless of whether it's a well-formed composite key.
    let _ = split_composite_key(&text);
});
