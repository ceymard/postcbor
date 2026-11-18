use crate::error::{Error, ErrorKind, Result};

pub const MAJOR_UNSIGNED: u8 = 0;
pub const MAJOR_NEGATIVE: u8 = 1;
pub const MAJOR_BYTES: u8 = 2;
pub const MAJOR_TEXT: u8 = 3;
pub const MAJOR_ARRAY: u8 = 4;
pub const MAJOR_MAP: u8 = 5;
pub const MAJOR_TAG: u8 = 6;
pub const MAJOR_SIMPLE: u8 = 7;

pub const INFO_INDEFINITE: u8 = 31;
pub const BREAK: u8 = 0xff;

/// A decoded item head. For major 7, `arg` holds the simple value or the raw float bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    pub major: u8,
    pub info: u8,
    pub arg: u64,
}

impl Head {
    pub fn is_indefinite(&self) -> bool {
        self.info == INFO_INDEFINITE
    }
    pub fn is_break(&self) -> bool {
        self.major == MAJOR_SIMPLE && self.info == INFO_INDEFINITE
    }
}

/// Bounds-checked reader. The only code allowed to index into input or body bytes.
#[derive(Clone, Copy, Debug)]
pub struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Cursor { buf, pos: 0 }
    }

    pub fn at(buf: &'a [u8], pos: usize) -> Self {
        Cursor { buf, pos }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn buf(&self) -> &'a [u8] {
        self.buf
    }

    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    pub fn err(&self, kind: ErrorKind) -> Error {
        Error::new(kind, self.pos)
    }

    pub fn peek(&self) -> Result<u8> {
        self.buf
            .get(self.pos)
            .copied()
            .ok_or_else(|| self.err(ErrorKind::Truncated))
    }

    pub fn byte(&mut self) -> Result<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Ok(b)
    }

    /// Takes `n` bytes; fails before any allocation could be sized from `n`.
    pub fn take(&mut self, n: u64) -> Result<&'a [u8]> {
        let n = usize::try_from(n).map_err(|_| self.err(ErrorKind::Truncated))?;
        if n > self.remaining() {
            return Err(self.err(ErrorKind::Truncated));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn read_u32_le(&mut self) -> Result<u32> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn uint_be(&mut self, n: u64) -> Result<u64> {
        Ok(self
            .take(n)?
            .iter()
            .fold(0u64, |acc, &b| (acc << 8) | u64::from(b)))
    }

    /// Reads an item head. Rejects reserved additional information and indefinite length on majors 0, 1 and 6.
    pub fn head(&mut self) -> Result<Head> {
        let start = self.pos;
        let ib = self.byte()?;
        let major = ib >> 5;
        let info = ib & 0x1f;
        let arg = match info {
            0..=23 => u64::from(info),
            24 => self.uint_be(1)?,
            25 => self.uint_be(2)?,
            26 => self.uint_be(4)?,
            27 => self.uint_be(8)?,
            INFO_INDEFINITE if matches!(major, MAJOR_BYTES..=MAJOR_MAP | MAJOR_SIMPLE) => 0,
            _ => return Err(Error::new(ErrorKind::Reserved, start)),
        };
        Ok(Head { major, info, arg })
    }

    /// Reads the u32 at byte offset `at`, without moving.
    pub fn u32_at(buf: &[u8], at: usize) -> Result<u32> {
        Cursor::at(buf, at)
            .read_u32_le()
            .map_err(|e| Error::new(ErrorKind::CorruptDatum, e.offset))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_rejects_reserved_info_and_bad_indefinite() {
        for b in [0x1c, 0x3d, 0x5e, 0x1f, 0x3f, 0xdf] {
            assert_eq!(Cursor::new(&[b]).head().unwrap_err().kind, ErrorKind::Reserved, "{b:#x}");
        }
    }

    #[test]
    fn take_refuses_lengths_beyond_input() {
        let mut c = Cursor::new(&[1, 2, 3]);
        assert_eq!(c.take(u64::MAX).unwrap_err().kind, ErrorKind::Truncated);
        assert_eq!(c.take(4).unwrap_err().kind, ErrorKind::Truncated);
        assert_eq!(c.take(3).unwrap(), &[1, 2, 3]);
    }
}
