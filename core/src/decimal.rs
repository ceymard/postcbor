//! Exact decimal numbers, as PostgreSQL's `numeric` holds them: the values jsonpath computes with (design §8).

use core::cmp::Ordering;

use crate::cursor::Cursor;
use crate::error::{Error, ErrorKind, NO_OFFSET, Result};
use crate::out::OutBuf;

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
const NAN: u16 = 0xc000;
const PINF: u16 = 0xd000;
const NINF: u16 = 0xf000;
/// Digits per base-10000 group.
const GROUP: usize = 4;
/// `numeric`'s limits: digits before the decimal point, and scale.
const MAX_INT_DIGITS: usize = 131_072;
const MAX_SCALE: u32 = 16_383;

fn overflow() -> Error {
    bad("value overflows numeric format")
}

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
        Self::from_groups(h, &mut c)
    }

    /// Reads the part after the varlena header; `h` is the first 16-bit word.
    fn from_groups(h: u16, c: &mut Cursor) -> Result<Dec> {
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

impl Dec {
    pub fn zero() -> Dec {
        Dec { neg: false, coef: Vec::new(), scale: 0 }
    }

    /// Digits before the decimal point (may be ≤ 0 for values below 1).
    fn int_len(&self) -> i64 {
        self.coef.len() as i64 - i64::from(self.scale)
    }

    /// Compares by value, ignoring scale.
    pub fn cmp_value(&self, o: &Dec) -> Ordering {
        let (az, bz) = (self.coef.is_empty(), o.coef.is_empty());
        let sign = |neg: bool, zero: bool| if zero { 0 } else if neg { -1 } else { 1 };
        match sign(self.neg, az).cmp(&sign(o.neg, bz)) {
            Ordering::Equal if az => return Ordering::Equal,
            Ordering::Equal => {}
            ord => return ord,
        }
        let mag = self.int_len().cmp(&o.int_len()).then_with(|| {
            // Same number of integer digits: compare digit by digit, padding the shorter with zeros.
            let n = self.coef.len().max(o.coef.len());
            (0..n).map(|i| (self.coef.get(i).copied().unwrap_or(b'0'), o.coef.get(i).copied().unwrap_or(b'0'))).find(|(a, b)| a != b).map_or(Ordering::Equal, |(a, b)| a.cmp(&b))
        });
        if self.neg { mag.reverse() } else { mag }
    }

    /// Value truncated toward zero, with scale 0.
    pub fn trunc(&self) -> Dec {
        let keep = self.coef.len().saturating_sub(self.scale as usize);
        let coef = self.coef.get(..keep).unwrap_or_default().to_vec();
        Dec { neg: self.neg && !coef.is_empty(), coef, scale: 0 }
    }

    fn has_fraction(&self) -> bool {
        let keep = self.coef.len().saturating_sub(self.scale as usize);
        self.coef.get(keep..).is_some_and(|f| f.iter().any(|&d| d != b'0'))
    }

    /// `numeric_floor` / `numeric_ceil`: scale 0, rounded down or up.
    pub fn floor_ceil(&self, ceil: bool) -> Dec {
        let mut t = self.trunc();
        if self.has_fraction() && self.neg != ceil {
            // Away from zero: add one to the magnitude.
            let mut carry = true;
            for d in t.coef.iter_mut().rev() {
                if *d == b'9' {
                    *d = b'0';
                } else {
                    *d += 1;
                    carry = false;
                    break;
                }
            }
            if carry {
                t.coef.insert(0, b'1');
            }
            t.neg = self.neg;
        }
        t
    }

    /// Value rounded half away from zero to an integer, if it fits in i64 (`numeric_int8`).
    pub fn round_i64(&self) -> Option<i64> {
        let mut t = self.trunc();
        let keep = self.coef.len().saturating_sub(self.scale as usize);
        // First digit after the point; values below 0.1 have an implicit zero there.
        let first_frac = if self.scale == 0 || self.int_len() < 0 { b'0' } else { self.coef.get(keep).copied().unwrap_or(b'0') };
        if first_frac >= b'5' {
            t = t.add_one_magnitude();
        }
        if t.coef.len() > 19 {
            return None;
        }
        let m = t.coef.iter().fold(0i128, |a, &d| a * 10 + i128::from(d - b'0'));
        i64::try_from(if self.neg { -m } else { m }).ok()
    }

    fn add_one_magnitude(mut self) -> Dec {
        let mut carry = true;
        for d in self.coef.iter_mut().rev() {
            if *d == b'9' {
                *d = b'0';
            } else {
                *d += 1;
                carry = false;
                break;
            }
        }
        if carry {
            self.coef.insert(0, b'1');
        }
        self
    }

    /// Writes the value as a PostgreSQL `numeric` varlena with a 4-byte header, in native byte order.
    pub fn write_numeric(&self, out: &mut impl OutBuf) -> Result<()> {
        // Pad so the decimal point falls between base-10000 groups.
        let int_len = self.int_len();
        let lead = (GROUP as i64 - int_len.rem_euclid(GROUP as i64)) % GROUP as i64;
        let frac_pad = (GROUP - self.scale as usize % GROUP) % GROUP;
        let mut digits = vec![b'0'; lead as usize];
        if int_len < 0 {
            digits.resize(digits.len() + int_len.unsigned_abs() as usize, b'0');
        }
        digits.extend_from_slice(&self.coef);
        digits.resize(digits.len() + frac_pad, b'0');
        let int_groups = (int_len.max(0) + lead) / GROUP as i64;
        let mut groups: Vec<u16> = digits.chunks(GROUP).map(|g| g.iter().fold(0u16, |a, &d| a * 10 + u16::from(d - b'0'))).collect();
        let mut weight = int_groups - 1;
        let first = groups.iter().position(|&g| g != 0).unwrap_or(groups.len());
        groups.drain(..first);
        weight -= first as i64;
        while groups.last() == Some(&0) {
            groups.pop();
        }
        if groups.is_empty() {
            weight = 0;
        }
        let weight = i16::try_from(weight).map_err(|_| overflow())?;
        let total = 8 + 2 * groups.len();
        let hdr = if cfg!(target_endian = "little") { (total as u32) << 2 } else { total as u32 };
        out.write_bytes(&hdr.to_ne_bytes())?;
        let sign = if self.neg { NEG } else { 0 };
        out.write_bytes(&(sign | (self.scale as u16 & DSCALE_MASK)).to_ne_bytes())?;
        out.write_bytes(&weight.to_ne_bytes())?;
        for g in groups {
            out.write_bytes(&g.to_ne_bytes())?;
        }
        Ok(())
    }

    /// Reads decimal text as `numeric_in` does for the forms the core writes: `-12.50`, `27315e-2`, `1.5e20`.
    /// The scale is the digits after the point minus the exponent, at least 0.
    pub fn parse(text: &str) -> Result<Dec> {
        let invalid = || bad("invalid decimal text");
        let (neg, t) = match text.strip_prefix('-') {
            Some(t) => (true, t),
            None => (false, text),
        };
        let (mant, exp) = match t.split_once(['e', 'E']) {
            Some((m, e)) => (m, e.parse::<i64>().map_err(|_| overflow())?),
            None => (t, 0),
        };
        let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
        if int.is_empty() && frac.is_empty() || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
            return Err(invalid());
        }
        let mut coef: Vec<u8> = int.bytes().chain(frac.bytes()).collect();
        let shift = exp.checked_sub(frac.len() as i64).ok_or_else(overflow)?;
        let scale = if shift >= 0 {
            let n = usize::try_from(shift).map_err(|_| overflow())?;
            if n > MAX_INT_DIGITS {
                return Err(overflow());
            }
            coef.resize(coef.len() + n, b'0');
            0
        } else {
            u32::try_from(shift.unsigned_abs()).ok().filter(|&s| s <= MAX_SCALE).ok_or_else(overflow)?
        };
        let first = coef.iter().position(|&d| d != b'0').unwrap_or(coef.len());
        coef.drain(..first);
        let d = Dec { neg: neg && !coef.is_empty(), coef, scale };
        if d.int_len() > MAX_INT_DIGITS as i64 {
            return Err(overflow());
        }
        Ok(d)
    }
}

/// A jsonpath number: PostgreSQL's `numeric`, with machine integers kept apart so the common comparisons need no
/// allocation. NaN equals itself and sorts above everything, as in `numeric`.
#[derive(Clone, Debug, PartialEq)]
pub enum Num {
    Int(i64),
    Dec(Dec),
    NaN,
    Inf { neg: bool },
}

impl Num {
    pub fn to_dec(&self) -> Option<Dec> {
        match self {
            Num::Int(i) => Some(Dec::from_i64(*i)),
            Num::Dec(d) => Some(d.clone()),
            _ => None,
        }
    }

    /// Text as `numeric_out` writes it.
    pub fn to_text(&self) -> String {
        match self {
            Num::Int(i) => i.to_string(),
            Num::Dec(d) => d.to_text(),
            Num::NaN => "NaN".into(),
            Num::Inf { neg: false } => "Infinity".into(),
            Num::Inf { neg: true } => "-Infinity".into(),
        }
    }

    /// Reads the text forms [`Num::to_text`] and `num::to_decimal` write.
    pub fn parse(text: &str) -> Result<Num> {
        Ok(match text {
            "NaN" => Num::NaN,
            "Infinity" => Num::Inf { neg: false },
            "-Infinity" => Num::Inf { neg: true },
            _ => Num::from_dec(Dec::parse(text)?),
        })
    }

    /// Integers of scale 0 that fit become [`Num::Int`].
    pub fn from_dec(d: Dec) -> Num {
        if d.scale == 0 && d.coef.len() <= 18 {
            let m = d.coef.iter().fold(0i64, |a, &c| a * 10 + i64::from(c - b'0'));
            return Num::Int(if d.neg { -m } else { m });
        }
        Num::Dec(d)
    }

    pub fn cmp_value(&self, o: &Num) -> Ordering {
        use Num::*;
        let rank = |n: &Num| match n {
            Inf { neg: true } => 0,
            Int(_) | Dec(_) => 1,
            Inf { neg: false } => 2,
            NaN => 3,
        };
        match (self, o) {
            (Int(a), Int(b)) => a.cmp(b),
            _ if rank(self) != 1 || rank(o) != 1 => rank(self).cmp(&rank(o)),
            _ => self.to_dec().unwrap_or_else(self::Dec::zero).cmp_value(&o.to_dec().unwrap_or_else(self::Dec::zero)),
        }
    }

    pub fn neg(&self) -> Num {
        match self {
            Num::Int(i) => i.checked_neg().map_or_else(|| Num::Dec(Dec { neg: false, ..Dec::from_i64(*i) }), Num::Int),
            Num::Dec(d) => Num::Dec(Dec { neg: !d.neg && !d.coef.is_empty(), ..d.clone() }),
            Num::NaN => Num::NaN,
            Num::Inf { neg } => Num::Inf { neg: !neg },
        }
    }

    pub fn abs(&self) -> Num {
        match self {
            Num::Int(i) if *i < 0 => self.neg(),
            Num::Dec(d) => Num::Dec(Dec { neg: false, ..d.clone() }),
            Num::Inf { .. } => Num::Inf { neg: false },
            n => n.clone(),
        }
    }

    pub fn floor_ceil(&self, ceil: bool) -> Num {
        match self {
            Num::Dec(d) => Num::from_dec(d.floor_ceil(ceil)),
            n => n.clone(),
        }
    }

    /// Truncated toward zero, if it fits in i32 (jsonpath array subscripts).
    pub fn trunc_i32(&self) -> Option<i32> {
        match self {
            Num::Int(i) => i32::try_from(*i).ok(),
            Num::Dec(d) => d.trunc().round_i64().and_then(|i| i32::try_from(i).ok()),
            _ => None,
        }
    }

    /// Rounded half away from zero, if it fits in i64 (`numeric_int8`).
    pub fn round_i64(&self) -> Option<i64> {
        match self {
            Num::Int(i) => Some(*i),
            Num::Dec(d) => d.round_i64(),
            _ => None,
        }
    }

    /// Writes the value as a PostgreSQL `numeric` varlena, in native byte order.
    pub fn write_numeric(&self, out: &mut impl OutBuf) -> Result<()> {
        let special = |out: &mut dyn FnMut(&[u8]) -> Result<()>, h: u16| {
            let hdr: u32 = if cfg!(target_endian = "little") { 6 << 2 } else { 6 };
            out(&hdr.to_ne_bytes())?;
            out(&h.to_ne_bytes())
        };
        let mut w = |b: &[u8]| out.write_bytes(b);
        match self {
            Num::Int(i) => Dec::from_i64(*i).write_numeric(out),
            Num::Dec(d) => d.write_numeric(out),
            Num::NaN => special(&mut w, NAN),
            Num::Inf { neg } => special(&mut w, if *neg { NINF } else { PINF }),
        }
    }

    /// Reads a PostgreSQL `numeric` varlena with a 4-byte header, in native byte order.
    pub fn from_numeric(bytes: &[u8]) -> Result<Num> {
        let mut c = Cursor::new(bytes);
        c.take(4).map_err(|_| bad("truncated numeric"))?;
        let h = c.read_u16_ne().map_err(|_| bad("truncated numeric"))?;
        Ok(match h {
            NAN => Num::NaN,
            PINF => Num::Inf { neg: false },
            NINF => Num::Inf { neg: true },
            _ => Num::from_dec(Dec::from_numeric(bytes)?),
        })
    }
}

impl Dec {
    pub fn from_i64(i: i64) -> Dec {
        let coef = if i == 0 { Vec::new() } else { i.unsigned_abs().to_string().into_bytes() };
        Dec { neg: i < 0, coef, scale: 0 }
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

    fn n(t: &str) -> Num {
        Num::parse(t).unwrap()
    }

    #[test]
    fn decimal_text_reads_with_numeric_in_scales() {
        for (t, want) in [("1.50", "1.50"), ("27315e-2", "273.15"), ("1e20", "100000000000000000000"), ("1e-7", "0.0000001"), ("1.5e-7", "0.00000015"), ("-0", "0"), ("0.00", "0.00"), ("-1.0", "-1.0"), ("NaN", "NaN")] {
            assert_eq!(n(t).to_text(), want, "{t}");
        }
        assert!(Num::parse("1e200000").is_err());
        assert!(Num::parse("1e-20000").is_err());
        assert!(Num::parse("1.").is_ok() && Num::parse(".").is_err() && Num::parse("1x").is_err());
    }

    #[test]
    fn numerics_round_trip_through_the_binary_form() {
        for t in ["0", "0.00", "1.5", "-12345.6789", "0.000012", "100000000", "123456789.123456789", "-9223372036854775808", "99999999999999999999.9", "NaN", "Infinity", "-Infinity"] {
            let mut b = Vec::new();
            n(t).write_numeric(&mut b).unwrap();
            assert_eq!(Num::from_numeric(&b).unwrap().to_text(), t);
        }
    }

    #[test]
    fn numbers_compare_by_value() {
        use Ordering::*;
        for (a, b, o) in [("1", "1.0", Equal), ("0.1", "0.11", Less), ("-2", "-1.5", Less), ("10", "9.99", Greater), ("NaN", "Infinity", Greater), ("NaN", "NaN", Equal), ("-Infinity", "-1e100", Less), ("0", "-0.00", Equal), ("0.001", "-5", Greater)] {
            assert_eq!(n(a).cmp_value(&n(b)), o, "{a} vs {b}");
            assert_eq!(n(b).cmp_value(&n(a)), o.reverse(), "{b} vs {a}");
        }
    }

    #[test]
    fn floor_ceil_and_rounding_follow_numeric() {
        for (t, f, c) in [("-1.5", "-2", "-1"), ("1.2", "1", "2"), ("9.9", "9", "10"), ("0.5", "0", "1"), ("-0.5", "-1", "0"), ("7", "7", "7"), ("-99.01", "-100", "-99")] {
            assert_eq!(n(t).floor_ceil(false).to_text(), f, "floor {t}");
            assert_eq!(n(t).floor_ceil(true).to_text(), c, "ceil {t}");
        }
        for (t, r) in [("2.5", Some(3)), ("-2.5", Some(-3)), ("0.49", Some(0)), ("0.05", Some(0)), ("9223372036854775807.4", Some(i64::MAX)), ("9223372036854775807.5", None), ("-9223372036854775808", Some(i64::MIN)), ("NaN", None)] {
            assert_eq!(n(t).round_i64(), r, "{t}");
        }
        assert_eq!(n("-2147483648.9").trunc_i32(), Some(i32::MIN));
        assert_eq!(n("2147483648").trunc_i32(), None);
    }
}
