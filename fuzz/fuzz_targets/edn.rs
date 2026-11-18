#![no_main]
//! Untrusted EDN text: rejected cleanly, or stored as a value meeting every invariant.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut out = Vec::new();
    if postbor_core::edn::to_datum(data, &mut out, &mut postbor_fuzz::ctx()).is_ok() {
        postbor_fuzz::check_stored(&out);
    }
});
