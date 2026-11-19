//! Compiled paths built by hand, for tests: PostgreSQL's binary layout, without PostgreSQL.

use crate::decimal::Num;

/// Path items for building compiled paths by hand.
pub enum P {
    Root,
    Cur,
    Key(&'static str),
    AnyArray,
    Any(u32, u32),
    Num(&'static str),
    Str(&'static str),
    Bin(u8, Vec<P>, Vec<P>),
    Not(Vec<P>),
    Exists(Vec<P>),
    Filter(Vec<P>),
    Method(u8),
}

fn word(buf: &mut [u8], at: usize, v: i32) {
    buf[at..at + 4].copy_from_slice(&v.to_ne_bytes());
}

/// Writes a chain of items in PostgreSQL's layout; returns the first item's offset.
fn enc(buf: &mut Vec<u8>, chain: &[P]) -> usize {
    let (p, rest) = chain.split_first().unwrap();
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
    let start = buf.len();
    let typ = match p {
        P::Root => 27,
        P::Cur => 26,
        P::Key(_) => 25,
        P::AnyArray => 21,
        P::Any(..) => 24,
        P::Num(_) => 2,
        P::Str(_) => 1,
        P::Not(_) => 6,
        P::Exists(_) => 30,
        P::Bin(t, ..) => *t,
        P::Filter(_) => 29,
        P::Method(t) => *t,
    };
    buf.extend_from_slice(&[typ, 0, 0, 0, 0, 0, 0, 0]);
    let slot = |buf: &mut Vec<u8>| {
        buf.extend_from_slice(&[0; 4]);
        buf.len() - 4
    };
    match p {
        P::Key(k) | P::Str(k) => {
            buf.extend_from_slice(&(k.len() as u32).to_ne_bytes());
            buf.extend_from_slice(k.as_bytes());
            buf.push(0);
        }
        P::Any(a, b) => {
            buf.extend_from_slice(&a.to_ne_bytes());
            buf.extend_from_slice(&b.to_ne_bytes());
        }
        P::Num(t) => Num::parse(t).unwrap().write_numeric(buf).unwrap(),
        P::Bin(_, l, r) => {
            let (ls, rs) = (slot(buf), slot(buf));
            let lo = enc(buf, l);
            word(buf, ls, (lo - start) as i32);
            let ro = enc(buf, r);
            word(buf, rs, (ro - start) as i32);
        }
        P::Filter(c) | P::Not(c) | P::Exists(c) => {
            let s = slot(buf);
            let o = enc(buf, c);
            word(buf, s, (o - start) as i32);
        }
        _ => {}
    }
    if !rest.is_empty() {
        let n = enc(buf, rest);
        word(buf, start + 4, (n - start) as i32);
    }
    start
}

pub fn compile(lax: bool, chain: &[P]) -> Vec<u8> {
    let mut data = Vec::new();
    enc(&mut data, chain);
    let mut v = (0x01u32 | if lax { 0x8000_0000 } else { 0 }).to_ne_bytes().to_vec();
    v.extend_from_slice(&data);
    v
}
