//! CBOR → JSON text for the `jsonb`/`json` casts (ADR-0009). Lossy by default; strict mode errors instead.

use crate::edn::print::{write_float, write_text};
use crate::error::{Error, ErrorKind, NO_OFFSET, Result};
use crate::num;
use crate::out::OutBuf;
use crate::value::{Kind, Value, end_of};

fn unrepresentable(what: &'static str) -> Error {
    Error::new(ErrorKind::InvalidArgument(what), NO_OFFSET)
}

enum Frame {
    Container { remaining: u64, done: u64, is_map: bool },
    /// Inside a non-text map key that is printed as an EDN string.
    Tag,
}

/// Appends JSON text for `v`.
/// Lossy (`strict = false`): bytes → base64url strings, non-text keys → their EDN text, NaN/±Infinity,
/// undefined and unassigned simple values → null, tags dropped (bignums become JSON numbers).
pub fn print(v: &Value, out: &mut impl OutBuf, strict: bool) -> Result<()> {
    let mut stack: Vec<Frame> = Vec::new();
    let mut pos = v.pos;
    let mut ticker = crate::ctx::Ticker::default();
    loop {
        ticker.tick();
        if let Some(Frame::Container { done, is_map, .. }) = stack.last() {
            if *is_map && done % 2 == 1 {
                out.write_bytes(b": ")?;
            } else if *done > 0 {
                out.write_bytes(b", ")?;
            }
            // Map keys that are not text become strings holding their EDN.
            if *is_map && done % 2 == 0 {
                let k = Value { d: v.d, pos };
                if !matches!(k.kind()?, Kind::Text(_)) {
                    if strict {
                        return Err(unrepresentable("map key is not text"));
                    }
                    let mut edn = Vec::new();
                    crate::edn::print(&k, &mut edn, Default::default())?;
                    let s = core::str::from_utf8(&edn).map_err(|_| unrepresentable("map key"))?;
                    write_text(out, s, false)?;
                    pos = k.end()?;
                    complete(&mut stack, out)?;
                    continue;
                }
            }
        }
        let item = Value { d: v.d, pos };
        let mut opened = false;
        match item.kind()? {
            Kind::Unsigned(_) | Kind::Negative(_) => {
                out.write_bytes(num::to_decimal(&item)?.unwrap_or_default().as_bytes())?;
            }
            Kind::Float(f) if f.is_finite() => write_float(out, f)?,
            Kind::Float(_) => {
                if strict {
                    return Err(unrepresentable("NaN and Infinity have no JSON form"));
                }
                out.write_bytes(b"null")?;
            }
            Kind::Bytes(b) => {
                if strict {
                    return Err(unrepresentable("byte strings have no JSON form"));
                }
                out.write_byte(b'"')?;
                write_base64url(out, b)?;
                out.write_byte(b'"')?;
            }
            Kind::Text(s) => write_text(out, s, false)?,
            Kind::Array(n) | Kind::Map(n) => {
                let is_map = matches!(item.kind()?, Kind::Map(_));
                out.write_byte(if is_map { b'{' } else { b'[' })?;
                if n == 0 {
                    out.write_byte(if is_map { b'}' } else { b']' })?;
                } else {
                    stack.push(Frame::Container { remaining: if is_map { n * 2 } else { n }, done: 0, is_map });
                    pos = item.head()?.1;
                    opened = true;
                }
            }
            Kind::Tag(2 | 3) => {
                out.write_bytes(num::to_decimal(&item)?.unwrap_or_default().as_bytes())?;
            }
            Kind::Tag(_) => {
                if strict {
                    return Err(unrepresentable("tags have no JSON form"));
                }
                stack.push(Frame::Tag);
                pos = item.head()?.1;
                opened = true;
            }
            Kind::False => out.write_bytes(b"false")?,
            Kind::True => out.write_bytes(b"true")?,
            Kind::Null => out.write_bytes(b"null")?,
            Kind::Undefined | Kind::Simple(_) => {
                if strict {
                    return Err(unrepresentable("undefined and simple values have no JSON form"));
                }
                out.write_bytes(b"null")?;
            }
        }
        if opened {
            continue;
        }
        pos = end_of(&v.d, pos)?;
        if complete(&mut stack, out)? {
            return Ok(());
        }
    }
}

/// Accounts for a finished item; closes containers. Returns true when the whole value is done.
fn complete(stack: &mut Vec<Frame>, out: &mut impl OutBuf) -> Result<bool> {
    loop {
        match stack.last_mut() {
            None => return Ok(true),
            Some(Frame::Tag) => {
                stack.pop();
            }
            Some(Frame::Container { remaining, done, is_map }) => {
                *remaining -= 1;
                *done += 1;
                if *remaining > 0 {
                    return Ok(false);
                }
                let close = if *is_map { b'}' } else { b']' };
                stack.pop();
                out.write_byte(close)?;
            }
        }
    }
}

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn write_base64url(out: &mut impl OutBuf, b: &[u8]) -> Result<()> {
    for chunk in b.chunks(3) {
        let n = chunk.iter().fold(0u32, |a, &x| (a << 8) | u32::from(x)) << (8 * (3 - chunk.len()));
        for i in 0..=chunk.len() {
            out.write_byte(B64URL[((n >> (18 - 6 * i)) & 63) as usize])?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::PlainCtx;
    use crate::datum::Datum;
    use crate::edn;

    fn j(text: &str, strict: bool) -> Result<String> {
        let mut d = Vec::new();
        edn::to_datum(text.as_bytes(), &mut d, &mut PlainCtx::default()).unwrap();
        let mut out = Vec::new();
        print(&Value::root(Datum::parse(&d).unwrap()), &mut out, strict)?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn lossy_conversion_follows_rfc_8949_advice() {
        assert_eq!(j(r#"{"a": [1, -2, 1.5, h'fbff', true, null, undefined, NaN, 1(5), simple(16)]}"#, false).unwrap(),
            r#"{"a": [1, -2, 1.5, "-_8", true, null, null, null, 5, null]}"#);
        assert_eq!(j(r#"{1: "x", h'01': [], [1]: {}}"#, false).unwrap(), r#"{"1": "x", "h'01'": [], "[1]": {}}"#);
        assert_eq!(j("-18446744073709551617", false).unwrap(), "-18446744073709551617");
        assert_eq!(j("h'00010203'", false).unwrap(), r#""AAECAw""#);
    }

    #[test]
    fn strict_conversion_rejects_what_json_cannot_hold() {
        for t in ["h'00'", "NaN", "1(2)", "undefined", "{1: 2}"] {
            assert!(j(t, true).is_err(), "{t}");
        }
        assert_eq!(j(r#"{"a": [1, "x", 2(h'010000000000000000')]}"#, true).unwrap(), r#"{"a": [1, "x", 18446744073709551616]}"#);
    }
}
