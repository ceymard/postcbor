//! Float width conversions. Floats are stored in the shortest width that represents the value exactly.

/// Canonical NaN: every NaN, whatever its sign or payload, is stored as this f16.
pub const CANONICAL_NAN: [u8; 3] = [0xf9, 0x7e, 0x00];

/// Exactly 2^k for −1022 ≤ k ≤ 1023. `powi` gives no precision guarantee, so it is never used here.
pub fn pow2(k: i32) -> f64 {
    f64::from_bits(((k + 1023) as u64) << 52)
}

pub fn f16_to_f64(h: u16) -> f64 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = i32::from((h >> 10) & 0x1f);
    let mant = f64::from(h & 0x3ff);
    match exp {
        0 => sign * mant * pow2(-24),
        31 if mant == 0.0 => sign * f64::INFINITY,
        31 => f64::NAN,
        _ => sign * (1024.0 + mant) * pow2(exp - 25),
    }
}

/// The f16 bits for `x` when the conversion is exact. NaN must be handled by the caller.
pub fn f64_to_f16_exact(x: f64) -> Option<u16> {
    let bits = x.to_bits();
    let sign = ((bits >> 63) as u16) << 15;
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let mant = bits & ((1u64 << 52) - 1);
    if exp == 0x7ff {
        return (mant == 0).then_some(sign | 0x7c00);
    }
    if exp == 0 {
        // Zero, or an f64 subnormal: far below the smallest f16 subnormal.
        return (mant == 0).then_some(sign);
    }
    let e = exp - 1023;
    if !(-24..=15).contains(&e) {
        return None;
    }
    if e >= -14 {
        if mant & ((1u64 << 42) - 1) != 0 {
            return None;
        }
        return Some(sign | (((e + 15) as u16) << 10) | (mant >> 42) as u16);
    }
    // f16 subnormal: value = m × 2^-24.
    let full = (1u64 << 52) | mant;
    let shift = (28 - e) as u32;
    if full & ((1u64 << shift) - 1) != 0 {
        return None;
    }
    Some(sign | (full >> shift) as u16)
}

/// Writes the shortest exact encoding of `x` into a 9-byte buffer; returns the length used.
pub fn encode_shortest(x: f64, buf: &mut [u8; 9]) -> usize {
    if x.is_nan() {
        buf[..3].copy_from_slice(&CANONICAL_NAN);
        return 3;
    }
    if let Some(h) = f64_to_f16_exact(x) {
        buf[0] = 0xf9;
        buf[1..3].copy_from_slice(&h.to_be_bytes());
        return 3;
    }
    let f = x as f32;
    if f64::from(f) == x {
        buf[0] = 0xfa;
        buf[1..5].copy_from_slice(&f.to_bits().to_be_bytes());
        return 5;
    }
    buf[0] = 0xfb;
    buf[1..9].copy_from_slice(&x.to_bits().to_be_bytes());
    9
}

/// Decodes a float from its major-7 additional information (25, 26, 27) and raw bits.
pub fn decode(info: u8, bits: u64) -> f64 {
    match info {
        25 => f16_to_f64(bits as u16),
        26 => f64::from(f32::from_bits(bits as u32)),
        _ => f64::from_bits(bits),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(x: f64) -> Vec<u8> {
        let mut b = [0u8; 9];
        let n = encode_shortest(x, &mut b);
        b[..n].to_vec()
    }

    #[test]
    #[cfg_attr(miri, ignore = "too slow under Miri")]
    fn every_f16_round_trips_through_f64() {
        for h in 0..=u16::MAX {
            let x = f16_to_f64(h);
            if x.is_nan() {
                continue;
            }
            assert_eq!(f64_to_f16_exact(x), Some(h), "{h:#x}");
        }
    }

    #[test]
    fn shortest_width_is_chosen() {
        assert_eq!(enc(1.5), [0xf9, 0x3e, 0x00]);
        assert_eq!(enc(-0.0), [0xf9, 0x80, 0x00]);
        assert_eq!(enc(f64::INFINITY), [0xf9, 0x7c, 0x00]);
        assert_eq!(enc(5.960464477539063e-8), [0xf9, 0x00, 0x01]);
        assert_eq!(enc(100000.0), [0xfa, 0x47, 0xc3, 0x50, 0x00]);
        assert_eq!(enc(1.1), [0xfb, 0x3f, 0xf1, 0x99, 0x99, 0x99, 0x99, 0x99, 0x9a]);
        assert_eq!(enc(f64::from_bits(0x7ff8_0000_0000_0001)), CANONICAL_NAN);
        assert_eq!(enc(-f64::NAN), CANONICAL_NAN);
    }

    #[test]
    fn values_just_outside_f16_are_not_shortened() {
        // 65520 rounds to infinity in f16; 2^-25 is below the smallest f16 subnormal; 1 + 2^-11 needs 11 mantissa bits.
        for x in [65520.0, pow2(-25), 1.0 + pow2(-11)] {
            assert_eq!(f64_to_f16_exact(x), None, "{x}");
        }
    }
}
