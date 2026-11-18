//! Test helper for the cross-check script: reads hex CBOR per line, prints the normalized body as hex or `ERR <kind>`.
use std::io::{BufRead, Write};

use postbor_core::ctx::PlainCtx;
use postbor_core::datum::{Datum, from_cbor};

fn main() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.expect("stdin");
        let bytes: Option<Vec<u8>> = (0..line.len())
            .step_by(2)
            .map(|i| line.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
            .collect();
        let Some(bytes) = bytes else {
            writeln!(out, "ERR hex").unwrap();
            continue;
        };
        let mut d = Vec::new();
        match from_cbor(&bytes, &mut d, &mut PlainCtx::default()) {
            Ok(()) => {
                let body = Datum::parse(&d).unwrap().body;
                writeln!(out, "{}", body.iter().map(|b| format!("{b:02x}")).collect::<String>()).unwrap();
            }
            Err(e) => writeln!(out, "ERR {:?}", e.kind).unwrap(),
        }
    }
}
