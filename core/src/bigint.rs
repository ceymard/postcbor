//! Decimal ↔ big-endian magnitude conversions for bignums (EDN literals, numeric casts).

use crate::error::{Error, ErrorKind, Result};

/// Big-endian magnitude (no leading zeros; empty for zero) of a run of ASCII digits in `radix` (2, 8, 10, 16).
pub fn digits_to_be(digits: &[u8], radix: u32) -> Result<Vec<u8>> {
    let mut mag: Vec<u8> = Vec::new();
    for (i, &d) in digits.iter().enumerate() {
        let v = (d as char).to_digit(radix).ok_or(Error::new(ErrorKind::Syntax("invalid digit"), i))?;
        let mut carry = v;
        for b in mag.iter_mut().rev() {
            let x = u32::from(*b) * radix + carry;
            *b = x as u8;
            carry = x >> 8;
        }
        while carry > 0 {
            mag.try_reserve(1).map_err(|_| Error::new(ErrorKind::OutOfMemory, i))?;
            mag.insert(0, carry as u8);
            carry >>= 8;
        }
    }
    Ok(mag)
}

/// Decimal digits of a big-endian magnitude.
pub fn be_to_decimal(mag: &[u8]) -> String {
    let mut m: Vec<u8> = mag.iter().copied().skip_while(|&b| b == 0).collect();
    if m.is_empty() {
        return "0".into();
    }
    let mut out = Vec::new();
    while !m.is_empty() {
        let mut rem = 0u32;
        for b in m.iter_mut() {
            let x = (rem << 8) | u32::from(*b);
            *b = (x / 10) as u8;
            rem = x % 10;
        }
        out.push(b'0' + rem as u8);
        let first = m.iter().position(|&b| b != 0).unwrap_or(m.len());
        m.drain(..first);
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// Subtracts one from a non-zero big-endian magnitude.
pub fn minus_one(mag: &mut Vec<u8>) {
    for b in mag.iter_mut().rev() {
        if *b == 0 {
            *b = 0xff;
        } else {
            *b -= 1;
            break;
        }
    }
    let first = mag.iter().position(|&b| b != 0).unwrap_or(mag.len());
    mag.drain(..first);
}

/// Adds one to a big-endian magnitude.
pub fn plus_one(mag: &mut Vec<u8>) {
    for b in mag.iter_mut().rev() {
        if *b == 0xff {
            *b = 0;
        } else {
            *b += 1;
            return;
        }
    }
    mag.insert(0, 1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_round_trips() {
        for s in ["0", "1", "255", "256", "18446744073709551615", "18446744073709551616", "340282366920938463463374607431768211457"] {
            let m = digits_to_be(s.as_bytes(), 10).unwrap();
            assert_eq!(be_to_decimal(&m), s);
        }
        assert_eq!(digits_to_be(b"ff", 16).unwrap(), [0xff]);
        assert_eq!(digits_to_be(b"18446744073709551616", 10).unwrap(), [1, 0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn increments_carry() {
        let mut m = vec![1, 0];
        minus_one(&mut m);
        assert_eq!(m, [0xff]);
        plus_one(&mut m);
        assert_eq!(m, [1, 0]);
    }
}
