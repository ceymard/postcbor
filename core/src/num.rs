//! Numbers ↔ decimal text, for `numeric` casts and `to_cbor(numeric)` (design §8).

use crate::bigint;
use crate::cursor::*;
use crate::encode::write_head;
use crate::error::{Error, ErrorKind, NO_OFFSET, Result};
use crate::float;
use crate::out::OutBuf;
use crate::value::{Kind, Value};

fn bad(msg: &'static str) -> Error {
    Error::new(ErrorKind::InvalidArgument(msg), NO_OFFSET)
}

/// Integer value of an item: plain integers and bignums, as (negative, magnitude) where the value is
/// `magnitude` or `−magnitude`.
fn integer(v: &Value) -> Result<Option<(bool, Vec<u8>)>> {
    Ok(match v.kind()? {
        Kind::Unsigned(n) => Some((false, n.to_be_bytes().to_vec())),
        Kind::Negative(n) => {
            let mut m = n.to_be_bytes().to_vec();
            bigint::plus_one(&mut m);
            Some((true, m))
        }
        Kind::Tag(t @ (2 | 3)) => {
            let Kind::Bytes(b) = v.tag_content()?.kind()? else { return Ok(None) };
            let mut m = b.to_vec();
            if t == 3 {
                bigint::plus_one(&mut m);
            }
            Some((t == 3, m))
        }
        _ => None,
    })
}

fn int_text(neg: bool, mag: &[u8]) -> String {
    let d = bigint::be_to_decimal(mag);
    if neg && d != "0" { format!("-{d}") } else { d }
}

/// Decimal text of a numeric item (integers, floats, bignums, tag 4, tag 5 with exponent ≥ 0), or None.
/// Floats give `NaN`, `Infinity`, `-Infinity` or their shortest exact decimal.
pub fn to_decimal(v: &Value) -> Result<Option<String>> {
    if let Some((neg, mag)) = integer(v)? {
        return Ok(Some(int_text(neg, &mag)));
    }
    Ok(match v.kind()? {
        Kind::Float(f) if f.is_nan() => Some("NaN".into()),
        Kind::Float(f) if f.is_infinite() => Some(if f > 0.0 { "Infinity" } else { "-Infinity" }.into()),
        Kind::Float(f) => Some(format!("{f:?}")),
        Kind::Tag(t @ (4 | 5)) => {
            let c = v.tag_content()?;
            let mut it = c.array_iter()?;
            let (Some(e), Some(m), None) = (it.next().transpose()?, it.next().transpose()?, it.next()) else {
                return Err(bad("decimal fraction must be [exponent, mantissa]"));
            };
            let exp = match e.kind()? {
                Kind::Unsigned(n) => i64::try_from(n).map_err(|_| bad("exponent out of range"))?,
                Kind::Negative(n) => -1 - i64::try_from(n).map_err(|_| bad("exponent out of range"))?,
                _ => return Err(bad("exponent must be an integer")),
            };
            let (neg, mut mag) = integer(&m)?.ok_or(bad("mantissa must be an integer"))?;
            if t == 4 {
                Some(format!("{}e{exp}", int_text(neg, &mag)))
            } else {
                if !(0..=100_000).contains(&exp) {
                    return Err(bad("bigfloat with a negative exponent has no exact numeric value"));
                }
                // mantissa × 2^exp
                for _ in 0..exp {
                    let mut carry = 0u8;
                    for b in mag.iter_mut().rev() {
                        let x = (u16::from(*b) << 1) | u16::from(carry);
                        *b = x as u8;
                        carry = (x >> 8) as u8;
                    }
                    if carry > 0 {
                        mag.insert(0, carry);
                    }
                }
                Some(int_text(neg, &mag))
            }
        }
        _ => None,
    })
}

/// Writes the body for an integer given as decimal digits with an optional sign: plain integer or bignum.
pub fn write_integer(text: &str, out: &mut impl OutBuf) -> Result<()> {
    let (neg, digits) = match text.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let mut mag = bigint::digits_to_be(digits.as_bytes(), 10)?;
    if neg && !mag.is_empty() {
        bigint::minus_one(&mut mag);
    } else if neg {
        // -0
        return write_head(out, MAJOR_UNSIGNED, 0);
    }
    if mag.len() <= 8 {
        let v = mag.iter().fold(0u64, |a, &b| (a << 8) | u64::from(b));
        return write_head(out, if neg { MAJOR_NEGATIVE } else { MAJOR_UNSIGNED }, v);
    }
    write_head(out, MAJOR_TAG, if neg { 3 } else { 2 })?;
    write_head(out, MAJOR_BYTES, mag.len() as u64)?;
    out.write_bytes(&mag)
}

/// Writes the body for a `numeric` value's text: integers stay integers, other values become a tag 4
/// decimal fraction keeping the scale (`1.50` → `4([-2, 150])`), NaN and infinities become floats.
pub fn write_numeric(text: &str, out: &mut impl OutBuf) -> Result<()> {
    match text {
        "NaN" => return out.write_bytes(&float::CANONICAL_NAN),
        "Infinity" => return write_float(f64::INFINITY, out),
        "-Infinity" => return write_float(f64::NEG_INFINITY, out),
        _ => {}
    }
    let Some((int, frac)) = text.split_once('.') else { return write_integer(text, out) };
    let scale = frac.len() as u64;
    out.write_bytes(&[0xc4, 0x82])?;
    write_head(out, MAJOR_NEGATIVE, scale - 1)?;
    write_integer(&format!("{int}{frac}"), out)
}

/// Writes the body of a float, in its shortest exact width.
pub fn write_float(f: f64, out: &mut impl OutBuf) -> Result<()> {
    let mut b = [0u8; 9];
    let n = float::encode_shortest(f, &mut b);
    out.write_bytes(&b[..n])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::PlainCtx;
    use crate::datum::Datum;
    use crate::edn;

    fn dec(text: &str) -> Option<String> {
        let mut d = Vec::new();
        edn::to_datum(text.as_bytes(), &mut d, &mut PlainCtx::default()).unwrap();
        to_decimal(&Value::root(&Datum::parse(&d).unwrap())).unwrap()
    }

    fn back(text: &str, f: fn(&str, &mut Vec<u8>) -> Result<()>) -> String {
        let mut body = Vec::new();
        f(text, &mut body).unwrap();
        let mut d = Vec::new();
        crate::datum::from_body(&body, &mut d, usize::MAX).unwrap();
        let mut out = Vec::new();
        edn::print(&Value::root(&Datum::parse(&d).unwrap()), &mut out, Default::default()).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn numbers_to_decimal_text() {
        assert_eq!(dec("-18446744073709551616").as_deref(), Some("-18446744073709551616"));
        assert_eq!(dec("-18446744073709551617").as_deref(), Some("-18446744073709551617"));
        assert_eq!(dec("18446744073709551616").as_deref(), Some("18446744073709551616"));
        assert_eq!(dec("4([-2, 27315])").as_deref(), Some("27315e-2"));
        assert_eq!(dec("5([3, -3])").as_deref(), Some("-24"));
        assert_eq!(dec("1.5").as_deref(), Some("1.5"));
        assert_eq!(dec("NaN").as_deref(), Some("NaN"));
        assert_eq!(dec("\"1\""), None);
    }

    #[test]
    fn numeric_text_to_cbor_keeps_scale() {
        assert_eq!(back("1.50", write_numeric), "4([-2, 150])");
        assert_eq!(back("-0.5", write_numeric), "4([-1, -5])");
        assert_eq!(back("12", write_numeric), "12");
        assert_eq!(back("-18446744073709551617", write_numeric), "3(h'010000000000000000')");
        assert_eq!(back("NaN", write_numeric), "NaN");
    }
}
