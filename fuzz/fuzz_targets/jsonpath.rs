#![no_main]
//! Arbitrary bytes as a compiled jsonpath: refused cleanly, or decoded into a path that prints.
use libfuzzer_sys::fuzz_target;
use postbor_core::jsonpath::{Path, print};

fuzz_target!(|data: &[u8]| {
    // The version word first, so most inputs reach the items.
    let mut payload = 0x8000_0001u32.to_ne_bytes().to_vec();
    payload.extend_from_slice(data);
    if let Ok(p) = Path::decode(&payload) {
        print::print(&p);
    }
});
