//! EDN output (design §5): core diagnostic notation only, deterministic.

use crate::cursor::*;
use crate::error::{Error, ErrorKind, Result};
use crate::float;
use crate::out::OutBuf;
use crate::value::Value;

#[derive(Clone, Copy, Debug, Default)]
pub struct PrintOpts {
    /// One entry per line, 4-space indentation, like `jsonb_pretty`.
    pub pretty: bool,
    /// Escape every non-ASCII character, for databases whose encoding is not UTF-8.
    pub ascii_only: bool,
}

fn corrupt(at: usize) -> Error {
    Error::new(ErrorKind::CorruptDatum, at)
}

enum Frame {
    Container { remaining: u64, done: u64, is_map: bool },
    Tag,
}

/// Appends the EDN text of `v` to `out`.
pub fn print(v: &Value, out: &mut impl OutBuf, opts: PrintOpts) -> Result<()> {
    let mut c = Cursor::at(v.d.body, v.pos);
    let mut stack: Vec<Frame> = Vec::new();
    let mut ticker = crate::ctx::Ticker::default();
    loop {
        ticker.tick();
        // Separator before the next item of the enclosing container.
        let depth = stack.iter().filter(|f| matches!(f, Frame::Container { .. })).count();
        if let Some(Frame::Container { done, is_map, .. }) = stack.last() {
            if *is_map && done % 2 == 1 {
                out.write_bytes(b": ")?;
            } else if *done > 0 {
                out.write_bytes(if opts.pretty { b"," } else { b", " })?;
                newline(out, opts, depth)?;
            } else {
                newline(out, opts, depth)?;
            }
        }
        let h = c.head().map_err(|e| corrupt(e.offset))?;
        let mut completed = true;
        match h.major {
            MAJOR_UNSIGNED => write_u64(out, h.arg)?,
            MAJOR_NEGATIVE => {
                out.write_byte(b'-')?;
                // −1 − n, printed without overflow.
                if h.arg == u64::MAX {
                    out.write_bytes(b"18446744073709551616")?;
                } else {
                    write_u64(out, h.arg + 1)?;
                }
            }
            MAJOR_BYTES => {
                let s = c.take(h.arg).map_err(|e| corrupt(e.offset))?;
                write_hex(out, s)?;
            }
            MAJOR_TEXT => {
                let s = c.take(h.arg).map_err(|e| corrupt(e.offset))?;
                let s = core::str::from_utf8(s).map_err(|_| corrupt(c.pos()))?;
                write_text(out, s, opts.ascii_only)?;
            }
            MAJOR_ARRAY | MAJOR_MAP => {
                let is_map = h.major == MAJOR_MAP;
                let (open, close) = if is_map { (b'{', b'}') } else { (b'[', b']') };
                out.write_byte(open)?;
                if h.arg == 0 {
                    out.write_byte(close)?;
                } else {
                    let remaining = if is_map { h.arg.checked_mul(2).ok_or(corrupt(c.pos()))? } else { h.arg };
                    stack.push(Frame::Container { remaining, done: 0, is_map });
                    completed = false;
                }
            }
            MAJOR_TAG => {
                write_u64(out, h.arg)?;
                out.write_byte(b'(')?;
                stack.push(Frame::Tag);
                completed = false;
            }
            _ => match h.info {
                20 => out.write_bytes(b"false")?,
                21 => out.write_bytes(b"true")?,
                22 => out.write_bytes(b"null")?,
                23 => out.write_bytes(b"undefined")?,
                25..=27 => write_float(out, float::decode(h.info, h.arg))?,
                _ => {
                    out.write_bytes(b"simple(")?;
                    write_u64(out, h.arg)?;
                    out.write_byte(b')')?;
                }
            },
        }
        if !completed {
            continue;
        }
        loop {
            match stack.last_mut() {
                None => return Ok(()),
                Some(Frame::Tag) => {
                    stack.pop();
                    out.write_byte(b')')?;
                }
                Some(Frame::Container { remaining, done, is_map }) => {
                    *remaining -= 1;
                    *done += 1;
                    if *remaining > 0 {
                        break;
                    }
                    let close = if *is_map { b'}' } else { b']' };
                    stack.pop();
                    let depth = stack.iter().filter(|f| matches!(f, Frame::Container { .. })).count();
                    newline(out, opts, depth)?;
                    out.write_byte(close)?;
                }
            }
        }
    }
}

fn newline(out: &mut impl OutBuf, opts: PrintOpts, depth: usize) -> Result<()> {
    if opts.pretty {
        out.write_byte(b'\n')?;
        for _ in 0..depth {
            out.write_bytes(b"    ")?;
        }
    }
    Ok(())
}

fn write_u64(out: &mut impl OutBuf, mut v: u64) -> Result<()> {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out.write_bytes(&buf[i..])
}

/// Shortest decimal that reads back as the same f64, always marked as a float.
pub fn write_float(out: &mut impl OutBuf, f: f64) -> Result<()> {
    if f.is_nan() {
        return out.write_bytes(b"NaN");
    }
    if f.is_infinite() {
        return out.write_bytes(if f > 0.0 { b"Infinity" } else { b"-Infinity" });
    }
    // Debug formatting is the shortest round-trip form and keeps ".0" or an exponent.
    let s = format!("{f:?}");
    out.write_bytes(s.as_bytes())
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn write_hex(out: &mut impl OutBuf, s: &[u8]) -> Result<()> {
    out.write_bytes(b"h'")?;
    for &b in s {
        out.write_bytes(&[HEX[(b >> 4) as usize], HEX[(b & 15) as usize]])?;
    }
    out.write_byte(b'\'')
}

/// JSON-style quoted text.
pub fn write_text(out: &mut impl OutBuf, s: &str, ascii_only: bool) -> Result<()> {
    out.write_byte(b'"')?;
    let mut run_start = 0;
    let bytes = s.as_bytes();
    for (i, ch) in s.char_indices() {
        let esc: Option<&[u8]> = match ch {
            '"' => Some(b"\\\""),
            '\\' => Some(b"\\\\"),
            '\n' => Some(b"\\n"),
            '\r' => Some(b"\\r"),
            '\t' => Some(b"\\t"),
            '\u{8}' => Some(b"\\b"),
            '\u{c}' => Some(b"\\f"),
            _ => None,
        };
        let needs_u = esc.is_none() && ((ch as u32) < 0x20 || (ascii_only && !ch.is_ascii()));
        if esc.is_none() && !needs_u {
            continue;
        }
        out.write_bytes(&bytes[run_start..i])?;
        run_start = i + ch.len_utf8();
        if let Some(e) = esc {
            out.write_bytes(e)?;
        } else {
            let mut units = [0u16; 2];
            for u in ch.encode_utf16(&mut units) {
                let u = *u;
                out.write_bytes(&[
                    b'\\',
                    b'u',
                    HEX[(u >> 12) as usize],
                    HEX[((u >> 8) & 15) as usize],
                    HEX[((u >> 4) & 15) as usize],
                    HEX[(u & 15) as usize],
                ])?;
            }
        }
    }
    out.write_bytes(&bytes[run_start..])?;
    out.write_byte(b'"')
}
