//! Index section (design §4): sampled entry offset tables for containers with many entries.
//! Every `STRIDE`-th entry's offset is kept, in 2 bytes when the whole table fits, else 4.

use crate::cursor::*;
use crate::error::{Error, ErrorKind, Result};
use crate::out::{OutBuf, try_push};

/// Containers with at least this many entries get an entry table.
pub const INDEX_THRESHOLD: u64 = 16;
/// One offset is kept every `STRIDE` entries; lookups step over at most `STRIDE - 1` entries after a jump.
pub const STRIDE: u64 = 8;
/// Top bit of a directory entry's table position: the table holds 2-byte offsets.
const NARROW: u32 = 1 << 31;

/// Directory and sampled tables built from a body, before serialization.
#[derive(Default)]
pub struct Built {
    /// (container offset in body, index of its table in `tables`)
    pub dir: Vec<(u32, usize)>,
    pub tables: Vec<Vec<u32>>,
}

struct Frame {
    container: usize,
    remaining: u64,
    item: u64,
    /// (table index, is_map) when this container is indexed.
    table: Option<(usize, bool)>,
}

fn corrupt(at: usize) -> Error {
    Error::new(ErrorKind::CorruptDatum, at)
}

fn to_u32(v: usize, at: usize) -> Result<u32> {
    u32::try_from(v).map_err(|_| Error::new(ErrorKind::TooLarge, at))
}

/// Walks a deterministic body in order, records the sampled entry offsets of every indexed container, and fails
/// with `DepthLimit` when the value nests deeper than `max_depth` (arrays, maps and tags count).
pub fn build(body: &[u8], max_depth: usize) -> Result<Built> {
    let mut b = Built::default();
    let mut c = Cursor::new(body);
    let mut stack: Vec<Frame> = Vec::new();
    loop {
        let pos = c.pos();
        if let Some(top) = stack.last_mut() {
            if let Some((t, is_map)) = top.table
                && (!is_map || top.item % 2 == 0)
            {
                let entry = if is_map { top.item / 2 } else { top.item };
                if entry.is_multiple_of(STRIDE) {
                    let rel = to_u32(pos - top.container, pos)?;
                    try_push(b.tables.get_mut(t).ok_or(corrupt(pos))?, rel)?;
                }
            }
            top.item += 1;
            top.remaining -= 1;
        }
        let h = c.head().map_err(|e| corrupt(e.offset))?;
        match h.major {
            MAJOR_BYTES | MAJOR_TEXT => {
                c.take(h.arg).map_err(|e| corrupt(e.offset))?;
            }
            MAJOR_ARRAY | MAJOR_MAP | MAJOR_TAG => {
                if stack.len() >= max_depth {
                    return Err(Error::new(ErrorKind::DepthLimit, pos));
                }
                let is_map = h.major == MAJOR_MAP;
                let items = match h.major {
                    MAJOR_TAG => 1,
                    MAJOR_MAP => h.arg.checked_mul(2).ok_or(corrupt(pos))?,
                    _ => h.arg,
                };
                if h.major != MAJOR_TAG && items > c.remaining() as u64 {
                    return Err(corrupt(pos));
                }
                let table = if h.major != MAJOR_TAG && h.arg >= INDEX_THRESHOLD {
                    try_push(&mut b.dir, (to_u32(pos, pos)?, b.tables.len()))?;
                    try_push(&mut b.tables, Vec::new())?;
                    Some((b.tables.len() - 1, is_map))
                } else {
                    None
                };
                if items > 0 {
                    try_push(&mut stack, Frame { container: pos, remaining: items, item: 0, table })?;
                }
            }
            _ => {}
        }
        while stack.last().is_some_and(|f| f.remaining == 0) {
            stack.pop();
        }
        if stack.is_empty() {
            return if c.is_empty() { Ok(b) } else { Err(corrupt(c.pos())) };
        }
    }
}

fn is_narrow(table: &[u32]) -> bool {
    table.iter().all(|&v| v <= u32::from(u16::MAX))
}

/// Appends the serialized index section (without the trailing body length).
pub fn write(b: &Built, out: &mut impl OutBuf) -> Result<()> {
    out.write_bytes(&(b.dir.len() as u32).to_le_bytes())?;
    // Tables are laid out in directory order; each entry records its table's byte position and width.
    let mut pos: u32 = 0;
    for &(off, t) in &b.dir {
        let table = b.tables.get(t).ok_or(corrupt(0))?;
        let narrow = is_narrow(table);
        if pos >= NARROW {
            return Err(Error::new(ErrorKind::TooLarge, 0));
        }
        out.write_bytes(&off.to_le_bytes())?;
        out.write_bytes(&(pos | if narrow { NARROW } else { 0 }).to_le_bytes())?;
        let width = if narrow { 2 } else { 4 };
        pos = pos.checked_add(to_u32(table.len() * width, 0)?).ok_or(Error::new(ErrorKind::TooLarge, 0))?;
    }
    for &(_, t) in &b.dir {
        let table = b.tables.get(t).ok_or(corrupt(0))?;
        let narrow = is_narrow(table);
        for &v in table {
            if narrow {
                out.write_bytes(&(v as u16).to_le_bytes())?;
            } else {
                out.write_bytes(&v.to_le_bytes())?;
            }
        }
    }
    Ok(())
}

/// One sampled entry table: its byte position in the tables area and its offset width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Table {
    pub at: usize,
    pub narrow: bool,
}

/// Read-only view of a stored index section.
#[derive(Clone, Copy, Debug)]
pub struct Index<'a> {
    dir: &'a [u8],
    tables: &'a [u8],
}

impl<'a> Index<'a> {
    pub fn parse(section: &'a [u8]) -> Result<Self> {
        let mut c = Cursor::new(section);
        let count = c.read_u32_le().map_err(|e| corrupt(e.offset))? as u64;
        let dir = c.take(count * 8).map_err(|e| corrupt(e.offset))?;
        let tables = c.take(c.remaining() as u64)?;
        Ok(Index { dir, tables })
    }

    pub fn dir_len(&self) -> usize {
        self.dir.len() / 8
    }

    /// (container offset, raw table word: byte position and width flag) of directory entry `i`.
    pub fn dir_entry(&self, i: usize) -> Result<(usize, u32)> {
        let off = Cursor::u32_at(self.dir, i * 8)? as usize;
        let t = Cursor::u32_at(self.dir, i * 8 + 4)?;
        Ok((off, t))
    }

    /// First directory position whose container offset is ≥ `body_off`.
    pub fn lower_bound(&self, body_off: usize) -> Result<usize> {
        let (mut lo, mut hi) = (0, self.dir_len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.dir_entry(mid)?.0 < body_off {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }

    /// Table of the container at `body_off`, if it is indexed.
    pub fn table_for(&self, body_off: usize) -> Result<Option<Table>> {
        let i = self.lower_bound(body_off)?;
        if i < self.dir_len() {
            let (off, t) = self.dir_entry(i)?;
            if off == body_off {
                return Ok(Some(Table { at: table_pos(t), narrow: t & NARROW != 0 }));
            }
        }
        Ok(None)
    }

    /// Offset, relative to its container, of sample `k` (entry `k × STRIDE`).
    pub fn sample(&self, t: Table, k: usize) -> Result<usize> {
        let width = if t.narrow { 2 } else { 4 };
        let at = k.checked_mul(width).and_then(|v| v.checked_add(t.at)).ok_or(corrupt(0))?;
        let mut c = Cursor::at(self.tables, at);
        if t.narrow {
            let s = c.take(2).map_err(|e| corrupt(e.offset))?;
            Ok(usize::from(u16::from_le_bytes([s[0], s[1]])))
        } else {
            Ok(c.read_u32_le().map_err(|e| corrupt(e.offset))? as usize)
        }
    }

    /// Byte length of the tables area.
    pub fn tables_len(&self) -> usize {
        self.tables.len()
    }

    /// Raw bytes of the tables area between two byte positions.
    pub fn tables_bytes(&self, from: usize, to: usize) -> Result<&'a [u8]> {
        self.tables.get(from..to).ok_or(corrupt(from))
    }
}

/// Byte position part of a raw directory table word.
pub fn table_pos(word: u32) -> usize {
    (word & !NARROW) as usize
}

/// A raw directory table word with its position moved down by `by` bytes, keeping the width flag.
pub fn shift_table_word(word: u32, by: usize) -> Result<u32> {
    let pos = table_pos(word).checked_sub(by).ok_or(corrupt(0))?;
    Ok(to_u32(pos, 0)? | (word & NARROW))
}
