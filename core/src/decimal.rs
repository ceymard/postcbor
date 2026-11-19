//! Exact decimal numbers, as PostgreSQL's `numeric` holds them: the values jsonpath computes with (design §8).

use crate::cursor::Cursor;
use crate::error::{Error, ErrorKind, NO_OFFSET, Result};

/// `(-1)^neg × coef × 10^-scale`, keeping the scale as written (`1.50` has scale 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dec {
    pub neg: bool,
    /// ASCII decimal digits without leading zeros; empty for zero.
    pub coef: Vec<u8>,
    pub scale: u32,
}

fn bad(msg: &'static str) -> Error {
    Error::new(ErrorKind::InvalidArgument(msg), NO_OFFSET)
}

const SIGN_MASK: u16 = 0xc000;
const NEG: u16 = 0x4000;
const SHORT: u16 = 0x8000;
const SPECIAL: u16 = 0xc000;
const SHORT_SIGN: u16 = 0x2000;
const SHORT_DSCALE_MASK: u16 = 0x1f80;
const SHORT_DSCALE_SHIFT: u16 = 7;
const SHORT_WEIGHT_SIGN: u16 = 0x0040;
const SHORT_WEIGHT_MASK: u16 = 0x003f;
const DSCALE_MASK: u16 = 0x3fff;
/// Digits per base-10000 group.
const GROUP: usize = 4;

impl Dec {
    /// Reads a finite PostgreSQL `numeric` varlena with a 4-byte header, in its in-memory (native-endian) form.
    /// NaN and infinities are refused: jsonpath literals cannot hold them.
    pub fn from_numeric(bytes: &[u8]) -> Result<Dec> {
        let mut c = Cursor::new(bytes);
        let hdr = c.read_u32_ne().map_err(|_| bad("truncated numeric"))?;
        let total = if cfg!(target_endian = "little") { hdr >> 2 } else { hdr & 0x3fff_ffff } as usize;
        if hdr & if cfg!(target_endian = "little") { 3 } else { 0xc000_0000 } != 0 || total < 6 || total > bytes.len() {
            return Err(bad("malformed numeric"));
        }
        let mut c = Cursor::new(bytes.get(..total).ok_or(bad("malformed numeric"))?);
        c.take(4)?;
        let h = c.read_u16_ne()?;
        let (neg, scale, weight) = match h & SIGN_MASK {
            SPECIAL => return Err(bad("NaN or infinity in a jsonpath numeric")),
            SHORT => {
                let w = h & SHORT_WEIGHT_MASK;
                let weight = if h & SHORT_WEIGHT_SIGN != 0 { i32::from(w) - 64 } else { i32::from(w) };
                (h & SHORT_SIGN != 0, u32::from((h & SHORT_DSCALE_MASK) >> SHORT_DSCALE_SHIFT), weight)
            }
            sign => {
                let w = c.read_u16_ne()? as i16;
                (sign == NEG, u32::from(h & DSCALE_MASK), i32::from(w))
            }
        };
        // value = groups (as one digit string) × 10^((weight - ngroups + 1) × 4)
        let mut digits = Vec::new();
        let mut ngroups: i64 = 0;
        while !c.is_empty() {
            let g = c.read_u16_ne()?;
            if g >= 10000 {
                return Err(bad("malformed numeric"));
            }
            digits.extend_from_slice(format!("{g:04}").as_bytes());
            ngroups += 1;
        }
        let exp10 = (i64::from(weight) - ngroups + 1) * GROUP as i64;
        // coef = value × 10^scale.
        let shift = exp10 + i64::from(scale);
        if shift >= 0 {
            digits.resize(digits.len() + shift as usize, b'0');
        } else {
            let cut = digits.len().saturating_sub(shift.unsigned_abs() as usize);
            if digits[cut..].iter().any(|&d| d != b'0') {
                return Err(bad("numeric digits beyond its scale"));
            }
            digits.truncate(cut);
        }
        let first = digits.iter().position(|&d| d != b'0').unwrap_or(digits.len());
        digits.drain(..first);
        Ok(Dec { neg: neg && !digits.is_empty(), coef: digits, scale })
    }

    /// Text as `numeric_out` writes it: `-12.50`, `0.001`, `0`.
    pub fn to_text(&self) -> String {
        let scale = self.scale as usize;
        let mut d = self.coef.clone();
        if d.len() <= scale {
            let mut z = vec![b'0'; scale + 1 - d.len()];
            z.extend_from_slice(&d);
            d = z;
        }
        let int_len = d.len() - scale;
        let mut s = String::with_capacity(d.len() + 2);
        if self.neg {
            s.push('-');
        }
        s.push_str(core::str::from_utf8(&d[..int_len]).unwrap_or("0"));
        if scale > 0 {
            s.push('.');
            s.push_str(core::str::from_utf8(&d[int_len..]).unwrap_or(""));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A long-format numeric in native byte order.
    fn numeric(neg: bool, weight: i16, dscale: u16, groups: &[u16]) -> Vec<u8> {
        let total = 8 + 2 * groups.len();
        let hdr = if cfg!(target_endian = "little") { (total as u32) << 2 } else { total as u32 };
        let mut v = hdr.to_ne_bytes().to_vec();
        v.extend_from_slice(&((if neg { NEG } else { 0 }) | dscale).to_ne_bytes());
        v.extend_from_slice(&weight.to_ne_bytes());
        for g in groups {
            v.extend_from_slice(&g.to_ne_bytes());
        }
        v
    }

    fn short(neg: bool, weight: i8, dscale: u16, groups: &[u16]) -> Vec<u8> {
        let total = 6 + 2 * groups.len();
        let hdr = if cfg!(target_endian = "little") { (total as u32) << 2 } else { total as u32 };
        let mut v = hdr.to_ne_bytes().to_vec();
        let w = if weight < 0 { SHORT_WEIGHT_SIGN | ((weight as i16 + 64) as u16) } else { weight as u16 };
        v.extend_from_slice(&(SHORT | if neg { SHORT_SIGN } else { 0 } | (dscale << SHORT_DSCALE_SHIFT) | w).to_ne_bytes());
        for g in groups {
            v.extend_from_slice(&g.to_ne_bytes());
        }
        v
    }

    #[test]
    fn numerics_read_with_their_scale() {
        let t = |b: Vec<u8>| Dec::from_numeric(&b).unwrap().to_text();
        assert_eq!(t(short(false, 0, 2, &[1, 5000])), "1.50");
        assert_eq!(t(short(true, 0, 0, &[12])), "-12");
        assert_eq!(t(short(false, -1, 3, &[10])), "0.001");
        assert_eq!(t(short(false, 0, 0, &[])), "0");
        assert_eq!(t(short(false, 0, 2, &[])), "0.00");
        assert_eq!(t(numeric(false, 2, 0, &[1, 0, 0])), "100000000");
        assert_eq!(t(numeric(true, 0, 1, &[3, 1000])), "-3.1");
        assert_eq!(t(numeric(false, -2, 6, &[1200])), "0.000012");
        assert!(Dec::from_numeric(&short(false, 0, 0, &[1, 5000])).is_err(), "digits beyond the scale");
        assert!(Dec::from_numeric(&[0, 0]).is_err());
    }
}
