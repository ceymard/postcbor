#![no_main]
//! Untrusted EDN text: rejected cleanly, or stored as a value meeting every invariant.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut out = Vec::new();
    if postcbor_core::edn::to_datum(data, &mut out, &mut postcbor_fuzz::ctx()).is_ok() {
        postcbor_fuzz::check_stored(&out);
    }
});
