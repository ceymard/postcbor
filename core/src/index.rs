//! Index section (design §4): entry offset tables for containers with many entries.

use crate::cursor::*;
use crate::error::{Error, ErrorKind, Result};
use crate::out::{OutBuf, try_push};

/// Containers with at least this many entries get an entry table.
pub const INDEX_THRESHOLD: u64 = 16;

/// Directory and tables built from a body, before serialization.
#[derive(Default)]
pub struct Built {
    /// (container offset in body, table offset in u32 units)
    pub dir: Vec<(u32, u32)>,
    pub tables: Vec<u32>,
}

struct Frame {
    container: usize,
    remaining: u64,
    item: u64,
    /// (table offset, is_map) when this container is indexed.
    table: Option<(usize, bool)>,
}

fn corrupt(at: usize) -> Error {
    Error::new(ErrorKind::CorruptDatum, at)
}

fn to_u32(v: usize, at: usize) -> Result<u32> {
    u32::try_from(v).map_err(|_| Error::new(ErrorKind::ExpansionLimit, at))
}

/// Walks a deterministic body in order and records entry offsets of every indexed container.
pub fn build(body: &[u8]) -> Result<Built> {
    let mut b = Built::default();
    let mut c = Cursor::new(body);
    let mut stack: Vec<Frame> = Vec::new();
    loop {
        let pos = c.pos();
        if let Some(top) = stack.last_mut() {
            if let Some((t, is_map)) = top.table
                && (!is_map || top.item % 2 == 0) {
                    let i = if is_map { top.item / 2 } else { top.item } as usize;
                    let slot = b.tables.get_mut(t + i).ok_or(corrupt(pos))?;
                    *slot = to_u32(pos - top.container, pos)?;
                }
            top.item += 1;
            top.remaining -= 1;
        }
        let h = c.head().map_err(|e| corrupt(e.offset))?;
        match h.major {
            MAJOR_BYTES | MAJOR_TEXT => {
                c.take(h.arg).map_err(|e| corrupt(e.offset))?;
            }
            MAJOR_ARRAY | MAJOR_MAP => {
                let is_map = h.major == MAJOR_MAP;
                let items = if is_map { h.arg.checked_mul(2).ok_or(corrupt(pos))? } else { h.arg };
                if items > c.remaining() as u64 {
                    return Err(corrupt(pos));
                }
                let table = if h.arg >= INDEX_THRESHOLD {
                    let t = b.tables.len();
                    try_push(&mut b.dir, (to_u32(pos, pos)?, to_u32(t, pos)?))?;
                    b.tables
                        .try_reserve(h.arg as usize)
                        .map_err(|_| Error::new(ErrorKind::OutOfMemory, pos))?;
                    b.tables.resize(t + h.arg as usize, 0);
                    Some((t, is_map))
                } else {
                    None
                };
                if items > 0 {
                    try_push(&mut stack, Frame { container: pos, remaining: items, item: 0, table })?;
                }
            }
            MAJOR_TAG => try_push(&mut stack, Frame { container: pos, remaining: 1, item: 0, table: None })?,
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

/// Appends the serialized index section (without the trailing body length).
pub fn write(b: &Built, out: &mut impl OutBuf) -> Result<()> {
    out.write_bytes(&(b.dir.len() as u32).to_le_bytes())?;
    for &(off, t) in &b.dir {
        out.write_bytes(&off.to_le_bytes())?;
        out.write_bytes(&t.to_le_bytes())?;
    }
    for &v in &b.tables {
        out.write_bytes(&v.to_le_bytes())?;
    }
    Ok(())
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
        if tables.len() % 4 != 0 {
            return Err(corrupt(section.len()));
        }
        Ok(Index { dir, tables })
    }

    pub fn dir_len(&self) -> usize {
        self.dir.len() / 8
    }

    /// (container offset, table offset) of directory entry `i`.
    pub fn dir_entry(&self, i: usize) -> Result<(usize, usize)> {
        let off = Cursor::u32_at(self.dir, i * 8)? as usize;
        let t = Cursor::u32_at(self.dir, i * 8 + 4)? as usize;
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

    /// Table offset of the container at `body_off`, if it is indexed.
    pub fn table_for(&self, body_off: usize) -> Result<Option<usize>> {
        let i = self.lower_bound(body_off)?;
        if i < self.dir_len() {
            let (off, t) = self.dir_entry(i)?;
            if off == body_off {
                return Ok(Some(t));
            }
        }
        Ok(None)
    }

    /// Offset, relative to its container, of entry `i` of the table at `table`.
    pub fn entry(&self, table: usize, i: usize) -> Result<usize> {
        let at = table.checked_add(i).and_then(|v| v.checked_mul(4)).ok_or(corrupt(0))?;
        Ok(Cursor::u32_at(self.tables, at)? as usize)
    }

    /// Byte length of the tables area, in u32 units.
    pub fn tables_len(&self) -> usize {
        self.tables.len() / 4
    }

    /// Raw u32 slice of the tables between two table offsets.
    pub fn tables_bytes(&self, from: usize, to: usize) -> Result<&'a [u8]> {
        self.tables.get(from * 4..to * 4).ok_or(corrupt(from))
    }
}
