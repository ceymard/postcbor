//! Datum layout (design §4): `header body [index body_len]`.

use crate::ctx::Ctx;
use crate::cursor::Cursor;
use crate::error::{Error, ErrorKind, Result};
use crate::index::{self, Index};
use crate::normalize::{Duplicates, normalize_unpacking};
use crate::out::OutBuf;

pub const VERSION: u8 = 0;
const VERSION_MASK: u8 = 0b11;
pub const HAS_INDEX: u8 = 0b100;
const RESERVED_MASK: u8 = !0b111;

/// A parsed stored datum: the body and its optional index section.
#[derive(Clone, Copy, Debug)]
pub struct Datum<'a> {
    pub body: &'a [u8],
    pub index: Option<Index<'a>>,
}

impl<'a> Datum<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let mut c = Cursor::new(bytes);
        let header = c.byte().map_err(|_| Error::new(ErrorKind::CorruptDatum, 0))?;
        if header & VERSION_MASK != VERSION {
            return Err(Error::new(ErrorKind::UnsupportedVersion, 0));
        }
        if header & RESERVED_MASK != 0 {
            return Err(Error::new(ErrorKind::CorruptDatum, 0));
        }
        if header & HAS_INDEX == 0 {
            let body = c.take(c.remaining() as u64)?;
            return Ok(Datum { body, index: None });
        }
        let len = bytes.len();
        let body_len = Cursor::u32_at(bytes, len.checked_sub(4).ok_or(Error::new(ErrorKind::CorruptDatum, 0))?)?;
        let body = c.take(u64::from(body_len)).map_err(|e| Error::new(ErrorKind::CorruptDatum, e.offset))?;
        let section = c.take((c.remaining() - 4) as u64).map_err(|e| Error::new(ErrorKind::CorruptDatum, e.offset))?;
        Ok(Datum { body, index: Some(Index::parse(section)?) })
    }
}

/// Normalizes untrusted CBOR and appends a complete datum to `out`.
pub fn from_cbor(input: &[u8], out: &mut impl OutBuf, ctx: &mut impl Ctx) -> Result<()> {
    from_cbor_with(input, out, ctx, Duplicates::Reject)
}

/// [`from_cbor`] with a duplicate-key policy.
pub fn from_cbor_with(input: &[u8], out: &mut impl OutBuf, ctx: &mut impl Ctx, dups: Duplicates) -> Result<()> {
    let base = out.len();
    out.write_byte(VERSION)?;
    if normalize_unpacking(input, out, ctx, dups)? {
        finish_index(out, base)?;
    }
    Ok(())
}

/// Appends a datum for a body that is already deterministic (built by our own writer).
pub fn from_body(body: &[u8], out: &mut impl OutBuf) -> Result<()> {
    let base = out.len();
    out.write_byte(VERSION)?;
    out.write_bytes(body)?;
    finish_index(out, base)
}

/// Builds and appends the index section for the body written after the header at `base`.
pub fn finish_index(out: &mut impl OutBuf, base: usize) -> Result<()> {
    let body_start = base + 1;
    let body_len = out.len() - body_start;
    let built = {
        let body = out.as_slice().get(body_start..).ok_or(Error::new(ErrorKind::CorruptDatum, base))?;
        index::build(body)?
    };
    if built.dir.is_empty() {
        return Ok(());
    }
    index::write(&built, out)?;
    let body_len = u32::try_from(body_len).map_err(|_| Error::new(ErrorKind::ExpansionLimit, body_len))?;
    out.write_bytes(&body_len.to_le_bytes())?;
    out.as_mut_slice()[base] |= HAS_INDEX;
    Ok(())
}

/// Fails with `DepthLimit` when the stored value nests deeper than `max_depth` (arrays, maps and tags).
/// Edits can add levels (`||` wraps scalars in an array); their results must stay readable by input.
pub fn check_depth(d: &Datum, max_depth: usize) -> Result<()> {
    let mut c = Cursor::new(d.body);
    // Items left at each open level.
    let mut stack: Vec<u64> = Vec::new();
    let mut pending: u64 = 1;
    while pending > 0 || !stack.is_empty() {
        if pending == 0 {
            pending = stack.pop().unwrap_or(0);
            continue;
        }
        pending -= 1;
        let at = c.pos();
        let h = c.head().map_err(|e| Error::new(ErrorKind::CorruptDatum, e.offset))?;
        let children = match h.major {
            crate::cursor::MAJOR_BYTES | crate::cursor::MAJOR_TEXT => {
                c.take(h.arg).map_err(|e| Error::new(ErrorKind::CorruptDatum, e.offset))?;
                0
            }
            crate::cursor::MAJOR_ARRAY => h.arg,
            crate::cursor::MAJOR_MAP => h.arg.checked_mul(2).ok_or(Error::new(ErrorKind::CorruptDatum, at))?,
            crate::cursor::MAJOR_TAG => 1,
            _ => continue,
        };
        if h.major >= crate::cursor::MAJOR_ARRAY {
            if stack.len() + 1 > max_depth {
                return Err(Error::new(ErrorKind::DepthLimit, at));
            }
            crate::out::try_push(&mut stack, pending)?;
            pending = children;
        }
    }
    Ok(())
}

/// Appends a datum holding the sub-value at body range `[start, end)` of `d`, reusing its entry tables.
pub fn write_subvalue(d: &Datum, start: usize, end: usize, out: &mut impl OutBuf) -> Result<()> {
    let body = d.body.get(start..end).ok_or(Error::new(ErrorKind::CorruptDatum, start))?;
    let base = out.len();
    out.write_byte(VERSION)?;
    out.write_bytes(body)?;
    let Some(ix) = d.index else { return Ok(()) };
    let a = ix.lower_bound(start)?;
    let b = ix.lower_bound(end)?;
    if a == b {
        return Ok(());
    }
    let t0 = ix.dir_entry(a)?.1;
    let t1 = if b < ix.dir_len() { ix.dir_entry(b)?.1 } else { ix.tables_len() };
    out.write_bytes(&((b - a) as u32).to_le_bytes())?;
    for i in a..b {
        let (off, t) = ix.dir_entry(i)?;
        out.write_bytes(&((off - start) as u32).to_le_bytes())?;
        out.write_bytes(&((t - t0) as u32).to_le_bytes())?;
    }
    out.write_bytes(ix.tables_bytes(t0, t1)?)?;
    out.write_bytes(&(body.len() as u32).to_le_bytes())?;
    out.as_mut_slice()[base] |= HAS_INDEX;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::PlainCtx;
    use crate::index::INDEX_THRESHOLD;

    fn datum(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        from_cbor(input, &mut out, &mut PlainCtx::default()).unwrap();
        out
    }

    /// [[24, 25, …] × outer], every inner array with `n` (≥ threshold, < 24) elements, so each inner array is indexed.
    fn nested(outer: usize, n: usize) -> Vec<u8> {
        let mut v = vec![0x80 | outer as u8];
        for _ in 0..outer {
            v.push(0x80 | n as u8);
            for i in 0..n {
                v.extend([0x18, i as u8 + 24]);
            }
        }
        v
    }

    #[test]
    fn small_values_have_no_index_section() {
        let d = datum(&[0x83, 1, 2, 3]);
        assert_eq!(d, [VERSION, 0x83, 1, 2, 3]);
        assert!(Datum::parse(&d).unwrap().index.is_none());
    }

    #[test]
    fn index_tables_point_at_every_entry() {
        let n = INDEX_THRESHOLD as usize;
        let d = datum(&nested(2, n));
        let p = Datum::parse(&d).unwrap();
        let ix = p.index.unwrap();
        // The 2-element outer array is below the threshold; both inner arrays are indexed.
        assert_eq!(ix.dir_len(), 2);
        assert!(ix.table_for(0).unwrap().is_none());
        // Second inner array starts after the outer head and the first inner array (1 + 2n bytes each).
        let inner = 1 + 1 + 2 * n;
        let t = ix.table_for(inner).unwrap().unwrap();
        assert_eq!(ix.entry(t, 5).unwrap(), 1 + 2 * 5);
    }

    #[test]
    fn map_tables_point_at_keys() {
        let n = INDEX_THRESHOLD as usize;
        let mut input = vec![0xa0 | n as u8];
        for i in 0..n {
            input.extend([0x18, 0x20 + i as u8, 0xf6]);
        }
        let d = datum(&input);
        let ix = Datum::parse(&d).unwrap().index.unwrap();
        let t = ix.table_for(0).unwrap().unwrap();
        for i in 0..n {
            assert_eq!(ix.entry(t, i).unwrap(), 1 + 3 * i);
        }
    }

    #[test]
    fn extracting_a_subvalue_equals_normalizing_it_alone() {
        let n = INDEX_THRESHOLD as usize;
        let d = datum(&nested(3, n));
        let p = Datum::parse(&d).unwrap();
        let inner_len = 1 + 2 * n;
        for k in 0..3 {
            let start = 1 + k * inner_len;
            let mut sub = Vec::new();
            write_subvalue(&p, start, start + inner_len, &mut sub).unwrap();
            let alone = datum(&p.body[start..start + inner_len]);
            assert_eq!(sub, alone, "inner {k}");
        }
        // The whole body extracts to the same datum.
        let mut whole = Vec::new();
        write_subvalue(&p, 0, p.body.len(), &mut whole).unwrap();
        assert_eq!(whole, d);
    }

    #[test]
    fn depth_counts_containers_and_tags() {
        let d = datum(&[0x81, 0xc1, 0x81, 0x00]);
        let p = Datum::parse(&d).unwrap();
        assert!(check_depth(&p, 3).is_ok());
        assert_eq!(check_depth(&p, 2).unwrap_err().kind, ErrorKind::DepthLimit);
        let flat = datum(&[0x83, 0x81, 0x00, 0x81, 0x00, 0x81, 0x00]);
        assert!(check_depth(&Datum::parse(&flat).unwrap(), 2).is_ok());
    }

    #[test]
    fn unknown_versions_and_reserved_bits_are_refused() {
        assert_eq!(Datum::parse(&[1, 0]).unwrap_err().kind, ErrorKind::UnsupportedVersion);
        assert_eq!(Datum::parse(&[0x08, 0]).unwrap_err().kind, ErrorKind::CorruptDatum);
        assert_eq!(Datum::parse(&[HAS_INDEX, 0, 1]).unwrap_err().kind, ErrorKind::CorruptDatum);
    }
}
