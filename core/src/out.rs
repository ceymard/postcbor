use crate::error::{Error, ErrorKind, Result};

/// Output buffer every core writer goes through. The glue backs it with `palloc` memory so results need no copy.
pub trait OutBuf {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn as_slice(&self) -> &[u8];
    fn as_mut_slice(&mut self) -> &mut [u8];
    /// Appends bytes. Fails with `OutOfMemory` instead of aborting.
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()>;
    /// Shrinks to `len` bytes; `len` must not exceed the current length.
    fn truncate(&mut self, len: usize);

    fn write_byte(&mut self, b: u8) -> Result<()> {
        self.write_bytes(&[b])
    }

    /// Inserts `bytes` at `at`, shifting what follows.
    fn insert(&mut self, at: usize, bytes: &[u8]) -> Result<()> {
        let old = self.len();
        self.write_bytes(bytes)?;
        let s = self.as_mut_slice();
        s.copy_within(at..old, at + bytes.len());
        s[at..at + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }
}

impl OutBuf for Vec<u8> {
    fn len(&self) -> usize {
        Vec::len(self)
    }
    fn as_slice(&self) -> &[u8] {
        self
    }
    fn as_mut_slice(&mut self) -> &mut [u8] {
        self
    }
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.try_reserve(bytes.len())
            .map_err(|_| Error::new(ErrorKind::OutOfMemory, 0))?;
        self.extend_from_slice(bytes);
        Ok(())
    }
    fn truncate(&mut self, len: usize) {
        Vec::truncate(self, len)
    }
}

/// `Vec::push` that reports allocation failure instead of aborting.
pub fn try_push<T>(v: &mut Vec<T>, item: T) -> Result<()> {
    v.try_reserve(1)
        .map_err(|_| Error::new(ErrorKind::OutOfMemory, 0))?;
    v.push(item);
    Ok(())
}
