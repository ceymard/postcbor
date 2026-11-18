//! EDN input (design §5). Produces plain CBOR (containers in indefinite form) that normalization then canonicalizes.
//! Iterative: nesting costs heap, never stack.

use crate::bigint;
use crate::ctx::Ctx;
use crate::cursor::*;
use crate::encode::write_head;
use crate::error::{Error, ErrorKind, Result};
use crate::float;
use crate::normalize::{Duplicates, normalize_unpacking};
use crate::out::{OutBuf, try_push};

fn syntax(msg: &'static str, at: usize) -> Error {
    Error::new(ErrorKind::Syntax(msg), at)
}

/// Text source with bounds-checked access.
struct Src<'a> {
    s: &'a [u8],
    pos: usize,
}

impl<'a> Src<'a> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }
    fn peek_at(&self, k: usize) -> Option<u8> {
        self.s.get(self.pos + k).copied()
    }
    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }
    fn eat(&mut self, b: u8) -> bool {
        if self.peek() == Some(b) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn starts_with(&self, p: &[u8]) -> bool {
        self.s.get(self.pos..).is_some_and(|r| r.starts_with(p))
    }
    fn expect(&mut self, b: u8, msg: &'static str) -> Result<()> {
        if self.eat(b) { Ok(()) } else { Err(syntax(msg, self.pos)) }
    }
    /// Skips whitespace and comments (`# …` to end of line, `/ … /`).
    fn ws(&mut self) -> Result<()> {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n' | b'\r') => self.pos += 1,
                Some(b'#') => {
                    while !matches!(self.bump(), Some(b'\n') | None) {}
                }
                Some(b'/') => {
                    let at = self.pos;
                    self.pos += 1;
                    loop {
                        match self.bump() {
                            Some(b'/') => break,
                            Some(_) => {}
                            None => return Err(syntax("unterminated comment", at)),
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }
    /// Skips an encoding indicator (`_0`–`_3`, `_i`): accepted and ignored since values are normalized.
    fn indicator(&mut self) {
        if self.peek() == Some(b'_') && matches!(self.peek_at(1), Some(b'0'..=b'3' | b'i')) {
            self.pos += 2;
        }
    }
}

enum Frame {
    Array,
    Map { in_value: bool },
    Tag,
    /// `<< … >>`: normalized encodings of the items so far.
    Embedded { acc: Vec<u8> },
}

/// Parses EDN text into a single CBOR item appended to `out` (not yet normalized).
pub fn parse(text: &[u8], out: &mut Vec<u8>, ctx: &mut impl Ctx) -> Result<()> {
    let max_depth = ctx.limits().max_depth;
    let mut src = Src { s: text, pos: 0 };
    let mut stack: Vec<Frame> = Vec::new();
    let cur: &mut Vec<u8> = out;
    // One buffer per open `<<`: each item inside is normalized on its own before joining.
    let mut embedded: Vec<Vec<u8>> = Vec::new();

    'value: loop {
        src.ws()?;
        let at = src.pos;
        let target: &mut Vec<u8> = embedded.last_mut().unwrap_or(&mut *cur);
        let opened = value(&mut src, target)?;
        if opened.is_some() && stack.len() >= max_depth {
            return Err(Error::new(ErrorKind::DepthLimit, at));
        }
        if let Some(f) = opened {
            match f {
                Open::Array => {
                    try_push(&mut stack, Frame::Array)?;
                    src.ws()?;
                    if src.eat(b']') {
                        stack.pop();
                        embedded.last_mut().unwrap_or(&mut *cur).push(BREAK);
                    } else {
                        continue 'value;
                    }
                }
                Open::Map => {
                    try_push(&mut stack, Frame::Map { in_value: false })?;
                    src.ws()?;
                    if src.eat(b'}') {
                        stack.pop();
                        embedded.last_mut().unwrap_or(&mut *cur).push(BREAK);
                    } else {
                        continue 'value;
                    }
                }
                Open::Tag => {
                    try_push(&mut stack, Frame::Tag)?;
                    continue 'value;
                }
                Open::Embedded => {
                    src.ws()?;
                    if src.starts_with(b">>") {
                        src.pos += 2;
                        let t = embedded.last_mut().unwrap_or(&mut *cur);
                        t.push(0x40);
                    } else {
                        try_push(&mut stack, Frame::Embedded { acc: Vec::new() })?;
                        try_push(&mut embedded, Vec::new())?;
                        continue 'value;
                    }
                }
            }
        }
        // A value is complete: consume separators and closers of the enclosing frames.
        loop {
            src.ws()?;
            match stack.last_mut() {
                None => {
                    if src.peek().is_some() {
                        return Err(syntax("unexpected text after the value", src.pos));
                    }
                    return Ok(());
                }
                Some(Frame::Array) => {
                    if src.eat(b',') {
                        src.ws()?;
                        if src.peek() == Some(b']') {
                            return Err(syntax("trailing comma", src.pos));
                        }
                        continue 'value;
                    }
                    src.expect(b']', "expected ',' or ']'")?;
                    stack.pop();
                    embedded.last_mut().unwrap_or(&mut *cur).push(BREAK);
                }
                Some(Frame::Map { in_value }) => {
                    if !*in_value {
                        src.expect(b':', "expected ':' after map key")?;
                        *in_value = true;
                        continue 'value;
                    }
                    if src.eat(b',') {
                        *in_value = false;
                        src.ws()?;
                        if src.peek() == Some(b'}') {
                            return Err(syntax("trailing comma", src.pos));
                        }
                        continue 'value;
                    }
                    src.expect(b'}', "expected ',' or '}'")?;
                    stack.pop();
                    embedded.last_mut().unwrap_or(&mut *cur).push(BREAK);
                }
                Some(Frame::Tag) => {
                    src.expect(b')', "expected ')' after tag content")?;
                    stack.pop();
                }
                Some(Frame::Embedded { acc }) => {
                    let item = embedded.last_mut().ok_or(syntax("internal error", src.pos))?;
                    normalize_unpacking(item, acc, ctx, Duplicates::Reject)?;
                    item.clear();
                    if src.eat(b',') {
                        continue 'value;
                    }
                    if !src.starts_with(b">>") {
                        return Err(syntax("expected ',' or '>>'", src.pos));
                    }
                    src.pos += 2;
                    let Some(Frame::Embedded { acc }) = stack.pop() else {
                        return Err(syntax("internal error", src.pos));
                    };
                    embedded.pop();
                    let t = embedded.last_mut().unwrap_or(&mut *cur);
                    write_head(t, MAJOR_BYTES, acc.len() as u64)?;
                    t.write_bytes(&acc)?;
                }
            }
        }
    }
}

enum Open {
    Array,
    Map,
    Tag,
    Embedded,
}

/// Parses one value start. Scalars are written completely; containers write their opening head and return what opened.
fn value(src: &mut Src, out: &mut Vec<u8>) -> Result<Option<Open>> {
    let at = src.pos;
    let Some(b) = src.peek() else { return Err(syntax("expected a value", at)) };
    match b {
        b'[' => {
            src.pos += 1;
            src.ws()?;
            src.indicator();
            src.eat(b'_');
            out.write_byte(0x9f)?;
            Ok(Some(Open::Array))
        }
        b'{' => {
            src.pos += 1;
            src.ws()?;
            src.indicator();
            src.eat(b'_');
            out.write_byte(0xbf)?;
            Ok(Some(Open::Map))
        }
        b'<' if src.starts_with(b"<<") => {
            src.pos += 2;
            Ok(Some(Open::Embedded))
        }
        b'(' => {
            src.pos += 1;
            src.ws()?;
            src.expect(b'_', "expected '_' for an indefinite-length string")?;
            chunks(src, out)?;
            Ok(None)
        }
        b'"' => {
            let s = quoted(src, b'"')?;
            write_head(out, MAJOR_TEXT, s.len() as u64)?;
            out.write_bytes(&s)?;
            src.indicator();
            Ok(None)
        }
        b'\'' => {
            let s = quoted(src, b'\'')?;
            write_head(out, MAJOR_BYTES, s.len() as u64)?;
            out.write_bytes(&s)?;
            src.indicator();
            Ok(None)
        }
        b'-' | b'+' | b'0'..=b'9' => number(src, out),
        b'a'..=b'z' | b'A'..=b'Z' => word(src, out),
        _ => Err(syntax("expected a value", at)),
    }
}

/// `(_ "a", "b")` → one definite string with the chunks joined.
fn chunks(src: &mut Src, out: &mut Vec<u8>) -> Result<()> {
    let mut joined: Vec<u8> = Vec::new();
    let mut major: Option<u8> = None;
    loop {
        src.ws()?;
        if src.eat(b')') {
            break;
        }
        if major.is_some() {
            src.expect(b',', "expected ',' between chunks")?;
            src.ws()?;
        }
        let at = src.pos;
        let mut tmp = Vec::new();
        if value(src, &mut tmp)?.is_some() {
            return Err(syntax("chunks must be strings", at));
        }
        let mut c = Cursor::new(&tmp);
        let h = c.head()?;
        if !matches!(h.major, MAJOR_BYTES | MAJOR_TEXT) || major.is_some_and(|m| m != h.major) {
            return Err(syntax("chunks must be strings of one type", at));
        }
        major = Some(h.major);
        joined.write_bytes(c.take(h.arg)?)?;
    }
    write_head(out, major.unwrap_or(MAJOR_BYTES), joined.len() as u64)?;
    out.write_bytes(&joined)
}

/// Contents of a `"…"` or `'…'` literal with JSON escapes.
fn quoted(src: &mut Src, q: u8) -> Result<Vec<u8>> {
    let start = src.pos;
    src.pos += 1;
    let mut s: Vec<u8> = Vec::new();
    loop {
        let at = src.pos;
        match src.bump() {
            None => return Err(syntax("unterminated string", start)),
            Some(c) if c == q => break,
            Some(b'\\') => {
                let e = src.bump().ok_or(syntax("unterminated string", start))?;
                let ch = match e {
                    b'"' => '"',
                    b'\'' => '\'',
                    b'\\' => '\\',
                    b'/' => '/',
                    b'b' => '\u{8}',
                    b'f' => '\u{c}',
                    b'n' => '\n',
                    b'r' => '\r',
                    b't' => '\t',
                    b'u' => {
                        let hi = hex4(src)?;
                        let cp = if (0xd800..0xdc00).contains(&hi) {
                            if !(src.eat(b'\\') && src.eat(b'u')) {
                                return Err(syntax("unpaired surrogate", at));
                            }
                            let lo = hex4(src)?;
                            if !(0xdc00..0xe000).contains(&lo) {
                                return Err(syntax("unpaired surrogate", at));
                            }
                            0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                        } else {
                            hi
                        };
                        char::from_u32(cp).ok_or(syntax("unpaired surrogate", at))?
                    }
                    _ => return Err(syntax("invalid escape", at)),
                };
                let mut b = [0u8; 4];
                s.write_bytes(ch.encode_utf8(&mut b).as_bytes())?;
            }
            Some(c) => s.write_byte(c)?,
        }
    }
    if q == b'"' && core::str::from_utf8(&s).is_err() {
        return Err(syntax("invalid UTF-8 in text", start));
    }
    Ok(s)
}

fn hex4(src: &mut Src) -> Result<u32> {
    let mut v = 0;
    for _ in 0..4 {
        let at = src.pos;
        let d = src.bump().and_then(|b| (b as char).to_digit(16)).ok_or(syntax("invalid \\u escape", at))?;
        v = v * 16 + d;
    }
    Ok(v)
}

/// Integers (any size, decimal or 0x/0o/0b), floats, ±Infinity, and tag numbers `N(`.
fn number(src: &mut Src, out: &mut Vec<u8>) -> Result<Option<Open>> {
    let start = src.pos;
    let neg = src.eat(b'-');
    if !neg {
        src.eat(b'+');
    }
    if src.starts_with(b"Infinity") {
        src.pos += 8;
        write_float(out, if neg { f64::NEG_INFINITY } else { f64::INFINITY })?;
        src.indicator();
        return Ok(None);
    }
    let radix = if src.peek() == Some(b'0') {
        match src.peek_at(1) {
            Some(b'x' | b'X') => 16,
            Some(b'o' | b'O') => 8,
            Some(b'b' | b'B') => 2,
            _ => 10,
        }
    } else {
        10
    };
    if radix != 10 {
        src.pos += 2;
    }
    let ds = src.pos;
    while src.peek().is_some_and(|b| (b as char).is_digit(radix)) {
        src.pos += 1;
    }
    let digits = src.s.get(ds..src.pos).unwrap_or_default();
    if digits.is_empty() {
        return Err(syntax("expected digits", src.pos));
    }
    let is_float = radix == 10 && matches!(src.peek(), Some(b'.' | b'e' | b'E'));
    if radix == 16 && matches!(src.peek(), Some(b'.' | b'p' | b'P')) {
        return Err(syntax("hexadecimal floats are not supported", start));
    }
    if is_float {
        if src.eat(b'.') {
            let fs = src.pos;
            while src.peek().is_some_and(|b| b.is_ascii_digit()) {
                src.pos += 1;
            }
            if src.pos == fs {
                return Err(syntax("expected digits after '.'", src.pos));
            }
        }
        if src.eat(b'e') || src.eat(b'E') {
            if !src.eat(b'-') {
                src.eat(b'+');
            }
            let es = src.pos;
            while src.peek().is_some_and(|b| b.is_ascii_digit()) {
                src.pos += 1;
            }
            if src.pos == es {
                return Err(syntax("expected exponent digits", src.pos));
            }
        }
        let text = core::str::from_utf8(src.s.get(start..src.pos).unwrap_or_default())
            .map_err(|_| syntax("invalid number", start))?;
        let f: f64 = text.parse().map_err(|_| syntax("invalid number", start))?;
        write_float(out, f)?;
        src.indicator();
        return Ok(None);
    }
    let mut mag = bigint::digits_to_be(digits, radix).map_err(|e| Error::new(e.kind, start))?;
    if src.peek() == Some(b'(') {
        if neg || mag.len() > 8 {
            return Err(syntax("invalid tag number", start));
        }
        src.pos += 1;
        write_head(out, MAJOR_TAG, be_u64(&mag))?;
        return Ok(Some(Open::Tag));
    }
    if neg {
        if mag.is_empty() {
            // -0 is the integer 0.
            write_head(out, MAJOR_UNSIGNED, 0)?;
        } else {
            bigint::minus_one(&mut mag);
            write_int(out, true, &mag)?;
        }
    } else {
        write_int(out, false, &mag)?;
    }
    src.indicator();
    Ok(None)
}

fn be_u64(m: &[u8]) -> u64 {
    m.iter().fold(0u64, |a, &b| (a << 8) | u64::from(b))
}

/// Writes the integer (or −1 − mag) as a plain integer or a bignum.
fn write_int(out: &mut Vec<u8>, neg: bool, mag: &[u8]) -> Result<()> {
    if mag.len() <= 8 {
        return write_head(out, if neg { MAJOR_NEGATIVE } else { MAJOR_UNSIGNED }, be_u64(mag));
    }
    write_head(out, MAJOR_TAG, if neg { 3 } else { 2 })?;
    write_head(out, MAJOR_BYTES, mag.len() as u64)?;
    out.write_bytes(mag)
}

fn write_float(out: &mut Vec<u8>, f: f64) -> Result<()> {
    let mut b = [0u8; 9];
    let n = float::encode_shortest(f, &mut b);
    out.write_bytes(&b[..n])
}

/// Keywords, `simple(n)`, and application literals (`h''`, `b64''`, `dt''`, `ip''`, …).
fn word(src: &mut Src, out: &mut Vec<u8>) -> Result<Option<Open>> {
    let start = src.pos;
    while src.peek().is_some_and(|b| b.is_ascii_alphanumeric()) {
        src.pos += 1;
    }
    let w = src.s.get(start..src.pos).unwrap_or_default();
    if src.peek() == Some(b'\'') {
        let body = app_body(src)?;
        match w {
            b"h" => {
                let bytes = hex_body(&body, start)?;
                write_head(out, MAJOR_BYTES, bytes.len() as u64)?;
                out.write_bytes(&bytes)?;
            }
            b"b64" => {
                let bytes = b64_body(&body, start)?;
                write_head(out, MAJOR_BYTES, bytes.len() as u64)?;
                out.write_bytes(&bytes)?;
            }
            b"dt" | b"DT" => {
                if w == b"DT" {
                    write_head(out, MAJOR_TAG, 1)?;
                }
                super::literals::datetime(&body, start, out)?;
            }
            b"ip" | b"IP" => super::literals::ip(&body, start, w == b"IP", out)?,
            _ => return Err(syntax("unsupported application literal", start)),
        }
        src.indicator();
        return Ok(None);
    }
    match w {
        b"false" => out.write_byte(0xf4)?,
        b"true" => out.write_byte(0xf5)?,
        b"null" => out.write_byte(0xf6)?,
        b"undefined" => out.write_byte(0xf7)?,
        b"NaN" => out.write_bytes(&float::CANONICAL_NAN)?,
        b"Infinity" => write_float(out, f64::INFINITY)?,
        b"simple" => {
            src.ws()?;
            src.expect(b'(', "expected '(' after simple")?;
            src.ws()?;
            let ds = src.pos;
            while src.peek().is_some_and(|b| b.is_ascii_digit()) {
                src.pos += 1;
            }
            let n: u32 = core::str::from_utf8(src.s.get(ds..src.pos).unwrap_or_default())
                .ok()
                .and_then(|t| t.parse().ok())
                .filter(|&n| n <= 255 && !(24..32).contains(&n))
                .ok_or(syntax("invalid simple value", ds))?;
            src.ws()?;
            src.expect(b')', "expected ')' after simple value")?;
            if n < 24 {
                out.write_byte(0xe0 | n as u8)?;
            } else {
                out.write_bytes(&[0xf8, n as u8])?;
            }
        }
        _ => return Err(syntax("unknown word", start)),
    }
    src.indicator();
    Ok(None)
}

/// Raw text between the quotes of an application literal, with `\'` and `\\` unescaped.
fn app_body(src: &mut Src) -> Result<Vec<u8>> {
    let start = src.pos;
    src.pos += 1;
    let mut s = Vec::new();
    loop {
        match src.bump() {
            None => return Err(syntax("unterminated literal", start)),
            Some(b'\'') => return Ok(s),
            Some(b'\\') => s.write_byte(src.bump().ok_or(syntax("unterminated literal", start))?)?,
            Some(c) => s.write_byte(c)?,
        }
    }
}

/// Hex digits, allowing whitespace and comments between them.
fn hex_body(body: &[u8], at: usize) -> Result<Vec<u8>> {
    let mut s = Src { s: body, pos: 0 };
    let mut out = Vec::new();
    let mut hi: Option<u32> = None;
    loop {
        s.ws().map_err(|e| Error::new(e.kind, at))?;
        let Some(b) = s.bump() else { break };
        let d = (b as char).to_digit(16).ok_or(syntax("invalid hex digit", at))?;
        match hi.take() {
            None => hi = Some(d),
            Some(h) => out.write_byte((h * 16 + d) as u8)?,
        }
    }
    if hi.is_some() {
        return Err(syntax("odd number of hex digits", at));
    }
    Ok(out)
}

/// Base64, standard or URL-safe alphabet, padding optional.
fn b64_body(body: &[u8], at: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    let mut padding = false;
    for &b in body {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => {
                padding = true;
                continue;
            }
            b' ' | b'\t' | b'\n' | b'\r' => continue,
            _ => return Err(syntax("invalid base64", at)),
        };
        if padding {
            return Err(syntax("invalid base64", at));
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.write_byte((acc >> bits) as u8)?;
            acc &= (1 << bits) - 1;
        }
    }
    if acc != 0 {
        return Err(syntax("invalid base64", at));
    }
    Ok(out)
}
