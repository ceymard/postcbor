//! Sort order (design §7): type rank, then natural value order. `cmp = Equal` exactly when bodies are equal.

use core::cmp::Ordering;

use crate::cursor::*;
use crate::error::{Error, ErrorKind, Result};
use crate::float;
use crate::value::Value;

fn corrupt(at: usize) -> Error {
    Error::new(ErrorKind::CorruptDatum, at)
}

/// A number read from a body, for cross-type numeric comparison.
#[derive(Clone, Copy)]
enum Num<'a> {
    Int(i128),
    /// Bignum beyond 64 bits; `neg` means the value is −1 − magnitude.
    Big { neg: bool, mag: &'a [u8] },
    Float(f64),
}

/// Rank of an item, and its number when it is one.
fn rank<'a>(h: &Head, c: &mut Cursor<'a>) -> Result<(u8, Option<Num<'a>>)> {
    Ok(match h.major {
        MAJOR_UNSIGNED => (0, Some(Num::Int(i128::from(h.arg)))),
        MAJOR_NEGATIVE => (0, Some(Num::Int(-1 - i128::from(h.arg)))),
        MAJOR_BYTES => (1, None),
        MAJOR_TEXT => (2, None),
        MAJOR_ARRAY => (3, None),
        MAJOR_MAP => (4, None),
        MAJOR_TAG if h.arg == 2 || h.arg == 3 => {
            let mut look = *c;
            let bh = look.head().map_err(|e| corrupt(e.offset))?;
            if bh.major != MAJOR_BYTES {
                return Err(corrupt(c.pos()));
            }
            let mag = look.take(bh.arg).map_err(|e| corrupt(e.offset))?;
            *c = look;
            (0, Some(Num::Big { neg: h.arg == 3, mag }))
        }
        MAJOR_TAG => (5, None),
        _ => match h.info {
            25..=27 => (0, Some(Num::Float(float::decode(h.info, h.arg)))),
            20 => (7, None),
            21 => (8, None),
            22 => (9, None),
            23 => (10, None),
            _ => (6, None),
        },
    })
}

/// Integer kinds sort before floats when values are equal.
fn num_cmp(a: Num, b: Num) -> Ordering {
    use Num::*;
    match (a, b) {
        (Int(x), Int(y)) => x.cmp(&y),
        (Float(x), Float(y)) => x.total_cmp(&y),
        (Big { neg: na, mag: ma }, Big { neg: nb, mag: mb }) => match (na, nb) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => mag_cmp(ma, mb),
            (true, true) => mag_cmp(mb, ma),
        },
        (Big { neg, .. }, Int(_)) => if neg { Ordering::Less } else { Ordering::Greater },
        (Int(_), Big { neg, .. }) => if neg { Ordering::Greater } else { Ordering::Less },
        (Int(i), Float(f)) => int_float(i, f).then(Ordering::Less),
        (Float(f), Int(i)) => int_float(i, f).reverse().then(Ordering::Greater),
        (Big { neg, mag }, Float(f)) => big_float(neg, mag, f).then(Ordering::Less),
        (Float(f), Big { neg, mag }) => big_float(neg, mag, f).reverse().then(Ordering::Greater),
    }
}

fn mag_cmp(a: &[u8], b: &[u8]) -> Ordering {
    // Normalization strips leading zeros, so longer means larger.
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// Exact comparison of an integer with a float, by value only.
fn int_float(i: i128, f: f64) -> Ordering {
    if f.is_nan() {
        return Ordering::Less;
    }
    let fl = f.floor();
    // i128 spans ±2^127; every u64/negative int fits well inside ±2^65.
    if fl >= float::pow2(100) {
        return Ordering::Less;
    }
    if fl < -float::pow2(100) {
        return Ordering::Greater;
    }
    let fi = fl as i128;
    match i.cmp(&fi) {
        Ordering::Equal if f > fl => Ordering::Less,
        o => o,
    }
}

/// Exact comparison of a bignum (|value| ≥ 2^64) with a float, by value only.
fn big_float(neg: bool, mag: &[u8], f: f64) -> Ordering {
    if f.is_nan() {
        return Ordering::Less;
    }
    if f.is_infinite() {
        return if f > 0.0 { Ordering::Less } else { Ordering::Greater };
    }
    let f_neg = f < 0.0;
    if neg != f_neg {
        return if neg { Ordering::Less } else { Ordering::Greater };
    }
    // Same sign. Compare magnitudes; floats ≥ 2^53 are integers, smaller ones lose to any bignum.
    let a = f.abs();
    let ord_mag = if a < float::pow2(64) {
        Ordering::Greater
    } else {
        let bits = a.to_bits();
        let exp = ((bits >> 52) & 0x7ff) as i32 - 1075;
        let m = (bits & ((1u64 << 52) - 1)) | (1u64 << 52);
        let fmag = shifted_be(m, exp as u32);
        // A negative bignum −1−n has magnitude n+1; compare against n via ≥ / >.
        if neg {
            plus_one_cmp(mag, &fmag)
        } else {
            mag_cmp(mag, &fmag)
        }
    };
    if neg { ord_mag.reverse() } else { ord_mag }
}

/// Big-endian bytes of m << shift, without leading zeros.
fn shifted_be(m: u64, shift: u32) -> Vec<u8> {
    let mut v: Vec<u8> = m.to_be_bytes().to_vec();
    v.extend(core::iter::repeat_n(0u8, (shift / 8) as usize));
    let r = shift % 8;
    if r > 0 {
        let mut carry = 0u16;
        for b in v.iter_mut().rev() {
            let x = (u16::from(*b) << r) | carry;
            *b = x as u8;
            carry = x >> 8;
        }
        v.insert(0, carry as u8);
    }
    let first = v.iter().position(|&b| b != 0).unwrap_or(v.len());
    v.split_off(first)
}

/// Compares mag + 1 with other.
fn plus_one_cmp(mag: &[u8], other: &[u8]) -> Ordering {
    let mut v = mag.to_vec();
    let mut i = v.len();
    loop {
        if i == 0 {
            v.insert(0, 1);
            break;
        }
        i -= 1;
        if v[i] == 0xff {
            v[i] = 0;
        } else {
            v[i] += 1;
            break;
        }
    }
    mag_cmp(&v, other)
}

/// Pair of cursors walking two items in parallel, with entries left at each nesting level.
struct Frame {
    left_a: u64,
    left_b: u64,
}

/// Total order of two stored items.
pub fn compare(a: &Value, b: &Value) -> Result<Ordering> {
    let mut ca = Cursor::at(a.d.body, a.pos);
    let mut cb = Cursor::at(b.d.body, b.pos);
    let mut stack: Vec<Frame> = Vec::new();
    let mut ticker = crate::ctx::Ticker::default();
    loop {
        ticker.tick();
        if let Some(top) = stack.last_mut() {
            match (top.left_a, top.left_b) {
                (0, 0) => {
                    stack.pop();
                    if stack.is_empty() {
                        return Ok(Ordering::Equal);
                    }
                    continue;
                }
                (0, _) => return Ok(Ordering::Less),
                (_, 0) => return Ok(Ordering::Greater),
                _ => {
                    top.left_a -= 1;
                    top.left_b -= 1;
                }
            }
        }
        let ha = ca.head().map_err(|e| corrupt(e.offset))?;
        let hb = cb.head().map_err(|e| corrupt(e.offset))?;
        let (ra, na) = rank(&ha, &mut ca)?;
        let (rb, nb) = rank(&hb, &mut cb)?;
        let o = ra.cmp(&rb);
        if o != Ordering::Equal {
            return Ok(o);
        }
        let o = match (na, nb) {
            (Some(x), Some(y)) => num_cmp(x, y),
            _ => match ha.major {
                MAJOR_BYTES | MAJOR_TEXT => {
                    let sa = ca.take(ha.arg).map_err(|e| corrupt(e.offset))?;
                    let sb = cb.take(hb.arg).map_err(|e| corrupt(e.offset))?;
                    sa.cmp(sb)
                }
                MAJOR_ARRAY => {
                    stack.push(Frame { left_a: ha.arg, left_b: hb.arg });
                    Ordering::Equal
                }
                MAJOR_MAP => {
                    let o = ha.arg.cmp(&hb.arg);
                    if o == Ordering::Equal {
                        stack.push(Frame { left_a: ha.arg * 2, left_b: hb.arg * 2 });
                    }
                    o
                }
                MAJOR_TAG => {
                    let o = ha.arg.cmp(&hb.arg);
                    if o == Ordering::Equal {
                        stack.push(Frame { left_a: 1, left_b: 1 });
                    }
                    o
                }
                // Simple values of the same rank: only unassigned ones carry a value.
                _ => ha.arg.cmp(&hb.arg),
            },
        };
        if o != Ordering::Equal {
            return Ok(o);
        }
        if stack.is_empty() {
            return Ok(Ordering::Equal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datum::Datum;
    use crate::test_util::stored;

    fn cmp_hex(a: &str, b: &str) -> Ordering {
        let (sa, sb) = (stored(a), stored(b));
        let va_d = Datum::parse(&sa).unwrap();
        let va = Value::root(&va_d);
        let vb_d = Datum::parse(&sb).unwrap();
        let vb = Value::root(&vb_d);
        compare(&va, &vb).unwrap()
    }

    #[test]
    fn values_follow_the_documented_order() {
        let asc = [
            "f9 fc00",                   // −Infinity
            "c3 49 010000000000000000", // −2^64 − 1
            "3b ffffffffffffffff",      // −2^64
            "fb c3f0000000000000",       // −2^64 as float (ties: integer first)
            "20",                        // −1
            "f9 bc00",                   // −1.0
            "00",                        // 0 (equal values: integer first)
            "f9 8000",                   // −0.0
            "f9 0000",                   // 0.0
            "f9 3800",                   // 0.5
            "01",                        // 1
            "1b ffffffffffffffff",       // 2^64 − 1
            "c2 49 010000000000000000",  // 2^64
            "fa 5f800000",               // 2^64 as float
            "f9 7c00",                   // Infinity
            "f9 7e00",                   // NaN
            "40",                        // h''
            "41 00",
            "60",                        // ""
            "62 6161",                   // "aa": text compares by content, not by encoded length
            "61 62",                     // "b"
        ];
        for w in asc.windows(2) {
            assert_eq!(cmp_hex(w[0], w[1]), Ordering::Less, "{} < {}", w[0], w[1]);
            assert_eq!(cmp_hex(w[1], w[0]), Ordering::Greater, "{} > {}", w[1], w[0]);
        }
        let ranks = ["01", "40", "60", "80", "a0", "c1 00", "f0", "f4", "f5", "f6", "f7"];
        for w in ranks.windows(2) {
            assert_eq!(cmp_hex(w[0], w[1]), Ordering::Less, "{} < {}", w[0], w[1]);
        }
    }

    #[test]
    fn containers_compare_elementwise_and_maps_by_size_first() {
        assert_eq!(cmp_hex("81 01", "82 01 00"), Ordering::Less);
        assert_eq!(cmp_hex("82 01 00", "81 02"), Ordering::Less);
        assert_eq!(cmp_hex("a1 01 02", "a2 01 00 02 00"), Ordering::Less);
        assert_eq!(cmp_hex("a1 01 03", "a1 01 02"), Ordering::Greater);
        assert_eq!(cmp_hex("c1 05", "c2 41 05"), Ordering::Greater); // 1(5) is a tag; 2(h'05') is the integer 5
        assert_eq!(cmp_hex("c1 05", "c1 05"), Ordering::Equal);
        assert_eq!(cmp_hex("c1 05", "d8 20 00"), Ordering::Less);
        assert_eq!(cmp_hex("82 81 01 02", "82 81 01 02"), Ordering::Equal);
    }
}
