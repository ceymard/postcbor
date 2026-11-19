#![no_main]
//! Untrusted CBOR bytes: rejected cleanly, or stored as a value meeting every invariant.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Some(stored) = postcbor_fuzz::store(data) {
        postcbor_fuzz::check_stored(&stored);
    }
});
