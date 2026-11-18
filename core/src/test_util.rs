use crate::ctx::PlainCtx;
use crate::datum::from_cbor;

/// Bytes from hex, ignoring whitespace.
pub fn hex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

/// A stored datum for CBOR given in hex.
pub fn stored(h: &str) -> Vec<u8> {
    let mut out = Vec::new();
    from_cbor(&hex(h), &mut out, &mut PlainCtx::default()).unwrap();
    out
}
