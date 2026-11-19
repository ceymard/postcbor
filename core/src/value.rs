//! Borrowed views over a stored body: decoding, navigation and iteration.

use crate::cursor::*;
use crate::datum::Datum;
use crate::error::{Error, ErrorKind, Result};
use crate::float;
use crate::index::{INDEX_THRESHOLD, STRIDE, Table};

/// The decoded content of one item. Container variants carry their entry count.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind<'a> {
    Unsigned(u64),
    /// Value is −1 − n.
    Negative(u64),
    Bytes(&'a [u8]),
    Text(&'a str),
    Array(u64),
    Map(u64),
    Tag(u64),
    Float(f64),
    False,
    True,
    Null,
    Undefined,
    /// Unassigned simple value.
    Simple(u8),
}

fn corrupt(at: usize) -> Error {
    Error::new(ErrorKind::CorruptDatum, at)
}

/// One item inside a stored datum.
#[derive(Clone, Copy, Debug)]
pub struct Value<'a> {
    pub d: &'a Datum<'a>,
    pub pos: usize,
}

impl<'a> Value<'a> {
    pub fn root(d: &'a Datum<'a>) -> Self {
        Value { d, pos: 0 }
    }

    fn at(&self, pos: usize) -> Value<'a> {
        Value { d: self.d, pos }
    }

    /// Head of this item and the position right after it.
    #[inline]
    pub fn head(&self) -> Result<(Head, usize)> {
        let mut c = Cursor::at(self.d.body, self.pos);
        let h = c.head().map_err(|e| corrupt(e.offset))?;
        Ok((h, c.pos()))
    }

    pub fn kind(&self) -> Result<Kind<'a>> {
        let mut c = Cursor::at(self.d.body, self.pos);
        let h = c.head().map_err(|e| corrupt(e.offset))?;
        Ok(match h.major {
            MAJOR_UNSIGNED => Kind::Unsigned(h.arg),
            MAJOR_NEGATIVE => Kind::Negative(h.arg),
            MAJOR_BYTES => Kind::Bytes(c.take(h.arg).map_err(|e| corrupt(e.offset))?),
            MAJOR_TEXT => {
                let s = c.take(h.arg).map_err(|e| corrupt(e.offset))?;
                Kind::Text(core::str::from_utf8(s).map_err(|_| corrupt(self.pos))?)
            }
            MAJOR_ARRAY => Kind::Array(h.arg),
            MAJOR_MAP => Kind::Map(h.arg),
            MAJOR_TAG => Kind::Tag(h.arg),
            _ => match h.info {
                20 => Kind::False,
                21 => Kind::True,
                22 => Kind::Null,
                23 => Kind::Undefined,
                25..=27 => Kind::Float(float::decode(h.info, h.arg)),
                _ => Kind::Simple(h.arg as u8),
            },
        })
    }

    /// Content bytes of a text string, without the UTF-8 check `kind` makes; None for other items.
    pub fn text_bytes(&self) -> Result<Option<&'a [u8]>> {
        let mut c = Cursor::at(self.d.body, self.pos);
        let h = c.head().map_err(|e| corrupt(e.offset))?;
        if h.major != MAJOR_TEXT {
            return Ok(None);
        }
        Ok(Some(c.take(h.arg).map_err(|e| corrupt(e.offset))?))
    }

    /// Body offset just past this item. Jumps over indexed containers through their last entry.
    #[inline]
    pub fn end(&self) -> Result<usize> {
        end_of(self.d, self.pos)
    }

    /// Body bytes of this item.
    #[inline]
    pub fn bytes(&self) -> Result<&'a [u8]> {
        let end = self.end()?;
        self.d.body.get(self.pos..end).ok_or(corrupt(self.pos))
    }

    /// First item inside this container or tag.
    fn first_child(&self) -> Result<usize> {
        Ok(self.head()?.1)
    }

    /// Content of a tag.
    pub fn tag_content(&self) -> Result<Value<'a>> {
        Ok(self.at(self.first_child()?))
    }

    /// Array element `i`; negative counts from the end. None when out of range or not an array.
    pub fn array_get(&self, i: i64) -> Result<Option<Value<'a>>> {
        let (h, first) = self.head()?;
        if h.major != MAJOR_ARRAY {
            return Ok(None);
        }
        let n = h.arg;
        let idx = if i < 0 { n.checked_sub(i.unsigned_abs()) } else { Some(i as u64) };
        let Some(idx) = idx.filter(|&x| x < n) else { return Ok(None) };
        // Jump to the sample at or before the element, then step over the rest.
        let (mut p, skip) = match self.table()? {
            Some(t) => {
                let off = self.d.index.ok_or(corrupt(self.pos))?.sample(t, (idx / STRIDE) as usize)?;
                (self.pos.checked_add(off).ok_or(corrupt(self.pos))?, idx % STRIDE)
            }
            None => (first, idx),
        };
        for _ in 0..skip {
            p = end_of(self.d, p)?;
        }
        Ok(Some(self.at(p)))
    }

    /// Value stored under the key whose deterministic encoding is `key`. None when absent or not a map.
    pub fn map_get(&self, key: &(impl EncodedKey + ?Sized)) -> Result<Option<Value<'a>>> {
        let (h, first) = self.head()?;
        if h.major != MAJOR_MAP {
            return Ok(None);
        }
        // With an index: binary search the sampled keys for the last one ≤ `key`, then scan from there.
        let (mut p, mut left) = (first, h.arg);
        if let Some(t) = self.table()? {
            let ix = self.d.index.ok_or(corrupt(self.pos))?;
            let samples = h.arg.div_ceil(STRIDE) as usize;
            let (mut lo, mut hi) = (0usize, samples);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let k = self.at(self.pos + ix.sample(t, mid)?);
                let kend = k.end()?;
                match key.cmp_stored(self.d.body.get(k.pos..kend).ok_or(corrupt(k.pos))?) {
                    core::cmp::Ordering::Less => lo = mid + 1,
                    core::cmp::Ordering::Greater => hi = mid,
                    core::cmp::Ordering::Equal => return Ok(Some(self.at(kend))),
                }
            }
            // Every sampled key before `lo` is smaller: the key, if present, follows sample `lo - 1`.
            if lo == 0 {
                return Ok(None);
            }
            p = self.pos + ix.sample(t, lo - 1)?;
            left = (h.arg - (lo as u64 - 1) * STRIDE).min(STRIDE);
        }
        for _ in 0..left {
            let kend = end_of(self.d, p)?;
            match key.cmp_stored(self.d.body.get(p..kend).ok_or(corrupt(p))?) {
                core::cmp::Ordering::Less => p = end_of(self.d, kend)?,
                core::cmp::Ordering::Equal => return Ok(Some(self.at(kend))),
                // Keys are sorted: once past, the key is absent.
                core::cmp::Ordering::Greater => return Ok(None),
            }
        }
        Ok(None)
    }

    /// Entry table of this container, when it is indexed.
    fn table(&self) -> Result<Option<Table>> {
        match self.d.index {
            Some(ix) => ix.table_for(self.pos),
            None => Ok(None),
        }
    }

    /// Iterates array elements; empty for non-arrays.
    pub fn array_iter(&self) -> Result<ArrayIter<'a>> {
        let (h, first) = self.head()?;
        let n = if h.major == MAJOR_ARRAY { h.arg } else { 0 };
        Ok(ArrayIter { d: self.d, pos: first, remaining: n, last: None })
    }

    /// Iterates map entries; empty for non-maps.
    pub fn map_iter(&self) -> Result<MapIter<'a>> {
        let (h, first) = self.head()?;
        let n = if h.major == MAJOR_MAP { h.arg } else { 0 };
        Ok(MapIter { d: self.d, pos: first, remaining: n, last_value: None })
    }
}

/// A deterministic key encoding that lookups compare against stored keys.
pub trait EncodedKey {
    /// Bytewise order of `stored` relative to this key.
    fn cmp_stored(&self, stored: &[u8]) -> core::cmp::Ordering;
}

impl EncodedKey for [u8] {
    /// Decides on the first byte when it differs: for strings shorter than 24 bytes that byte holds the length, so
    /// most comparisons end there without a call to `memcmp`.
    #[inline(always)]
    fn cmp_stored(&self, stored: &[u8]) -> core::cmp::Ordering {
        match (stored.split_first(), self.split_first()) {
            (Some((a, ra)), Some((b, rb))) if a == b => ra.cmp(rb),
            (Some((a, _)), Some((b, _))) => a.cmp(b),
            _ => stored.cmp(self),
        }
    }
}

/// Body offset just past the item at `pos`.
#[inline(always)]
pub fn end_of(d: &Datum, pos: usize) -> Result<usize> {
    // Fast path: scalars and strings end where their head says; containers and tags need the walk.
    let mut c = Cursor::at(d.body, pos);
    let h = c.head().map_err(|e| corrupt(e.offset))?;
    match h.major {
        MAJOR_BYTES | MAJOR_TEXT => {
            c.take(h.arg).map_err(|e| corrupt(e.offset))?;
            Ok(c.pos())
        }
        MAJOR_ARRAY | MAJOR_MAP | MAJOR_TAG => end_of_nested(d, pos),
        _ => Ok(c.pos()),
    }
}

/// [`end_of`] for containers and tags.
fn end_of_nested(d: &Datum, pos: usize) -> Result<usize> {
    let mut c = Cursor::at(d.body, pos);
    let mut pending: u64 = 1;
    while pending > 0 {
        let item = c.pos();
        let h = c.head().map_err(|e| corrupt(e.offset))?;
        pending -= 1;
        match h.major {
            MAJOR_BYTES | MAJOR_TEXT => {
                c.take(h.arg).map_err(|e| corrupt(e.offset))?;
            }
            MAJOR_ARRAY | MAJOR_MAP => {
                let per: u64 = if h.major == MAJOR_MAP { 2 } else { 1 };
                if h.arg >= INDEX_THRESHOLD
                    && let Some(ix) = d.index
                        && let Some(t) = ix.table_for(item)? {
                            // Jump to the last sample; only the entries from it to the end remain to skip.
                            let k = (h.arg - 1) / STRIDE;
                            let last = ix.sample(t, k as usize)?;
                            c = Cursor::at(d.body, item.checked_add(last).ok_or(corrupt(item))?);
                            pending += (h.arg - k * STRIDE) * per;
                            continue;
                        }
                pending = h.arg.checked_mul(per).and_then(|n| n.checked_add(pending)).ok_or(corrupt(item))?;
            }
            MAJOR_TAG => pending += 1,
            _ => {}
        }
    }
    Ok(c.pos())
}

/// Array elements. Lazy: an element's end is only computed when moving past it.
pub struct ArrayIter<'a> {
    d: &'a Datum<'a>,
    pos: usize,
    remaining: u64,
    last: Option<usize>,
}

impl<'a> ArrayIter<'a> {
    /// Moves past the previously returned item.
    fn advance(&mut self) -> Result<()> {
        if let Some(p) = self.last.take() {
            self.pos = end_of(self.d, p)?;
        }
        Ok(())
    }
}

impl<'a> Iterator for ArrayIter<'a> {
    type Item = Result<Value<'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        if let Err(e) = self.advance() {
            self.remaining = 0;
            return Some(Err(e));
        }
        self.remaining -= 1;
        self.last = Some(self.pos);
        Some(Ok(Value { d: self.d, pos: self.pos }))
    }
}

/// Map entries. Lazy on values: a value's end is only computed when moving past it.
pub struct MapIter<'a> {
    d: &'a Datum<'a>,
    pos: usize,
    remaining: u64,
    last_value: Option<usize>,
}

impl<'a> Iterator for MapIter<'a> {
    type Item = Result<(Value<'a>, Value<'a>)>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let step = |it: &mut Self| -> Result<(Value<'a>, Value<'a>)> {
            if let Some(p) = it.last_value.take() {
                it.pos = end_of(it.d, p)?;
            }
            let k = Value { d: it.d, pos: it.pos };
            let v = Value { d: it.d, pos: k.end()? };
            it.last_value = Some(v.pos);
            Ok((k, v))
        };
        self.remaining -= 1;
        match step(self) {
            Ok(kv) => Some(Ok(kv)),
            Err(e) => {
                self.remaining = 0;
                Some(Err(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::PlainCtx;
    use crate::datum::from_cbor;

    fn stored(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        from_cbor(input, &mut out, &mut PlainCtx::default()).unwrap();
        out
    }

    fn int_array(n: u8) -> Vec<u8> {
        let mut v = vec![0x98, n];
        for i in 0..n {
            v.extend([0x18, i]);
        }
        v
    }

    fn int_map(n: u8) -> Vec<u8> {
        let mut v = vec![0xb8, n];
        for i in 0..n {
            v.extend([0x18, i, 0x18, i.wrapping_add(100)]);
        }
        v
    }

    #[test]
    fn array_access_agrees_with_and_without_index() {
        for n in [3u8, 15, 16, 40] {
            let s = stored(&int_array(n));
            let root_d = Datum::parse(&s).unwrap();
            let root = Value::root(&root_d);
            for i in -(i64::from(n)) - 1..=i64::from(n) {
                let got = root.array_get(i).unwrap().map(|v| v.kind().unwrap());
                let want = if i >= i64::from(n) || i < -(i64::from(n)) {
                    None
                } else {
                    Some(Kind::Unsigned(i.rem_euclid(i64::from(n)) as u64))
                };
                assert_eq!(got, want, "n={n} i={i}");
            }
            assert_eq!(root.end().unwrap(), root.d.body.len());
        }
    }

    #[test]
    fn map_lookup_finds_every_key_and_rejects_absent_ones() {
        for n in [3u8, 16, 50] {
            let s = stored(&int_map(n));
            let root_d = Datum::parse(&s).unwrap();
            let root = Value::root(&root_d);
            for i in 0..n {
                let k = [0x18, i];
                let k: &[u8] = if i < 24 { &k[1..] } else { &k };
                let v = root.map_get(k).unwrap().expect("present");
                assert_eq!(v.kind().unwrap(), Kind::Unsigned(u64::from(i.wrapping_add(100))));
            }
            assert!(root.map_get(&[0x19u8, 0x01, 0x00][..]).unwrap().is_none());
            assert!(root.map_get(&[0x60u8][..]).unwrap().is_none());
            assert_eq!(root.map_iter().unwrap().count(), n as usize);
        }
    }

    #[test]
    fn end_jumps_over_nested_indexed_containers() {
        // [[0..40], {0..40}, 7]
        let mut input = vec![0x83];
        input.extend(int_array(40));
        input.extend(int_map(40));
        input.push(0x07);
        let s = stored(&input);
        let root_d = Datum::parse(&s).unwrap();
        let root = Value::root(&root_d);
        assert_eq!(root.array_get(2).unwrap().unwrap().kind().unwrap(), Kind::Unsigned(7));
        let kinds: Vec<_> = root.array_iter().unwrap().map(|v| v.unwrap().kind().unwrap()).collect();
        assert_eq!(kinds, [Kind::Array(40), Kind::Map(40), Kind::Unsigned(7)]);
    }
}
