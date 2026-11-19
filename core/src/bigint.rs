//! Decimal ↔ big-endian magnitude conversions for bignums (EDN literals, numeric casts).

use crate::error::{Error, ErrorKind, Result};

/// Big-endian magnitude (no leading zeros; empty for zero) of a run of ASCII digits in `radix` (2, 8, 10, 16).
pub fn digits_to_be(digits: &[u8], radix: u32) -> Result<Vec<u8>> {
    // Digits per step: the most whose value fits in a u64.
    let per = match radix {
        2 => 63,
        8 => 21,
        16 => 15,
        _ => 19,
    };
    let mut limbs: Vec<u64> = Vec::new(); // little-endian
    for (c, chunk) in digits.chunks(per).enumerate() {
        let mut v: u64 = 0;
        let mut scale: u64 = 1;
        for (j, &d) in chunk.iter().enumerate() {
            let i = c * per + j;
            let x = (d as char).to_digit(radix).ok_or(Error::new(ErrorKind::Syntax("invalid digit"), i))?;
            v = v * u64::from(radix) + u64::from(x);
            scale *= u64::from(radix);
        }
        let mut carry = u128::from(v);
        for l in limbs.iter_mut() {
            let x = u128::from(*l) * u128::from(scale) + carry;
            *l = x as u64;
            carry = x >> 64;
        }
        if carry > 0 {
            limbs.try_reserve(1).map_err(|_| Error::new(ErrorKind::OutOfMemory, c * per))?;
            limbs.push(carry as u64);
        }
    }
    let mut mag = Vec::new();
    mag.try_reserve(limbs.len() * 8).map_err(|_| Error::new(ErrorKind::OutOfMemory, 0))?;
    for l in limbs.iter().rev() {
        mag.extend_from_slice(&l.to_be_bytes());
    }
    let first = mag.iter().position(|&b| b != 0).unwrap_or(mag.len());
    mag.drain(..first);
    Ok(mag)
}

/// Decimal digits of a big-endian magnitude.
pub fn be_to_decimal(mag: &[u8]) -> String {
    const STEP: u128 = 10_000_000_000_000_000_000; // 10^19
    // Little-endian u64 limbs.
    let mut limbs: Vec<u64> = mag.rchunks(8).map(|c| c.iter().fold(0u64, |a, &b| (a << 8) | u64::from(b))).collect();
    while limbs.last() == Some(&0) {
        limbs.pop();
    }
    if limbs.is_empty() {
        return "0".into();
    }
    // Groups of 19 digits, least significant first.
    let mut groups: Vec<u64> = Vec::new();
    while !limbs.is_empty() {
        let mut rem: u128 = 0;
        for l in limbs.iter_mut().rev() {
            let x = (rem << 64) | u128::from(*l);
            *l = (x / STEP) as u64;
            rem = x % STEP;
        }
        groups.push(rem as u64);
        while limbs.last() == Some(&0) {
            limbs.pop();
        }
    }
    let mut out = String::with_capacity(groups.len() * 19);
    let mut it = groups.iter().rev();
    if let Some(g) = it.next() {
        out.push_str(&g.to_string());
    }
    for g in it {
        out.push_str(&format!("{g:019}"));
    }
    out
}

/// `mag × base^n` for a big-endian magnitude; `base` at most 10.
pub fn mul_pow(mag: &[u8], base: u64, n: u32) -> Result<Vec<u8>> {
    // The largest power of `base` that fits in a u64 multiplies at each step.
    let (mut step, mut per) = (1u64, 0u32);
    while let Some(next) = step.checked_mul(base) {
        step = next;
        per += 1;
    }
    let mut limbs: Vec<u64> = mag.rchunks(8).map(|c| c.iter().fold(0u64, |a, &b| (a << 8) | u64::from(b))).collect();
    let mut left = n;
    while left > 0 {
        let k = left.min(per);
        let m = base.pow(k);
        let mut carry: u128 = 0;
        for l in limbs.iter_mut() {
            let x = u128::from(*l) * u128::from(m) + carry;
            *l = x as u64;
            carry = x >> 64;
        }
        if carry > 0 {
            limbs.try_reserve(1).map_err(|_| Error::new(ErrorKind::OutOfMemory, 0))?;
            limbs.push(carry as u64);
        }
        left -= k;
    }
    let mut out = Vec::new();
    out.try_reserve(limbs.len() * 8).map_err(|_| Error::new(ErrorKind::OutOfMemory, 0))?;
    for l in limbs.iter().rev() {
        out.extend_from_slice(&l.to_be_bytes());
    }
    let first = out.iter().position(|&b| b != 0).unwrap_or(out.len());
    out.drain(..first);
    Ok(out)
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
        assert_eq!(digits_to_be(b"000", 10).unwrap(), [] as [u8; 0]);
        assert_eq!(be_to_decimal(&[0, 0, 1]), "1");
    }

    #[test]
    fn long_numbers_convert_in_every_radix() {
        // Digit counts that cross several step boundaries, against byte-wise expectations.
        let hex: String = (0..77).map(|i| char::from(b"0123456789abcdef"[(i * 7 + 3) % 16])).collect();
        let bytes: Vec<u8> = (0..39).map(|i| u8::from_str_radix(&format!("0{hex}")[2 * i..2 * i + 2], 16).unwrap()).collect();
        let want: Vec<u8> = bytes.iter().copied().skip_while(|&b| b == 0).collect();
        assert_eq!(digits_to_be(hex.as_bytes(), 16).unwrap(), want);
        let bin: String = want.iter().map(|b| format!("{b:08b}")).collect();
        assert_eq!(digits_to_be(bin.as_bytes(), 2).unwrap(), want);
        let oct = "7".repeat(64); // 192 one bits
        assert_eq!(digits_to_be(oct.as_bytes(), 8).unwrap(), vec![0xff; 24]);
        let dec: String = (0..1000).map(|i| char::from(b'1' + (i * 3 % 9) as u8)).collect();
        assert_eq!(be_to_decimal(&digits_to_be(dec.as_bytes(), 10).unwrap()), dec);
        assert_eq!(be_to_decimal(&digits_to_be(b"10000000000000000000", 10).unwrap()), "10000000000000000000");
        assert!(digits_to_be(b"12a", 10).is_err());
    }

    #[test]
    fn powers_multiply_exactly() {
        assert_eq!(be_to_decimal(&mul_pow(&[3], 5, 1).unwrap()), "15");
        assert_eq!(be_to_decimal(&mul_pow(&[1], 5, 30).unwrap()), "931322574615478515625");
        assert_eq!(be_to_decimal(&mul_pow(&[1], 2, 100).unwrap()), "1267650600228229401496703205376");
        assert_eq!(mul_pow(&[], 5, 3).unwrap(), [] as [u8; 0]);
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
