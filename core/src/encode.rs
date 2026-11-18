//! Shortest-form head encoding.

use crate::error::Result;
use crate::out::OutBuf;

/// Encodes a head in its shortest form into `buf`; returns the length used.
pub fn head_bytes(major: u8, arg: u64, buf: &mut [u8; 9]) -> usize {
    let mt = major << 5;
    if arg < 24 {
        buf[0] = mt | arg as u8;
        1
    } else if arg <= 0xff {
        buf[0] = mt | 24;
        buf[1] = arg as u8;
        2
    } else if arg <= 0xffff {
        buf[0] = mt | 25;
        buf[1..3].copy_from_slice(&(arg as u16).to_be_bytes());
        3
    } else if arg <= 0xffff_ffff {
        buf[0] = mt | 26;
        buf[1..5].copy_from_slice(&(arg as u32).to_be_bytes());
        5
    } else {
        buf[0] = mt | 27;
        buf[1..9].copy_from_slice(&arg.to_be_bytes());
        9
    }
}

pub fn write_head(out: &mut impl OutBuf, major: u8, arg: u64) -> Result<()> {
    let mut b = [0u8; 9];
    let n = head_bytes(major, arg, &mut b);
    out.write_bytes(&b[..n])
}

pub fn insert_head(out: &mut impl OutBuf, at: usize, major: u8, arg: u64) -> Result<()> {
    let mut b = [0u8; 9];
    let n = head_bytes(major, arg, &mut b);
    out.insert(at, &b[..n])
}
