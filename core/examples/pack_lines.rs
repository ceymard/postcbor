//! Test helper for tests/cborx: reads hex CBOR per line, prints `cbor_pack(value, 'cbor-x')` as hex, or `ERR <kind>`.
use std::io::{BufRead, Write};

use postbor_core::ctx::PlainCtx;
use postbor_core::datum::{Datum, from_cbor};
use postbor_core::pack::{PackFormat, pack};
use postbor_core::value::Value;

fn main() {
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let line = line.expect("stdin");
        let bytes: Vec<u8> = (0..line.len()).step_by(2).filter_map(|i| u8::from_str_radix(line.get(i..i + 2)?, 16).ok()).collect();
        let mut d = Vec::new();
        let r = from_cbor(&bytes, &mut d, &mut PlainCtx::default()).and_then(|()| {
            let mut packed = Vec::new();
            pack(&Value::root(&Datum::parse(&d)?), PackFormat::CborX, &mut packed)?;
            Ok(packed)
        });
        match r {
            Ok(p) => writeln!(out, "{}", p.iter().map(|b| format!("{b:02x}")).collect::<String>()).unwrap(),
            Err(e) => writeln!(out, "ERR {:?}", e.kind).unwrap(),
        }
    }
}
