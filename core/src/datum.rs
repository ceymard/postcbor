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
        finish_index(out, base, ctx.limits().max_depth)?;
    }
    Ok(())
}

/// Appends a datum for a body that is already deterministic (built by our own writer).
pub fn from_body(body: &[u8], out: &mut impl OutBuf, max_depth: usize) -> Result<()> {
    let base = out.len();
    out.write_byte(VERSION)?;
    out.write_bytes(body)?;
    finish_index(out, base, max_depth)
}

/// Builds and appends the index section for the body written after the header at `base`. The same walk enforces
/// `max_depth`: edits can add levels (`||` wraps scalars in an array), and a stored value deeper than input
/// accepts could not be dumped and restored.
pub fn finish_index(out: &mut impl OutBuf, base: usize, max_depth: usize) -> Result<()> {
    let body_start = base + 1;
    let body_len = out.len() - body_start;
    let built = {
        let body = out.as_slice().get(body_start..).ok_or(Error::new(ErrorKind::CorruptDatum, base))?;
        index::build(body, max_depth)?
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
    // Tables of the run are contiguous: copy them and shift positions, keeping each width flag.
    let t0 = index::table_pos(ix.dir_entry(a)?.1);
    let t1 = if b < ix.dir_len() { index::table_pos(ix.dir_entry(b)?.1) } else { ix.tables_len() };
    out.write_bytes(&((b - a) as u32).to_le_bytes())?;
    for i in a..b {
        let (off, t) = ix.dir_entry(i)?;
        out.write_bytes(&((off - start) as u32).to_le_bytes())?;
        out.write_bytes(&index::shift_table_word(t, t0)?.to_le_bytes())?;
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
        // Samples are entries 0, 8, …; offsets fit 2 bytes.
        assert!(t.narrow);
        assert_eq!(ix.sample(t, 1).unwrap(), 1 + 2 * 8);
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
        for k in 0..n / 8 {
            assert_eq!(ix.sample(t, k).unwrap(), 1 + 3 * 8 * k);
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
    fn index_build_enforces_depth_on_containers_and_tags() {
        let body = [0x81, 0xc1, 0x81, 0x00];
        let mut out = Vec::new();
        assert!(from_body(&body, &mut out, 3).is_ok());
        assert_eq!(from_body(&body, &mut Vec::new(), 2).unwrap_err().kind, ErrorKind::DepthLimit);
        assert!(from_body(&[0x83, 0x81, 0x00, 0x81, 0x00, 0x81, 0x00], &mut Vec::new(), 2).is_ok());
    }

    #[test]
    fn large_containers_use_four_byte_offsets() {
        // 20 elements of 5000 bytes: sample offsets pass 65535.
        let mut input = vec![0x94];
        for _ in 0..20 {
            input.extend([0x59, 0x13, 0x88]);
            input.extend(std::iter::repeat_n(0u8, 5000));
        }
        let d = datum(&input);
        let p = Datum::parse(&d).unwrap();
        let t = p.index.unwrap().table_for(0).unwrap().unwrap();
        assert!(!t.narrow);
        let v = crate::value::Value::root(p);
        for i in 0..20 {
            assert_eq!(v.array_get(i).unwrap().unwrap().pos, 1 + i as usize * 5003);
        }
    }

    #[test]
    fn unknown_versions_and_reserved_bits_are_refused() {
        assert_eq!(Datum::parse(&[1, 0]).unwrap_err().kind, ErrorKind::UnsupportedVersion);
        assert_eq!(Datum::parse(&[0x08, 0]).unwrap_err().kind, ErrorKind::CorruptDatum);
        assert_eq!(Datum::parse(&[HAS_INDEX, 0, 1]).unwrap_err().kind, ErrorKind::CorruptDatum);
    }
}
