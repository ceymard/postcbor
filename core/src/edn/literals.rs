//! `dt''`/`DT''` and `ip''`/`IP''` application literals (draft-ietf-cbor-edn-literals-29).

use crate::cursor::*;
use crate::encode::write_head;
use crate::error::{Error, ErrorKind, Result};
use crate::float;
use crate::out::OutBuf;

fn bad(msg: &'static str, at: usize) -> Error {
    Error::new(ErrorKind::Syntax(msg), at)
}

fn num(s: &[u8], at: usize) -> Result<i64> {
    if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
        return Err(bad("invalid date-time", at));
    }
    Ok(s.iter().fold(0i64, |a, &b| a * 10 + i64::from(b - b'0')))
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// RFC 3339 date-time → epoch seconds: an integer, or a float when there is a fractional part.
pub fn datetime(s: &[u8], at: usize, out: &mut Vec<u8>) -> Result<()> {
    let e = || bad("invalid date-time", at);
    let g = |a: usize, b: usize| s.get(a..b).ok_or_else(e);
    let (y, m, d) = (num(g(0, 4)?, at)?, num(g(5, 7)?, at)?, num(g(8, 10)?, at)?);
    if g(4, 5)? != b"-" || g(7, 8)? != b"-" || !matches!(g(10, 11)?, b"T" | b"t") || g(13, 14)? != b":" || g(16, 17)? != b":" {
        return Err(e());
    }
    let (hh, mm, ss) = (num(g(11, 13)?, at)?, num(g(14, 16)?, at)?, num(g(17, 19)?, at)?);
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) || hh > 23 || mm > 59 || ss > 60 {
        return Err(e());
    }
    let mut p = 19;
    let mut frac: Option<&[u8]> = None;
    if s.get(p) == Some(&b'.') {
        let fs = p + 1;
        p = fs;
        while s.get(p).is_some_and(u8::is_ascii_digit) {
            p += 1;
        }
        if p == fs {
            return Err(e());
        }
        frac = s.get(fs..p);
    }
    let offset = match s.get(p) {
        Some(b'Z' | b'z') if p + 1 == s.len() => 0,
        Some(&sign @ (b'+' | b'-')) if p + 6 == s.len() => {
            let (oh, om) = (num(g(p + 1, p + 3)?, at)?, num(g(p + 4, p + 6)?, at)?);
            if g(p + 3, p + 4)? != b":" || oh > 23 || om > 59 {
                return Err(e());
            }
            let o = oh * 3600 + om * 60;
            if sign == b'-' { -o } else { o }
        }
        _ => return Err(e()),
    };
    let secs = days_from_civil(y, m, d) * 86400 + hh * 3600 + mm * 60 + ss - offset;
    match frac {
        None => {
            if secs >= 0 {
                write_head(out, MAJOR_UNSIGNED, secs as u64)
            } else {
                write_head(out, MAJOR_NEGATIVE, (-1 - secs) as u64)
            }
        }
        Some(f) => {
            // Build the exact decimal of secs + 0.f, then round once to f64.
            let text = if secs >= 0 || f.iter().all(|&c| c == b'0') {
                format!("{secs}.{}", core::str::from_utf8(f).map_err(|_| e())?)
            } else {
                // Negative: secs + 0.f = -((-secs - 1) + (1 - 0.f)); 1 - 0.f is the ten's complement of f.
                let mut comp = f.to_vec();
                let mut borrow = false;
                for c in comp.iter_mut().rev() {
                    let d = i32::from(*c - b'0') + i32::from(borrow);
                    let r = if d == 0 { 0 } else { 10 - d };
                    borrow = d != 0;
                    *c = b'0' + r as u8;
                }
                format!("-{}.{}", -secs - 1, core::str::from_utf8(&comp).map_err(|_| e())?)
            };
            let v: f64 = text.parse().map_err(|_| e())?;
            let mut b = [0u8; 9];
            let n = float::encode_shortest(v, &mut b);
            out.write_bytes(&b[..n])
        }
    }
}

/// IPv4/IPv6 address or prefix. `tagged` wraps it in tag 52 (IPv4) or 54 (IPv6).
pub fn ip(s: &[u8], at: usize, tagged: bool, out: &mut Vec<u8>) -> Result<()> {
    let e = || bad("invalid IP address", at);
    let text = core::str::from_utf8(s).map_err(|_| e())?;
    let (addr, prefix) = match text.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (text, None),
    };
    let (bytes, v6): (Vec<u8>, bool) = if addr.contains(':') {
        (parse_v6(addr).ok_or_else(e)?.to_vec(), true)
    } else {
        (parse_v4(addr).ok_or_else(e)?.to_vec(), false)
    };
    if tagged {
        write_head(out, MAJOR_TAG, if v6 { 54 } else { 52 })?;
    }
    let Some(p) = prefix else {
        write_head(out, MAJOR_BYTES, bytes.len() as u64)?;
        return out.write_bytes(&bytes);
    };
    let n: usize = p.parse().ok().filter(|&n| n <= bytes.len() * 8).ok_or_else(e)?;
    // Bits past the prefix must be zero; trailing zero bytes are omitted.
    for (i, &b) in bytes.iter().enumerate() {
        let keep = n.saturating_sub(i * 8).min(8);
        let mask = if keep == 0 { 0u8 } else { 0xffu8 << (8 - keep) };
        if b & !mask != 0 {
            return Err(bad("IP prefix has bits set past its length", at));
        }
    }
    let mut len = n.div_ceil(8);
    while len > 0 && bytes.get(len - 1) == Some(&0) {
        len -= 1;
    }
    out.write_byte(0x82)?;
    write_head(out, MAJOR_UNSIGNED, n as u64)?;
    write_head(out, MAJOR_BYTES, len as u64)?;
    out.write_bytes(bytes.get(..len).unwrap_or_default())
}

fn parse_v4(s: &str) -> Option<[u8; 4]> {
    let mut b = [0u8; 4];
    let mut parts = s.split('.');
    for slot in &mut b {
        let p = parts.next()?;
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        *slot = p.parse().ok()?;
    }
    parts.next().is_none().then_some(b)
}

fn parse_v6(s: &str) -> Option<[u8; 16]> {
    let (head, tail) = match s.split_once("::") {
        Some((h, t)) => (h, Some(t)),
        None => (s, None),
    };
    let groups = |part: &str| -> Option<Vec<u16>> {
        if part.is_empty() {
            return Some(Vec::new());
        }
        let mut v = Vec::new();
        let pieces: Vec<&str> = part.split(':').collect();
        for (i, g) in pieces.iter().enumerate() {
            if i == pieces.len() - 1 && g.contains('.') {
                let b = parse_v4(g)?;
                v.push(u16::from_be_bytes([b[0], b[1]]));
                v.push(u16::from_be_bytes([b[2], b[3]]));
            } else {
                if g.is_empty() || g.len() > 4 {
                    return None;
                }
                v.push(u16::from_str_radix(g, 16).ok()?);
            }
        }
        Some(v)
    };
    let h = groups(head)?;
    let mut words = [0u16; 8];
    match tail {
        None => {
            if h.len() != 8 {
                return None;
            }
            words.copy_from_slice(&h);
        }
        Some(t) => {
            let t = groups(t)?;
            if h.len() + t.len() > 7 {
                return None;
            }
            words[..h.len()].copy_from_slice(&h);
            words[8 - t.len()..].copy_from_slice(&t);
        }
    }
    let mut b = [0u8; 16];
    for (i, w) in words.iter().enumerate() {
        b[i * 2..i * 2 + 2].copy_from_slice(&w.to_be_bytes());
    }
    Some(b)
}
