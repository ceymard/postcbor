//! PostgreSQL `jsonpath` values (design §8 Path queries): decoding the compiled binary form into a tree of items.
//! The layout follows PostgreSQL's `jspInitByBuffer`: each item is a type byte, padding to 4, the offset of the next
//! item, then type-specific fields; child offsets are relative to the item's start and 0 means "absent".

pub mod eval;
pub mod gin;
pub mod print;
#[cfg(test)]
pub(crate) mod test_path;

use crate::cursor::Cursor;
use crate::decimal::{Dec, Num};
use crate::error::{Error, ErrorKind, NO_OFFSET, Result};

const _: () = assert!(MAX_NESTING == 256, "update the nesting error message");

/// Deepest nesting of items accepted, counting both arguments and chained steps.
pub const MAX_NESTING: usize = 256;

const VERSION: u32 = 0x01;
const LAX: u32 = 0x8000_0000;

/// Index of an item in [`Path::items`].
pub type ItemId = usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    And,
    Or,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    StartsWith,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Not,
    IsUnknown,
    Plus,
    Minus,
    Filter,
    Exists,
}

/// Item methods without arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Type,
    Size,
    Abs,
    Floor,
    Ceiling,
    Double,
    KeyValue,
    Bigint,
    Boolean,
    Integer,
    Number,
    String,
}

/// Datetime methods, with an optional template (`.datetime()`) or precision (time methods) argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatetimeMethod {
    Datetime,
    Date,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind<'a> {
    Null,
    String(&'a [u8]),
    Numeric(Num),
    Bool(bool),
    Binary(BinOp, ItemId, ItemId),
    Unary(UnOp, ItemId),
    AnyArray,
    AnyKey,
    /// Subscripts: `from` or `from to to`.
    IndexArray(Vec<(ItemId, Option<ItemId>)>),
    /// `.**{first to last}`; `u32::MAX` stands for `last`.
    Any { first: u32, last: u32 },
    Key(&'a [u8]),
    Current,
    Root,
    Variable(&'a [u8]),
    Method(Method),
    Datetime(DatetimeMethod, Option<ItemId>),
    /// `.decimal(precision, scale)`.
    Decimal(Option<ItemId>, Option<ItemId>),
    Last,
    LikeRegex { expr: ItemId, pattern: &'a [u8], flags: u32 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item<'a> {
    pub kind: Kind<'a>,
    pub next: Option<ItemId>,
}

/// A decoded path. Items borrow strings from the `jsonpath` value.
#[derive(Clone, Debug, PartialEq)]
pub struct Path<'a> {
    pub lax: bool,
    pub items: Vec<Item<'a>>,
    pub root: ItemId,
}

fn bad(msg: &'static str) -> Error {
    Error::new(ErrorKind::InvalidArgument(msg), NO_OFFSET)
}

fn malformed() -> Error {
    bad("malformed jsonpath value")
}

/// Pending decoding work: the item at `offset` gets id `id`.
struct Todo {
    offset: usize,
    id: ItemId,
    depth: usize,
}

impl<'a> Path<'a> {
    /// Decodes the payload of a `jsonpath` varlena (header word and items). Fails on unknown versions and item
    /// types, on offsets that do not point forward inside the value, and on nesting deeper than [`MAX_NESTING`].
    pub fn decode(payload: &'a [u8]) -> Result<Path<'a>> {
        let mut c = Cursor::new(payload);
        let header = c.read_u32_ne().map_err(|_| malformed())?;
        if header & !LAX != VERSION {
            return Err(bad("unsupported jsonpath version"));
        }
        let data = payload.get(4..).ok_or_else(malformed)?;
        let mut items: Vec<Option<Item<'a>>> = vec![None];
        // PostgreSQL writes a tree: an item referenced twice would escape the nesting cap. One bit per offset.
        let mut seen = vec![0u64; data.len() / 64 + 1];
        let mut mark = |off: usize| -> bool {
            let (w, b) = (off / 64, 1u64 << (off % 64));
            let fresh = seen[w] & b == 0;
            seen[w] |= b;
            fresh
        };
        mark(0);
        let mut todo = vec![Todo { offset: 0, id: 0, depth: 1 }];
        while let Some(t) = todo.pop() {
            let mut c = Cursor::at(data, t.offset);
            let typ = c.byte().map_err(|_| malformed())?;
            c.align(4).map_err(|_| malformed())?;
            let next_rel = c.read_u32_ne().map_err(|_| malformed())? as i32;
            // Child references: absolute offset → new id.
            let mut child = |rel: i32| -> Result<ItemId> {
                let off = usize::try_from(rel).ok().filter(|&r| r > 0).and_then(|r| t.offset.checked_add(r)).ok_or_else(malformed)?;
                if off >= data.len() {
                    return Err(malformed());
                }
                if t.depth >= MAX_NESTING {
                    return Err(bad("jsonpath nests deeper than 256 items"));
                }
                if !mark(off) {
                    return Err(malformed());
                }
                let id = items.len();
                items.push(None);
                todo.push(Todo { offset: off, id, depth: t.depth + 1 });
                Ok(id)
            };
            let opt = |rel: i32, child: &mut dyn FnMut(i32) -> Result<ItemId>| -> Result<Option<ItemId>> {
                if rel == 0 { Ok(None) } else { child(rel).map(Some) }
            };
            let i32_ = |c: &mut Cursor| -> Result<i32> { Ok(c.read_u32_ne().map_err(|_| malformed())? as i32) };
            let string = |c: &mut Cursor<'a>| -> Result<&'a [u8]> {
                let len = c.read_u32_ne().map_err(|_| malformed())?;
                c.take(u64::from(len)).map_err(|_| malformed())
            };
            let bin = |op: BinOp, c: &mut Cursor, child: &mut dyn FnMut(i32) -> Result<ItemId>| -> Result<Kind<'a>> {
                let l = c.read_u32_ne().map_err(|_| malformed())? as i32;
                let r = c.read_u32_ne().map_err(|_| malformed())? as i32;
                Ok(Kind::Binary(op, child(l)?, child(r)?))
            };
            use BinOp as B;
            use Method as M;
            let kind = match typ {
                0 => Kind::Null,
                1 => Kind::String(string(&mut c)?),
                2 => Kind::Numeric(Num::from_dec(Dec::from_numeric(c.buf().get(c.pos()..).ok_or_else(malformed)?)?)),
                3 => Kind::Bool(c.byte().map_err(|_| malformed())? != 0),
                4 => bin(B::And, &mut c, &mut child)?,
                5 => bin(B::Or, &mut c, &mut child)?,
                6 => Kind::Unary(UnOp::Not, child(i32_(&mut c)?)?),
                7 => Kind::Unary(UnOp::IsUnknown, child(i32_(&mut c)?)?),
                8 => bin(B::Eq, &mut c, &mut child)?,
                9 => bin(B::Ne, &mut c, &mut child)?,
                10 => bin(B::Lt, &mut c, &mut child)?,
                11 => bin(B::Gt, &mut c, &mut child)?,
                12 => bin(B::Le, &mut c, &mut child)?,
                13 => bin(B::Ge, &mut c, &mut child)?,
                14 => bin(B::Add, &mut c, &mut child)?,
                15 => bin(B::Sub, &mut c, &mut child)?,
                16 => bin(B::Mul, &mut c, &mut child)?,
                17 => bin(B::Div, &mut c, &mut child)?,
                18 => bin(B::Mod, &mut c, &mut child)?,
                19 => Kind::Unary(UnOp::Plus, child(i32_(&mut c)?)?),
                20 => Kind::Unary(UnOp::Minus, child(i32_(&mut c)?)?),
                21 => Kind::AnyArray,
                22 => Kind::AnyKey,
                23 => {
                    let n = i32_(&mut c)?;
                    let n = usize::try_from(n).map_err(|_| malformed())?;
                    if n > c.remaining() / 8 {
                        return Err(malformed());
                    }
                    let mut subs = Vec::with_capacity(n);
                    for _ in 0..n {
                        let from = i32_(&mut c)?;
                        let to = i32_(&mut c)?;
                        subs.push((child(from)?, opt(to, &mut child)?));
                    }
                    Kind::IndexArray(subs)
                }
                24 => {
                    let first = c.read_u32_ne().map_err(|_| malformed())?;
                    let last = c.read_u32_ne().map_err(|_| malformed())?;
                    Kind::Any { first, last }
                }
                25 => Kind::Key(string(&mut c)?),
                26 => Kind::Current,
                27 => Kind::Root,
                28 => Kind::Variable(string(&mut c)?),
                29 => Kind::Unary(UnOp::Filter, child(i32_(&mut c)?)?),
                30 => Kind::Unary(UnOp::Exists, child(i32_(&mut c)?)?),
                31 => Kind::Method(M::Type),
                32 => Kind::Method(M::Size),
                33 => Kind::Method(M::Abs),
                34 => Kind::Method(M::Floor),
                35 => Kind::Method(M::Ceiling),
                36 => Kind::Method(M::Double),
                37 => Kind::Datetime(DatetimeMethod::Datetime, opt(i32_(&mut c)?, &mut child)?),
                38 => Kind::Method(M::KeyValue),
                // 39 (array subscript) only appears inside IndexArray's own fields.
                40 => Kind::Last,
                41 => bin(B::StartsWith, &mut c, &mut child)?,
                42 => {
                    let flags = c.read_u32_ne().map_err(|_| malformed())?;
                    let expr = i32_(&mut c)?;
                    let pattern = string(&mut c)?;
                    Kind::LikeRegex { expr: child(expr)?, pattern, flags }
                }
                43 => Kind::Method(M::Bigint),
                44 => Kind::Method(M::Boolean),
                45 => Kind::Datetime(DatetimeMethod::Date, None),
                46 => {
                    let p = i32_(&mut c)?;
                    let s = i32_(&mut c)?;
                    Kind::Decimal(opt(p, &mut child)?, opt(s, &mut child)?)
                }
                47 => Kind::Method(M::Integer),
                48 => Kind::Method(M::Number),
                49 => Kind::Method(M::String),
                50 => Kind::Datetime(DatetimeMethod::Time, opt(i32_(&mut c)?, &mut child)?),
                51 => Kind::Datetime(DatetimeMethod::TimeTz, opt(i32_(&mut c)?, &mut child)?),
                52 => Kind::Datetime(DatetimeMethod::Timestamp, opt(i32_(&mut c)?, &mut child)?),
                53 => Kind::Datetime(DatetimeMethod::TimestampTz, opt(i32_(&mut c)?, &mut child)?),
                _ => return Err(bad("unsupported jsonpath item")),
            };
            let next = if next_rel > 0 { Some(child(next_rel)?) } else { None };
            *items.get_mut(t.id).ok_or_else(malformed)? = Some(Item { kind, next });
        }
        let items = items.into_iter().collect::<Option<Vec<_>>>().ok_or_else(malformed)?;
        Ok(Path { lax: header & LAX != 0, items, root: 0 })
    }

    pub fn item(&self, id: ItemId) -> &Item<'a> {
        &self.items[id]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(v: &mut Vec<u8>, x: i32) {
        v.extend_from_slice(&x.to_ne_bytes());
    }

    /// A lax path: `n` nested `!(…)` around `null`.
    fn nots(n: usize) -> Vec<u8> {
        let mut v = Vec::new();
        word(&mut v, (VERSION | LAX) as i32);
        for _ in 0..n {
            v.extend_from_slice(&[6, 0, 0, 0]);
            word(&mut v, 0);
            word(&mut v, 12); // the argument follows this item
        }
        v.extend_from_slice(&[0, 0, 0, 0]);
        word(&mut v, 0);
        v
    }

    #[test]
    fn nesting_is_capped_at_256_items() {
        assert_eq!(Path::decode(&nots(MAX_NESTING - 1)).unwrap().items.len(), MAX_NESTING);
        assert!(Path::decode(&nots(MAX_NESTING)).is_err());
    }

    /// `null && null` whose operands are at the given offsets from the `&&` item.
    fn and(left: i32, right: i32) -> Vec<u8> {
        let mut v = Vec::new();
        word(&mut v, (VERSION | LAX) as i32);
        v.extend_from_slice(&[4, 0, 0, 0]);
        word(&mut v, 0);
        word(&mut v, left);
        word(&mut v, right);
        for _ in 0..2 {
            v.extend_from_slice(&[0, 0, 0, 0]);
            word(&mut v, 0);
        }
        v
    }

    #[test]
    fn items_referenced_twice_are_refused() {
        assert_eq!(Path::decode(&and(16, 24)).unwrap().items.len(), 3);
        assert!(Path::decode(&and(16, 16)).is_err());
    }
}
