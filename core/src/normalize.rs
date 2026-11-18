//! Untrusted CBOR → deterministic encoding (design §3). Iterative: nesting depth costs heap, never stack.

use crate::ctx::{Ctx, INTERRUPT_EVERY};
use crate::cursor::*;
use crate::encode::{insert_head, write_head};
use crate::error::{Error, ErrorKind, Result};
use crate::float;
use crate::index::INDEX_THRESHOLD;
use crate::out::{OutBuf, try_push};

enum Frame {
    Array {
        remaining: Option<u64>,
        start: usize,
        count: u64,
    },
    Map {
        remaining: Option<u64>,
        start: usize,
        count: u64,
        entries_base: usize,
        next_start: usize,
        pending_key: Option<(usize, usize)>,
    },
    Tag,
}

/// Byte ranges (in the output) of one written map entry.
#[derive(Clone, Copy)]
struct Entry {
    key_start: usize,
    key_end: usize,
    end: usize,
}

/// What to do with duplicate map keys.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Duplicates {
    /// Untrusted input: duplicates are an error.
    Reject,
    /// Builders and aggregates: the last entry wins, as in jsonb.
    KeepLast,
}

/// Normalizes one CBOR item from `input`, appending its deterministic encoding to `out`.
/// Returns true when some container reached [`INDEX_THRESHOLD`] entries, so an index section is needed.
pub fn normalize(input: &[u8], out: &mut impl OutBuf, ctx: &mut impl Ctx) -> Result<bool> {
    normalize_with(input, out, ctx, Duplicates::Reject)
}

/// [`normalize_with`], unpacking packed input when normalization meets a packing tag (rare, so plain input pays nothing).
pub fn normalize_unpacking(input: &[u8], out: &mut impl OutBuf, ctx: &mut impl Ctx, dups: Duplicates) -> Result<bool> {
    let base = out.len();
    match normalize_with(input, out, ctx, dups) {
        Err(e) if e.kind == ErrorKind::Packed => {
            out.truncate(base);
            let plain = crate::pack::unpack(input, ctx)?;
            normalize_with(&plain, out, ctx, dups)
        }
        r => r,
    }
}

/// [`normalize`] with a duplicate-key policy. Packed input fails with the internal `Packed` signal.
pub fn normalize_with(input: &[u8], out: &mut impl OutBuf, ctx: &mut impl Ctx, dups: Duplicates) -> Result<bool> {
    let limits = *ctx.limits();
    let max_body = limits.max_body(input.len());
    let body_start = out.len();
    let mut c = Cursor::new(input);
    let mut stack: Vec<Frame> = Vec::new();
    let mut entries: Vec<Entry> = Vec::new();
    let mut needs_index = false;
    let mut items: u32 = 0;

    loop {
        let completed = match stack.last() {
            Some(Frame::Array { remaining: Some(0), .. }) | Some(Frame::Map { remaining: Some(0), .. }) => {
                close(&mut stack, &mut entries, out, &mut needs_index, c.pos(), dups)?;
                true
            }
            Some(Frame::Array { remaining: None, .. }) | Some(Frame::Map { remaining: None, .. })
                if c.peek()? == BREAK =>
            {
                c.byte()?;
                close(&mut stack, &mut entries, out, &mut needs_index, c.pos(), dups)?;
                true
            }
            _ => {
                items = items.wrapping_add(1);
                if items.is_multiple_of(INTERRUPT_EVERY) {
                    ctx.check_interrupts();
                }
                read_item(&mut c, out, &mut stack, entries.len(), &mut needs_index, limits.max_depth)?
            }
        };

        if out.len() - body_start > max_body {
            return Err(c.err(ErrorKind::ExpansionLimit));
        }
        if !completed {
            continue;
        }
        // An item ended: account for it in the enclosing frames.
        loop {
            match stack.last_mut() {
                None => {
                    if !c.is_empty() {
                        return Err(c.err(ErrorKind::TrailingBytes));
                    }
                    return Ok(needs_index);
                }
                Some(Frame::Tag) => {
                    stack.pop();
                }
                Some(Frame::Array { remaining, count, .. }) => {
                    *count += 1;
                    if let Some(r) = remaining {
                        *r -= 1;
                    }
                    break;
                }
                Some(Frame::Map { remaining, count, next_start, pending_key, .. }) => {
                    match pending_key.take() {
                        None => *pending_key = Some((*next_start, out.len())),
                        Some((key_start, key_end)) => {
                            try_push(&mut entries, Entry { key_start, key_end, end: out.len() })?;
                            *next_start = out.len();
                            *count += 1;
                            if let Some(r) = remaining {
                                *r -= 1;
                            }
                        }
                    }
                    break;
                }
            }
        }
    }
}

/// Reads one item head and its scalar content. Returns false when it opened a container or tag frame.
fn read_item(
    c: &mut Cursor,
    out: &mut impl OutBuf,
    stack: &mut Vec<Frame>,
    entries_len: usize,
    needs_index: &mut bool,
    max_depth: usize,
) -> Result<bool> {
    let item_pos = c.pos();
    let h = c.head()?;
    match h.major {
        MAJOR_UNSIGNED | MAJOR_NEGATIVE => {
            write_head(out, h.major, h.arg)?;
            Ok(true)
        }
        MAJOR_BYTES | MAJOR_TEXT => {
            read_string(c, out, h)?;
            Ok(true)
        }
        MAJOR_ARRAY | MAJOR_MAP => {
            if stack.len() >= max_depth {
                return Err(Error::new(ErrorKind::DepthLimit, item_pos));
            }
            let remaining = if h.is_indefinite() {
                None
            } else {
                // Each item takes at least one byte: a larger count is a lie we must not size anything from.
                let per = if h.major == MAJOR_MAP { 2 } else { 1 };
                if h.arg > (c.remaining() / per) as u64 {
                    return Err(Error::new(ErrorKind::LengthTooLarge, item_pos));
                }
                write_head(out, h.major, h.arg)?;
                if h.arg >= INDEX_THRESHOLD {
                    *needs_index = true;
                }
                Some(h.arg)
            };
            let start = out.len();
            let frame = if h.major == MAJOR_ARRAY {
                Frame::Array { remaining, start, count: 0 }
            } else {
                Frame::Map {
                    remaining,
                    start,
                    count: 0,
                    entries_base: entries_len,
                    next_start: start,
                    pending_key: None,
                }
            };
            try_push(stack, frame)?;
            Ok(false)
        }
        MAJOR_TAG if h.arg == 2 || h.arg == 3 => {
            bignum(c, out, h.arg)?;
            Ok(true)
        }
        MAJOR_TAG if crate::pack::is_packing_tag(h.arg) => Err(Error::new(ErrorKind::Packed, item_pos)),
        MAJOR_TAG => {
            if stack.len() >= max_depth {
                return Err(Error::new(ErrorKind::DepthLimit, item_pos));
            }
            write_head(out, MAJOR_TAG, h.arg)?;
            try_push(stack, Frame::Tag)?;
            Ok(false)
        }
        _ => {
            match h.info {
                0..=23 => out.write_byte(0xe0 | h.info)?,
                24 if h.arg < 32 => return Err(Error::new(ErrorKind::InvalidSimple, item_pos)),
                24 => out.write_bytes(&[0xf8, h.arg as u8])?,
                25..=27 => {
                    let mut b = [0u8; 9];
                    let n = float::encode_shortest(float::decode(h.info, h.arg), &mut b);
                    out.write_bytes(&b[..n])?;
                }
                _ => return Err(Error::new(ErrorKind::UnexpectedBreak, item_pos)),
            }
            Ok(true)
        }
    }
}

fn read_string(c: &mut Cursor, out: &mut impl OutBuf, h: Head) -> Result<()> {
    if !h.is_indefinite() {
        let at = c.pos();
        let s = c.take(h.arg)?;
        check_utf8(h.major, s, at)?;
        write_head(out, h.major, h.arg)?;
        return out.write_bytes(s);
    }
    let start = out.len();
    let mut total: u64 = 0;
    loop {
        let at = c.pos();
        if c.peek()? == BREAK {
            c.byte()?;
            break;
        }
        let ch = c.head()?;
        if ch.major != h.major || ch.is_indefinite() {
            return Err(Error::new(ErrorKind::InvalidChunk, at));
        }
        let s = c.take(ch.arg)?;
        check_utf8(h.major, s, at)?;
        out.write_bytes(s)?;
        total += ch.arg;
    }
    insert_head(out, start, h.major, total)
}

fn check_utf8(major: u8, s: &[u8], at: usize) -> Result<()> {
    if major == MAJOR_TEXT && core::str::from_utf8(s).is_err() {
        return Err(Error::new(ErrorKind::InvalidUtf8, at));
    }
    Ok(())
}

/// Tag 2/3: strips leading zeros and stores values that fit in 64 bits as plain integers.
fn bignum(c: &mut Cursor, out: &mut impl OutBuf, tag: u64) -> Result<()> {
    let at = c.pos();
    let h = c.head()?;
    if h.major != MAJOR_BYTES {
        return Err(Error::new(ErrorKind::InvalidBignum, at));
    }
    let mut joined: Vec<u8> = Vec::new();
    let bytes: &[u8] = if h.is_indefinite() {
        read_string(c, &mut joined, h)?;
        // `joined` holds head + content; skip the head we just wrote.
        let mut jc = Cursor::new(&joined);
        let jh = jc.head()?;
        jc.take(jh.arg)?
    } else {
        c.take(h.arg)?
    };
    let first = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    let mut bc = Cursor::new(bytes);
    bc.take(first as u64)?;
    let digits = bc.take(bc.remaining() as u64)?;
    if digits.len() <= 8 {
        let v = digits.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
        let major = if tag == 2 { MAJOR_UNSIGNED } else { MAJOR_NEGATIVE };
        return write_head(out, major, v);
    }
    write_head(out, MAJOR_TAG, tag)?;
    write_head(out, MAJOR_BYTES, digits.len() as u64)?;
    out.write_bytes(digits)
}

fn close(
    stack: &mut Vec<Frame>,
    entries: &mut Vec<Entry>,
    out: &mut impl OutBuf,
    needs_index: &mut bool,
    pos: usize,
    dups: Duplicates,
) -> Result<()> {
    match stack.pop() {
        Some(Frame::Array { remaining, start, count }) => {
            if remaining.is_none() {
                insert_head(out, start, MAJOR_ARRAY, count)?;
                if count >= INDEX_THRESHOLD {
                    *needs_index = true;
                }
            }
            Ok(())
        }
        Some(Frame::Map { remaining, start, count, entries_base, pending_key, .. }) => {
            if pending_key.is_some() {
                return Err(Error::new(ErrorKind::MissingValue, pos));
            }
            let removed = sort_entries(out, &entries[entries_base..], dups)?;
            entries.truncate(entries_base);
            let kept = count - removed;
            if remaining.is_none() {
                insert_head(out, start, MAJOR_MAP, kept)?;
            } else if removed > 0 {
                shrink_head(out, start, count, kept)?;
            }
            if kept >= INDEX_THRESHOLD {
                *needs_index = true;
            }
            Ok(())
        }
        _ => Err(Error::new(ErrorKind::CorruptDatum, pos)),
    }
}

/// Rewrites the definite map head just before `start` for a smaller entry count.
fn shrink_head(out: &mut impl OutBuf, start: usize, old: u64, new: u64) -> Result<()> {
    let mut b = [0u8; 9];
    let old_len = crate::encode::head_bytes(MAJOR_MAP, old, &mut b);
    let new_len = crate::encode::head_bytes(MAJOR_MAP, new, &mut b);
    let head_pos = start.checked_sub(old_len).ok_or(Error::new(ErrorKind::CorruptDatum, start))?;
    let len = out.len();
    let s = out.as_mut_slice();
    s.copy_within(start..len, head_pos + new_len);
    s[head_pos..head_pos + new_len].copy_from_slice(&b[..new_len]);
    out.truncate(len - (old_len - new_len));
    Ok(())
}

/// Sorts a map's contiguous entries (the last bytes written) by encoded key and handles duplicates.
/// Returns how many entries were dropped as duplicates.
fn sort_entries(out: &mut impl OutBuf, es: &[Entry], dups: Duplicates) -> Result<u64> {
    let (Some(first), Some(last)) = (es.first(), es.last()) else {
        return Ok(0);
    };
    let body = out.as_slice();
    let key = |e: &Entry| slice(body, e.key_start, e.key_end);
    let mut sorted = true;
    for w in es.windows(2) {
        match key(&w[0])?.cmp(key(&w[1])?) {
            core::cmp::Ordering::Less => {}
            core::cmp::Ordering::Equal if dups == Duplicates::Reject => {
                return Err(Error::new(ErrorKind::DuplicateKey, w[1].key_start));
            }
            _ => sorted = false,
        }
    }
    if sorted {
        return Ok(0);
    }
    let mut order: Vec<&Entry> = Vec::new();
    order.try_reserve(es.len()).map_err(|_| Error::new(ErrorKind::OutOfMemory, 0))?;
    order.extend(es.iter());
    // Stable: among equal keys the input order is kept, so the last one is the latest.
    // Keys were validated above, so slicing cannot fail here; fall back to equal on the impossible error.
    order.sort_by(|a, b| match (key(a), key(b)) {
        (Ok(x), Ok(y)) => x.cmp(y),
        _ => core::cmp::Ordering::Equal,
    });
    let mut scratch: Vec<u8> = Vec::new();
    scratch
        .try_reserve(last.end - first.key_start)
        .map_err(|_| Error::new(ErrorKind::OutOfMemory, 0))?;
    let mut removed = 0u64;
    for (i, e) in order.iter().enumerate() {
        if let Some(next) = order.get(i + 1)
            && key(e)? == key(next)? {
                if dups == Duplicates::Reject {
                    return Err(Error::new(ErrorKind::DuplicateKey, next.key_start));
                }
                removed += 1;
                continue;
            }
        scratch.extend_from_slice(slice(body, e.key_start, e.end)?);
    }
    let region = out
        .as_mut_slice()
        .get_mut(first.key_start..first.key_start + scratch.len())
        .ok_or(Error::new(ErrorKind::CorruptDatum, first.key_start))?;
    region.copy_from_slice(&scratch);
    out.truncate(first.key_start + scratch.len());
    Ok(removed)
}

/// Bounds-checked sub-slice of a buffer we wrote ourselves.
pub(crate) fn slice(buf: &[u8], start: usize, end: usize) -> Result<&[u8]> {
    buf.get(start..end).ok_or(Error::new(ErrorKind::CorruptDatum, start))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::{Limits, PlainCtx};

    use crate::test_util::hex;

    fn norm(input: &str) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        normalize(&hex(input), &mut out, &mut PlainCtx::default())?;
        Ok(out)
    }

    fn kind(input: &str) -> ErrorKind {
        norm(input).unwrap_err().kind
    }

    #[test]
    fn non_shortest_heads_are_shortened() {
        assert_eq!(norm("1b0000000000000001").unwrap(), hex("01"));
        assert_eq!(norm("3818").unwrap(), hex("3818"));
        assert_eq!(norm("5a00000002abcd").unwrap(), hex("42abcd"));
        assert_eq!(norm("d90001 f6").unwrap(), hex("c1f6"));
        assert_eq!(norm("f818").unwrap_err().kind, ErrorKind::InvalidSimple);
        assert_eq!(norm("f820").unwrap(), hex("f820"));
    }

    #[test]
    fn indefinite_items_become_definite() {
        assert_eq!(norm("9f 01 9f 02 ff ff").unwrap(), hex("82 01 81 02"));
        assert_eq!(norm("5f 42 0102 41 03 ff").unwrap(), hex("43 010203"));
        assert_eq!(norm("7f 61 61 60 ff").unwrap(), hex("61 61"));
        assert_eq!(norm("bf 61 62 01 61 61 02 ff").unwrap(), hex("a2 6161 02 6162 01"));
        assert_eq!(kind("5f 61 61 ff"), ErrorKind::InvalidChunk);
        assert_eq!(kind("5f 5f ff ff"), ErrorKind::InvalidChunk);
        assert_eq!(kind("bf 01 ff"), ErrorKind::MissingValue);
    }

    #[test]
    fn indefinite_container_reaching_threshold_needs_index() {
        let mut input = vec![0x9f];
        input.extend(std::iter::repeat_n(0x01, INDEX_THRESHOLD as usize));
        input.push(0xff);
        let mut out = Vec::new();
        assert!(normalize(&input, &mut out, &mut PlainCtx::default()).unwrap());
        assert_eq!(out[0], 0x90);
    }

    #[test]
    fn map_keys_are_sorted_bytewise_and_duplicates_rejected() {
        // {"aa": 1, "b": 2, 10: 3} → 10 (0x0a) < "b" (0x6162) < "aa" (0x626161)
        assert_eq!(
            norm("a3 626161 01 6162 02 0a 03").unwrap(),
            hex("a3 0a 03 6162 02 626161 01")
        );
        assert_eq!(kind("a2 01 00 01 00"), ErrorKind::DuplicateKey);
        // Duplicates that only appear after key normalization.
        assert_eq!(kind("a2 01 00 c2 41 01 00"), ErrorKind::DuplicateKey);
        assert_eq!(kind("a2 1801 00 01 00"), ErrorKind::DuplicateKey);
    }

    #[test]
    fn builders_keep_the_last_duplicate() {
        let mut out = Vec::new();
        // {"a": 1, "b": 2, "a": 3} definite and indefinite.
        for input in ["a3 6161 01 6162 02 6161 03", "bf 6161 01 6162 02 6161 03 ff"] {
            out.clear();
            normalize_with(&hex(input), &mut out, &mut PlainCtx::default(), Duplicates::KeepLast).unwrap();
            assert_eq!(out, hex("a2 6161 03 6162 02"), "{input}");
        }
    }

    #[test]
    fn bignums_fitting_64_bits_become_integers() {
        assert_eq!(norm("c2 41 01").unwrap(), hex("01"));
        assert_eq!(norm("c3 42 0000").unwrap(), hex("20"));
        assert_eq!(norm("c2 48 ffffffffffffffff").unwrap(), hex("1bffffffffffffffff"));
        assert_eq!(norm("c2 4b 0000 010000000000000000").unwrap(), hex("c2 49 010000000000000000"));
        assert_eq!(norm("c2 5f 41 01 ff").unwrap(), hex("01"));
        assert_eq!(kind("c2 01"), ErrorKind::InvalidBignum);
    }

    #[test]
    fn malformed_input_is_rejected() {
        assert_eq!(kind("01 02"), ErrorKind::TrailingBytes);
        assert_eq!(kind("ff"), ErrorKind::UnexpectedBreak);
        assert_eq!(kind("82 01"), ErrorKind::LengthTooLarge);
        assert_eq!(kind("62 61"), ErrorKind::Truncated);
        assert_eq!(kind("62 c328"), ErrorKind::InvalidUtf8);
        assert_eq!(kind("9b ffffffffffffffff 00"), ErrorKind::LengthTooLarge);
        assert_eq!(kind("bb 0000000000000001 00"), ErrorKind::LengthTooLarge);
        assert_eq!(kind("5b ffffffffffffffff"), ErrorKind::Truncated);
    }

    #[test]
    #[cfg_attr(miri, ignore = "too slow under Miri")]
    fn depth_limit_applies_to_containers_and_tags() {
        let mut ctx = PlainCtx(Limits { max_depth: 3, ..Limits::default() });
        let mut out = Vec::new();
        assert!(normalize(&hex("81 81 c1 00"), &mut out, &mut ctx).is_ok());
        out.clear();
        let e = normalize(&hex("81 81 81 c1 00"), &mut out, &mut ctx).unwrap_err();
        assert_eq!(e.kind, ErrorKind::DepthLimit);
        // Deep nesting fails on the limit, not on the stack.
        let deep: Vec<u8> = std::iter::repeat_n(0x81, 1_000_000).chain([0x00]).collect();
        let e = normalize(&deep, &mut Vec::new(), &mut PlainCtx::default()).unwrap_err();
        assert_eq!(e.kind, ErrorKind::DepthLimit);
    }

    #[test]
    fn rfc8949_appendix_a_vectors_normalize_to_themselves_or_shortest_form() {
        // (input, deterministic encoding)
        let cases = [
            ("00", "00"), ("17", "17"), ("1818", "1818"), ("1903e8", "1903e8"),
            ("1bffffffffffffffff", "1bffffffffffffffff"),
            ("c249010000000000000000", "c249010000000000000000"),
            ("3bffffffffffffffff", "3bffffffffffffffff"),
            ("c349010000000000000000", "c349010000000000000000"),
            ("f90000", "f90000"), ("f98000", "f98000"), ("fb3ff199999999999a", "fb3ff199999999999a"),
            ("f97bff", "f97bff"), ("fa7f7fffff", "fa7f7fffff"), ("fb7e37e43c8800759c", "fb7e37e43c8800759c"),
            ("fa47c35000", "fa47c35000"), ("fb3ff0000000000000", "f93c00"), ("fbfff0000000000000", "f9fc00"),
            ("fa7fc00000", "f97e00"), ("fb7ff8000000000000", "f97e00"),
            ("f4", "f4"), ("f7", "f7"), ("f0", "f0"), ("f8ff", "f8ff"),
            ("c074323031332d30332d32315432303a30343a30305a", "c074323031332d30332d32315432303a30343a30305a"),
            ("d74401020304", "d74401020304"), ("40", "40"), ("60", "60"),
            ("62225c", "62225c"), ("63e6b0b4", "63e6b0b4"), ("64f0908591", "64f0908591"),
            ("8301820203820405", "8301820203820405"),
            ("a201020304", "a201020304"),
            ("a26161016162820203", "a26161016162820203"),
            ("5f42010243030405ff", "450102030405"),
            ("7f657374726561646d696e67ff", "6973747265616d696e67"),
            ("9f018202039f0405ffff", "8301820203820405"),
            ("bf61610161629f0203ffff", "a26161016162820203"),
            ("826161bf61626163ff", "826161a161626163"),
        ];
        for (input, want) in cases {
            assert_eq!(norm(input).unwrap(), hex(want), "{input}");
            assert_eq!(norm(want).unwrap(), hex(want), "idempotence {want}");
        }
    }
}
