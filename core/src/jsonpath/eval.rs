//! Running a decoded path over a value (design §8 Path queries). A port of PostgreSQL's `jsonpath_exec.c`, keeping
//! its functions, flags and evaluation order: results, early exits and which errors silent mode suppresses all
//! depend on them. Recursion follows the path expression only, whose nesting the decoder caps; `.**` walks the
//! document with an explicit stack.

use core::cmp::Ordering;
use std::borrow::Cow;
use std::rc::Rc;

use super::{BinOp, DatetimeMethod, ItemId, Kind, Method, Path, UnOp};
use crate::ctx::Ticker;
use crate::cursor::{MAJOR_ARRAY, MAJOR_MAP, MAJOR_NEGATIVE, MAJOR_TEXT, MAJOR_UNSIGNED};
use crate::datum;
use crate::encode::write_head;
use crate::decimal::Num;
use crate::error::Error;
use crate::num;
use crate::ops::key_text;
use crate::out::OutBuf;
use crate::value::{ArrayIter, Kind as VKind, MapIter, Value};

/// SQLSTATEs of path errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    ArrayNotFound,
    ObjectNotFound,
    MemberNotFound,
    InvalidSubscript,
    SingletonRequired,
    NonNumericItem,
    NumberNotFound,
    UndefinedObject,
    InvalidParameter,
    NotSupported,
    Internal,
}

/// Why a path evaluation stopped. Errors raised by the host itself (PostgreSQL's own functions) never come back.
#[derive(Debug)]
pub enum PathError {
    /// A corrupt stored value, or a number beyond `numeric`'s range.
    Core(Error),
    Sql(Code, String),
}

impl From<Error> for PathError {
    fn from(e: Error) -> Self {
        PathError::Core(e)
    }
}

type R<T> = Result<T, PathError>;

/// What PostgreSQL computes with: `numeric` arithmetic, its input functions and regexes.
pub trait Host {
    /// `+ - * / %` on `numeric`. With `throw`, a failure (division by zero, overflow) raises PostgreSQL's error;
    /// otherwise it returns None.
    fn arith(&mut self, op: BinOp, a: &Num, b: &Num, throw: bool) -> Option<Num>;
    /// `float8in`: None when the text is not a valid `double precision`.
    fn parse_float8(&mut self, s: &str) -> Option<f64>;
    /// `float8_numeric` of a finite double.
    fn float8_numeric(&mut self, f: f64) -> Num;
    /// `int4in` (`bits64` false) or `int8in`: None when invalid or out of range.
    fn parse_int(&mut self, s: &str, bits64: bool) -> Option<i64>;
    /// `parse_bool`.
    fn parse_bool(&mut self, s: &str) -> Option<bool>;
    /// `numeric_in` with a typmod (-1 for none): None when invalid or not within the typmod.
    fn parse_numeric(&mut self, s: &str, typmod: i32) -> Option<Num>;
    /// `make_numeric_typmod_safe`: with `throw` an invalid precision or scale raises; otherwise None.
    fn numeric_typmod(&mut self, precision: i32, scale: i32, throw: bool) -> Option<i32>;
    /// `like_regex`, with the flags the path stores. Invalid patterns raise.
    fn regex_match(&mut self, pattern: &[u8], flags: u32, s: &[u8]) -> bool;
    /// Raises when the stack is close to its limit.
    fn check_stack(&mut self);
}

/// A generated `.keyvalue()` object: `{"id": id, "key": key, "value": value}`.
#[derive(Debug)]
pub struct KeyValue<'a> {
    pub key: JItem<'a>,
    pub value: JItem<'a>,
    pub id: i64,
}

/// An SQL/JSON item: a value inside the document or `vars`, or one computed by the path.
#[derive(Clone, Debug)]
pub enum JItem<'a> {
    Doc(Value<'a>),
    Null,
    Bool(bool),
    Num(Num),
    Str(Cow<'a, [u8]>),
    KeyValue(Rc<KeyValue<'a>>),
}

/// Item types, as jsonpath tells them apart. Byte strings, `undefined` and other simple values are CBOR's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ty {
    Null,
    Bool,
    Num,
    Str,
    Bytes,
    Undefined,
    Simple,
    Array,
    Object,
}

/// Tags the path sees through: all but numbers (2–5) and datetimes (0, 1).
fn peel(v: Value) -> R<Value> {
    let mut v = v;
    while let VKind::Tag(t) = v.kind()? {
        if t <= 5 && number_tag(v, t)? {
            break;
        }
        v = v.tag_content()?;
    }
    Ok(v)
}

/// Tags 2 and 3 are always numbers (normalization checks their content); 4 and 5 when they hold
/// `[integer, integer or bignum]`. Tags 0 and 1 stay transparent until datetimes are supported.
fn number_tag(v: Value, t: u64) -> R<bool> {
    Ok(match t {
        2 | 3 => true,
        4 | 5 => {
            let c = v.tag_content()?;
            let mut it = c.array_iter()?;
            let int = |x: Option<crate::Result<Value>>, big: bool| -> R<bool> {
                Ok(match x.transpose()? {
                    Some(x) => match x.kind()? {
                        VKind::Unsigned(_) | VKind::Negative(_) => true,
                        VKind::Tag(2 | 3) => big,
                        _ => false,
                    },
                    None => false,
                })
            };
            matches!(c.kind()?, VKind::Array(2)) && int(it.next(), false)? && int(it.next(), true)?
        }
        _ => false,
    })
}

fn ty(it: &JItem) -> R<Ty> {
    Ok(match it {
        JItem::Null => Ty::Null,
        JItem::Bool(_) => Ty::Bool,
        JItem::Num(_) => Ty::Num,
        JItem::Str(_) => Ty::Str,
        JItem::KeyValue(_) => Ty::Object,
        JItem::Doc(v) => {
            let v = peel(*v)?;
            match v.kind()? {
                VKind::Unsigned(_) | VKind::Negative(_) | VKind::Float(_) | VKind::Tag(_) => Ty::Num,
                VKind::Bytes(_) => Ty::Bytes,
                VKind::Text(_) => Ty::Str,
                VKind::Array(_) => Ty::Array,
                VKind::Map(_) => Ty::Object,
                VKind::False | VKind::True => Ty::Bool,
                VKind::Null => Ty::Null,
                VKind::Undefined => Ty::Undefined,
                VKind::Simple(_) => Ty::Simple,
            }
        }
    })
}

fn type_name(t: Ty) -> &'static str {
    match t {
        Ty::Null => "null",
        Ty::Bool => "boolean",
        Ty::Num => "number",
        Ty::Str => "string",
        Ty::Bytes => "bytes",
        Ty::Undefined => "undefined",
        Ty::Simple => "simple",
        Ty::Array => "array",
        Ty::Object => "object",
    }
}

/// The number an item holds, if it is one.
fn num_of(it: &JItem) -> R<Option<Num>> {
    let v = match it {
        JItem::Num(n) => return Ok(Some(n.clone())),
        JItem::Doc(v) => peel(*v)?,
        _ => return Ok(None),
    };
    Ok(match v.kind()? {
        VKind::Unsigned(n) => Some(i64::try_from(n).map_or_else(|_| Num::Dec(crate::decimal::Dec { neg: false, coef: n.to_string().into_bytes(), scale: 0 }), Num::Int)),
        VKind::Negative(n) if n < i64::MAX as u64 => Some(Num::Int(-1 - n as i64)),
        VKind::Float(f) if f.is_nan() => Some(Num::NaN),
        VKind::Float(f) if f.is_infinite() => Some(Num::Inf { neg: f < 0.0 }),
        VKind::Negative(_) | VKind::Float(_) | VKind::Tag(_) => match num::to_decimal(&v)? {
            Some(t) => Some(Num::parse(&t)?),
            None => None,
        },
        _ => None,
    })
}

/// The text an item holds, if it is a string.
fn str_of<'i>(it: &'i JItem) -> R<Option<&'i [u8]>> {
    Ok(match it {
        JItem::Str(s) => Some(s),
        JItem::Doc(v) => match peel(*v)?.kind()? {
            VKind::Text(t) => Some(t.as_bytes()),
            _ => None,
        },
        _ => None,
    })
}

fn bool_of(it: &JItem) -> R<Option<bool>> {
    Ok(match it {
        JItem::Bool(b) => Some(*b),
        JItem::Doc(v) => match peel(*v)?.kind()? {
            VKind::True => Some(true),
            VKind::False => Some(false),
            _ => None,
        },
        _ => None,
    })
}

/// The document container behind an item, tags peeled.
fn doc_container<'a>(it: &JItem<'a>) -> R<Option<Value<'a>>> {
    Ok(match it {
        JItem::Doc(v) => {
            let v = peel(*v)?;
            matches!(v.kind()?, VKind::Array(_) | VKind::Map(_)).then_some(v)
        }
        _ => None,
    })
}

/// `JsonbArraySize`: the length of an array item, or None.
fn array_size(it: &JItem) -> R<Option<u64>> {
    Ok(match it {
        JItem::Doc(v) => match peel(*v)?.kind()? {
            VKind::Array(n) => Some(n),
            _ => None,
        },
        _ => None,
    })
}

const KV_KEYS: [&[u8]; 3] = [b"id", b"key", b"value"];

/// Values of an array or object, in stored order (jsonb's order for text keys).
enum Children<'a> {
    Array(ArrayIter<'a>),
    Map(MapIter<'a>),
    Kv(Rc<KeyValue<'a>>, usize),
}

impl<'a> Children<'a> {
    fn of(it: &JItem<'a>) -> R<Option<Children<'a>>> {
        if let JItem::KeyValue(kv) = it {
            return Ok(Some(Children::Kv(kv.clone(), 0)));
        }
        Ok(match doc_container(it)? {
            Some(v) if matches!(v.kind()?, VKind::Array(_)) => Some(Children::Array(v.array_iter()?)),
            Some(v) => Some(Children::Map(v.map_iter()?)),
            None => None,
        })
    }

    fn next(&mut self) -> R<Option<JItem<'a>>> {
        Ok(match self {
            Children::Array(i) => i.next().transpose()?.map(JItem::Doc),
            Children::Map(i) => i.next().transpose()?.map(|(_, v)| JItem::Doc(v)),
            Children::Kv(kv, n) => {
                *n += 1;
                match *n {
                    1 => Some(JItem::Num(Num::Int(kv.id))),
                    2 => Some(kv.key.clone()),
                    3 => Some(kv.value.clone()),
                    _ => None,
                }
            }
        })
    }
}

fn is_container(it: &JItem) -> R<bool> {
    Ok(matches!(it, JItem::KeyValue(_)) || doc_container(it)?.is_some())
}

/// `jperOk`, `jperNotFound`, `jperError` (a suppressed error).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Res {
    Ok,
    NotFound,
    Error,
}

/// SQL/JSON's three-valued logic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tri {
    True,
    False,
    Unknown,
}

/// The object `.keyvalue()` ids are counted from: an id, and the container offsets are measured from.
#[derive(Clone, Copy)]
struct Base<'a> {
    anchor: Option<Value<'a>>,
    /// Added to offsets inside the anchor, so a generated object's own id (offset 0) stays distinct.
    extra: i64,
    id: i32,
}

type Found<'r, 'a> = Option<&'r mut Vec<JItem<'a>>>;

#[derive(Clone, Copy)]
enum Pred {
    Cmp(BinOp),
    StartsWith,
    /// The `like_regex` item.
    Regex(ItemId),
}

fn op_name(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
        _ => "?",
    }
}

fn method_name(m: Method) -> &'static str {
    match m {
        Method::Type => "type",
        Method::Size => "size",
        Method::Abs => "abs",
        Method::Floor => "floor",
        Method::Ceiling => "ceiling",
        Method::Double => "double",
        Method::KeyValue => "keyvalue",
        Method::Bigint => "bigint",
        Method::Boolean => "boolean",
        Method::Date => "date",
        Method::Integer => "integer",
        Method::Number => "number",
        Method::String => "string",
    }
}

fn lossy(s: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(s)
}

struct Exec<'a, 'h, H: Host> {
    path: &'a Path<'a>,
    host: &'h mut H,
    vars: Option<Value<'a>>,
    root: JItem<'a>,
    current: JItem<'a>,
    base: Base<'a>,
    last_generated_id: i32,
    innermost_array_size: i32,
    lax: bool,
    ignore_structural: bool,
    throw: bool,
    ticker: Ticker,
}

/// Runs `path` over `doc` (`executeJsonPath`). With `found`, every result is appended; without, evaluation stops
/// at the first result. `throw` false suppresses the errors jsonpath's silent mode suppresses, returning
/// [`Res::Error`] with the results found so far. `vars`, when given, must be a map.
pub fn execute<'a, H: Host>(path: &'a Path<'a>, doc: Value<'a>, vars: Option<Value<'a>>, throw: bool, host: &mut H, found: Found<'_, 'a>) -> R<Res> {
    let vars = match vars {
        Some(v) => {
            let v = peel(v)?;
            if !matches!(v.kind()?, VKind::Map(_)) {
                return Err(PathError::Sql(Code::InvalidParameter, "\"vars\" argument is not an object".into()));
            }
            Some(v)
        }
        None => None,
    };
    let root = JItem::Doc(doc);
    let mut cx = Exec {
        path,
        host,
        vars,
        current: root.clone(),
        root,
        base: Base { anchor: None, extra: 0, id: 0 },
        last_generated_id: 1 + i32::from(vars.is_some()),
        innermost_array_size: -1,
        lax: path.lax,
        ignore_structural: path.lax,
        throw,
        ticker: Ticker::default(),
    };
    let root = cx.root.clone();
    if !cx.lax && found.is_none() {
        // Strict mode must see every result to know there is no error.
        let mut vals = Vec::new();
        let res = cx.exec(path.root, &root, Some(&mut vals))?;
        if res == Res::Error {
            return Ok(res);
        }
        return Ok(if vals.is_empty() { Res::NotFound } else { Res::Ok });
    }
    cx.exec(path.root, &root, found)
}

impl<'a, H: Host> Exec<'a, '_, H> {
    /// `RETURN_ERROR`: raise, or report a suppressed error.
    fn soft(&self, code: Code, msg: impl FnOnce() -> String) -> R<Res> {
        if self.throw { Err(PathError::Sql(code, msg())) } else { Ok(Res::Error) }
    }

    fn exec(&mut self, id: ItemId, jb: &JItem<'a>, found: Found<'_, 'a>) -> R<Res> {
        self.exec_opt_unwrap_target(id, jb, found, self.lax)
    }

    fn next_item(&mut self, next: Option<ItemId>, v: JItem<'a>, found: Found<'_, 'a>) -> R<Res> {
        match next {
            Some(n) => self.exec(n, &v, found),
            None => {
                if let Some(f) = found {
                    f.push(v);
                }
                Ok(Res::Ok)
            }
        }
    }

    fn exec_opt_unwrap_target(&mut self, id: ItemId, jb: &JItem<'a>, mut found: Found<'_, 'a>, unwrap: bool) -> R<Res> {
        self.host.check_stack();
        self.ticker.tick();
        let item = self.path.item(id);
        let next = item.next;
        match &item.kind {
            Kind::Null | Kind::Bool(_) | Kind::Numeric(_) | Kind::String(_) | Kind::Variable(_) => {
                // A missing variable must still raise its error.
                if next.is_none() && found.is_none() && !matches!(item.kind, Kind::Variable(_)) {
                    return Ok(Res::Ok);
                }
                let saved = self.base;
                let v = self.get_item(id)?;
                let res = self.next_item(next, v, found);
                self.base = saved;
                res
            }
            Kind::Binary(BinOp::And | BinOp::Or | BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::StartsWith, ..)
            | Kind::Unary(UnOp::Not | UnOp::IsUnknown | UnOp::Exists, _)
            | Kind::LikeRegex { .. } => {
                let st = self.bool_item(id, jb, true)?;
                self.append_bool_result(next, found, st)
            }
            Kind::Binary(op, l, r) => self.binary_arith(*op, *l, *r, next, jb, found),
            Kind::Unary(op @ (UnOp::Plus | UnOp::Minus), a) => self.unary_arith(*op, *a, next, jb, found),
            Kind::AnyArray => {
                if array_size(jb)?.is_some() {
                    self.unwrap_target_array(next, jb, found, self.lax)
                } else if self.lax {
                    self.next_item(next, jb.clone(), found)
                } else if !self.ignore_structural {
                    self.soft(Code::ArrayNotFound, || "jsonpath wildcard array accessor can only be applied to an array".into())
                } else {
                    Ok(Res::NotFound)
                }
            }
            Kind::AnyKey => {
                if ty(jb)? == Ty::Object {
                    self.any_item(next, jb, found, 1, 1, 1, false, self.lax)
                } else if unwrap && array_size(jb)?.is_some() {
                    self.unwrap_target_array(Some(id), jb, found, false)
                } else if !self.ignore_structural {
                    self.soft(Code::ObjectNotFound, || "jsonpath wildcard member accessor can only be applied to an object".into())
                } else {
                    Ok(Res::NotFound)
                }
            }
            Kind::IndexArray(subs) => self.index_array(subs, next, jb, found),
            Kind::Any { first, last } => {
                let (first, last) = (*first, *last);
                let mut res = Res::NotFound;
                // First the item itself, when zero steps are allowed.
                if first == 0 {
                    let saved = self.ignore_structural;
                    self.ignore_structural = true;
                    res = self.next_item(next, jb.clone(), found.as_deref_mut())?;
                    self.ignore_structural = saved;
                    if res == Res::Ok && found.is_none() {
                        return Ok(res);
                    }
                }
                if is_container(jb)? {
                    res = self.any_item(next, jb, found, 1, first, last, true, self.lax)?;
                }
                Ok(res)
            }
            Kind::Key(k) => {
                if ty(jb)? == Ty::Object {
                    match self.member(jb, k)? {
                        Some(v) => self.next_item(next, v, found),
                        None if !self.ignore_structural => self.soft(Code::MemberNotFound, || format!("JSON object does not contain key \"{}\"", lossy(k))),
                        None => Ok(Res::NotFound),
                    }
                } else if unwrap && array_size(jb)?.is_some() {
                    self.unwrap_target_array(Some(id), jb, found, false)
                } else if !self.ignore_structural {
                    self.soft(Code::MemberNotFound, || "jsonpath member accessor can only be applied to an object".into())
                } else {
                    Ok(Res::NotFound)
                }
            }
            Kind::Current => self.next_item(next, self.current.clone(), found),
            Kind::Root => {
                let root = self.root.clone();
                let saved = self.base;
                self.base = Base { anchor: doc_container(&root)?, extra: 0, id: 0 };
                let res = self.next_item(next, root, found);
                self.base = saved;
                res
            }
            Kind::Unary(UnOp::Filter, arg) => {
                if unwrap && array_size(jb)?.is_some() {
                    return self.unwrap_target_array(Some(id), jb, found, false);
                }
                if self.nested_bool(*arg, jb)? != Tri::True {
                    return Ok(Res::NotFound);
                }
                self.next_item(next, jb.clone(), found)
            }
            Kind::Method(Method::Type) => {
                let t = type_name(ty(jb)?);
                self.next_item(next, JItem::Str(Cow::Borrowed(t.as_bytes())), found)
            }
            Kind::Method(Method::Size) => {
                let size = match array_size(jb)? {
                    Some(n) => n,
                    None if !self.lax => {
                        if !self.ignore_structural {
                            return self.soft(Code::ArrayNotFound, || "jsonpath item method .size() can only be applied to an array".into());
                        }
                        return Ok(Res::NotFound);
                    }
                    None => 1,
                };
                self.next_item(next, JItem::Num(Num::Int(size as i64)), found)
            }
            Kind::Method(m @ (Method::Abs | Method::Floor | Method::Ceiling)) => {
                if unwrap && array_size(jb)?.is_some() {
                    return self.unwrap_target_array(Some(id), jb, found, false);
                }
                let Some(n) = num_of(jb)? else {
                    return self.soft(Code::NonNumericItem, || format!("jsonpath item method .{}() can only be applied to a numeric value", method_name(*m)));
                };
                let n = match m {
                    Method::Abs => n.abs(),
                    Method::Floor => n.floor_ceil(false),
                    _ => n.floor_ceil(true),
                };
                if next.is_none() && found.is_none() {
                    return Ok(Res::Ok);
                }
                self.next_item(next, JItem::Num(n), found)
            }
            Kind::Method(m) => {
                if unwrap && array_size(jb)?.is_some() {
                    return self.unwrap_target_array(Some(id), jb, found, false);
                }
                match m {
                    Method::KeyValue => self.keyvalue(next, jb, found),
                    Method::Date => Err(PathError::Sql(Code::NotSupported, "jsonpath item method .date() is not supported for cbor yet".into())),
                    _ => match self.convert(*m, None, jb)? {
                        Ok(v) => self.next_item(next, v, found),
                        Err(res) => Ok(res),
                    },
                }
            }
            Kind::Decimal(p, s) => {
                if unwrap && array_size(jb)?.is_some() {
                    return self.unwrap_target_array(Some(id), jb, found, false);
                }
                match self.convert(Method::Number, Some((*p, *s)), jb)? {
                    Ok(v) => self.next_item(next, v, found),
                    Err(res) => Ok(res),
                }
            }
            Kind::Datetime(m, _) => {
                if unwrap && array_size(jb)?.is_some() {
                    return self.unwrap_target_array(Some(id), jb, found, false);
                }
                let name = match m {
                    DatetimeMethod::Datetime => "datetime",
                    DatetimeMethod::Time => "time",
                    DatetimeMethod::TimeTz => "time_tz",
                    DatetimeMethod::Timestamp => "timestamp",
                    DatetimeMethod::TimestampTz => "timestamp_tz",
                };
                Err(PathError::Sql(Code::NotSupported, format!("jsonpath item method .{name}() is not supported for cbor yet")))
            }
            Kind::Last => {
                if self.innermost_array_size < 0 {
                    return Err(PathError::Sql(Code::Internal, "evaluating jsonpath LAST outside of array subscript".into()));
                }
                if next.is_none() && found.is_none() {
                    return Ok(Res::Ok);
                }
                let last = i64::from(self.innermost_array_size) - 1;
                self.next_item(next, JItem::Num(Num::Int(last)), found)
            }
        }
    }

    /// `[subscripts]`.
    fn index_array(&mut self, subs: &[(ItemId, Option<ItemId>)], next: Option<ItemId>, jb: &JItem<'a>, mut found: Found<'_, 'a>) -> R<Res> {
        let size = array_size(jb)?;
        if size.is_none() && !self.lax {
            if !self.ignore_structural {
                return self.soft(Code::ArrayNotFound, || "jsonpath array accessor can only be applied to an array".into());
            }
            return Ok(Res::NotFound);
        }
        let singleton = size.is_none();
        let size = i64::try_from(size.unwrap_or(1)).unwrap_or(i64::MAX);
        let saved_size = self.innermost_array_size;
        self.innermost_array_size = i32::try_from(size).unwrap_or(i32::MAX);
        let array = doc_container(jb)?;
        let mut res = Res::NotFound;
        'subs: for &(from, to) in subs {
            let Some(index_from) = self.array_index(from, jb)? else {
                res = Res::Error;
                break;
            };
            let index_to = match to {
                Some(to) => match self.array_index(to, jb)? {
                    Some(i) => i,
                    None => {
                        res = Res::Error;
                        break;
                    }
                },
                None => index_from,
            };
            let (mut index_from, mut index_to) = (i64::from(index_from), i64::from(index_to));
            if !self.ignore_structural && (index_from < 0 || index_from > index_to || index_to >= size) {
                self.innermost_array_size = saved_size;
                return self.soft(Code::InvalidSubscript, || "jsonpath array subscript is out of bounds".into());
            }
            index_from = index_from.max(0);
            index_to = index_to.min(size - 1);
            res = Res::NotFound;
            for index in index_from..=index_to {
                let v = if singleton {
                    jb.clone()
                } else {
                    match array.map(|a| a.array_get(index)).transpose()?.flatten() {
                        Some(v) => JItem::Doc(v),
                        None => continue,
                    }
                };
                if next.is_none() && found.is_none() {
                    self.innermost_array_size = saved_size;
                    return Ok(Res::Ok);
                }
                res = self.next_item(next, v, found.as_deref_mut())?;
                if res == Res::Error || (res == Res::Ok && found.is_none()) {
                    break 'subs;
                }
            }
        }
        self.innermost_array_size = saved_size;
        Ok(res)
    }

    /// `executeItemUnwrapTargetArray`.
    fn unwrap_target_array(&mut self, id: Option<ItemId>, jb: &JItem<'a>, found: Found<'_, 'a>, unwrap_elements: bool) -> R<Res> {
        self.any_item(id, jb, found, 1, 1, 1, false, unwrap_elements)
    }

    /// `executeItemOptUnwrapResult`: in lax mode with `unwrap`, array results are replaced by their elements.
    fn exec_opt_unwrap_result(&mut self, id: ItemId, jb: &JItem<'a>, unwrap: bool, found: &mut Vec<JItem<'a>>) -> R<Res> {
        if unwrap && self.lax {
            let mut seq = Vec::new();
            let res = self.exec(id, jb, Some(&mut seq))?;
            if res == Res::Error {
                return Ok(res);
            }
            for item in seq {
                if array_size(&item)?.is_some() {
                    self.unwrap_target_array(None, &item, Some(found), false)?;
                } else {
                    found.push(item);
                }
            }
            return Ok(Res::Ok);
        }
        self.exec(id, jb, Some(found))
    }

    fn exec_opt_unwrap_result_no_throw(&mut self, id: ItemId, jb: &JItem<'a>, unwrap: bool, found: &mut Vec<JItem<'a>>) -> R<Res> {
        let saved = self.throw;
        self.throw = false;
        let res = self.exec_opt_unwrap_result(id, jb, unwrap, found);
        self.throw = saved;
        res
    }

    fn bool_item(&mut self, id: ItemId, jb: &JItem<'a>, can_have_next: bool) -> R<Tri> {
        self.host.check_stack();
        let item = self.path.item(id);
        if !can_have_next && item.next.is_some() {
            return Err(PathError::Sql(Code::Internal, "boolean jsonpath item cannot have next item".into()));
        }
        Ok(match item.kind {
            Kind::Binary(BinOp::And, l, r) => {
                let res = self.bool_item(l, jb, false)?;
                if res == Tri::False {
                    return Ok(Tri::False);
                }
                // SQL/JSON evaluates the right side even after an unknown left side.
                let res2 = self.bool_item(r, jb, false)?;
                if res2 == Tri::True { res } else { res2 }
            }
            Kind::Binary(BinOp::Or, l, r) => {
                let res = self.bool_item(l, jb, false)?;
                if res == Tri::True {
                    return Ok(Tri::True);
                }
                let res2 = self.bool_item(r, jb, false)?;
                if res2 == Tri::False { res } else { res2 }
            }
            Kind::Unary(UnOp::Not, a) => match self.bool_item(a, jb, false)? {
                Tri::Unknown => Tri::Unknown,
                Tri::True => Tri::False,
                Tri::False => Tri::True,
            },
            Kind::Unary(UnOp::IsUnknown, a) => {
                if self.bool_item(a, jb, false)? == Tri::Unknown { Tri::True } else { Tri::False }
            }
            Kind::Binary(op @ (BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge), l, r) => self.predicate(l, Some(r), jb, true, Pred::Cmp(op))?,
            Kind::Binary(BinOp::StartsWith, l, r) => self.predicate(l, Some(r), jb, false, Pred::StartsWith)?,
            Kind::LikeRegex { expr, .. } => self.predicate(expr, None, jb, false, Pred::Regex(id))?,
            Kind::Unary(UnOp::Exists, a) => {
                let mut vals = Vec::new();
                if !self.lax {
                    // Strict mode must see every result to know there is no error.
                    if self.exec_opt_unwrap_result_no_throw(a, jb, false, &mut vals)? == Res::Error {
                        return Ok(Tri::Unknown);
                    }
                    if vals.is_empty() { Tri::False } else { Tri::True }
                } else {
                    let saved = self.throw;
                    self.throw = false;
                    let res = self.exec(a, jb, None);
                    self.throw = saved;
                    match res? {
                        Res::Error => Tri::Unknown,
                        Res::Ok => Tri::True,
                        Res::NotFound => Tri::False,
                    }
                }
            }
            _ => return Err(PathError::Sql(Code::Internal, "invalid boolean jsonpath item type".into())),
        })
    }

    /// A filter's condition, with `@` bound to the filtered item.
    fn nested_bool(&mut self, id: ItemId, jb: &JItem<'a>) -> R<Tri> {
        let prev = core::mem::replace(&mut self.current, jb.clone());
        let res = self.bool_item(id, jb, false);
        self.current = prev;
        res
    }

    /// `executeAnyItem`: visits the values of `jb` and, down to `last` levels, of the containers inside it,
    /// depth first; runs `id` (or collects the value) on each at level `first` or deeper.
    #[allow(clippy::too_many_arguments)]
    fn any_item(&mut self, id: Option<ItemId>, jb: &JItem<'a>, mut found: Found<'_, 'a>, level: u32, first: u32, last: u32, ignore_structural: bool, unwrap_next: bool) -> R<Res> {
        let mut res = Res::NotFound;
        if level > last {
            return Ok(res);
        }
        let Some(top) = Children::of(jb)? else { return Ok(res) };
        let mut stack = vec![(top, level)];
        while let Some((children, level)) = stack.last_mut() {
            let level = *level;
            self.ticker.tick();
            let Some(v) = children.next()? else {
                stack.pop();
                continue;
            };
            // `.**` alone (any depth) collects leaves only.
            let leaves_only = first == u32::MAX && last == u32::MAX;
            if level >= first || (leaves_only && !is_container(&v)?) {
                match id {
                    Some(id) => {
                        let saved = self.ignore_structural;
                        if ignore_structural {
                            self.ignore_structural = true;
                        }
                        let r = self.exec_opt_unwrap_target(id, &v, found.as_deref_mut(), unwrap_next);
                        self.ignore_structural = saved;
                        let r = r?;
                        res = r;
                        if r == Res::Error || (r == Res::Ok && found.is_none()) {
                            return Ok(r);
                        }
                    }
                    None => match found.as_deref_mut() {
                        Some(f) => f.push(v.clone()),
                        None => return Ok(Res::Ok),
                    },
                }
            }
            if level < last
                && let Some(c) = Children::of(&v)?
            {
                // PostgreSQL's recursive call starts with "not found" and returns its last result.
                res = Res::NotFound;
                stack.push((c, level + 1));
            }
        }
        Ok(res)
    }

    /// `executePredicate`: true when any pair of left and right results satisfies the predicate. Strict mode checks
    /// every pair, so that an error anywhere makes the result unknown.
    fn predicate(&mut self, larg: ItemId, rarg: Option<ItemId>, jb: &JItem<'a>, unwrap_right: bool, pred: Pred) -> R<Tri> {
        let mut lseq = Vec::new();
        if self.exec_opt_unwrap_result_no_throw(larg, jb, true, &mut lseq)? == Res::Error {
            return Ok(Tri::Unknown);
        }
        let mut rseq = Vec::new();
        if let Some(r) = rarg
            && self.exec_opt_unwrap_result_no_throw(r, jb, unwrap_right, &mut rseq)? == Res::Error
        {
            return Ok(Tri::Unknown);
        }
        let (mut error, mut found) = (false, false);
        for l in &lseq {
            let rights: Vec<Option<&JItem<'a>>> = if rarg.is_some() { rseq.iter().map(Some).collect() } else { vec![None] };
            for r in rights {
                let res = match pred {
                    Pred::Cmp(op) => compare_items(op, l, r.unwrap_or(&JItem::Null))?,
                    Pred::StartsWith => starts_with(l, r.unwrap_or(&JItem::Null))?,
                    Pred::Regex(id) => self.like_regex(id, l)?,
                };
                match res {
                    Tri::Unknown if !self.lax => return Ok(Tri::Unknown),
                    Tri::Unknown => error = true,
                    Tri::True if self.lax => return Ok(Tri::True),
                    Tri::True => found = true,
                    Tri::False => {}
                }
            }
        }
        Ok(if found {
            Tri::True
        } else if error {
            Tri::Unknown
        } else {
            Tri::False
        })
    }

    fn like_regex(&mut self, id: ItemId, s: &JItem<'a>) -> R<Tri> {
        let Kind::LikeRegex { pattern, flags, .. } = self.path.item(id).kind else { return Ok(Tri::Unknown) };
        Ok(match str_of(s)? {
            Some(s) if self.host.regex_match(pattern, flags, s) => Tri::True,
            Some(_) => Tri::False,
            None => Tri::Unknown,
        })
    }

    fn binary_arith(&mut self, op: BinOp, l: ItemId, r: ItemId, next: Option<ItemId>, jb: &JItem<'a>, found: Found<'_, 'a>) -> R<Res> {
        // PostgreSQL unwraps the operands of every binary operator, not only the multiplicative ones.
        let mut lseq = Vec::new();
        let res = self.exec_opt_unwrap_result(l, jb, true, &mut lseq)?;
        if res == Res::Error {
            return Ok(res);
        }
        let mut rseq = Vec::new();
        let res = self.exec_opt_unwrap_result(r, jb, true, &mut rseq)?;
        if res == Res::Error {
            return Ok(res);
        }
        let single = |seq: &[JItem]| -> R<Option<Num>> { if seq.len() == 1 { num_of(&seq[0]) } else { Ok(None) } };
        let Some(a) = single(&lseq)? else {
            return self.soft(Code::SingletonRequired, || format!("left operand of jsonpath operator {} is not a single numeric value", op_name(op)));
        };
        let Some(b) = single(&rseq)? else {
            return self.soft(Code::SingletonRequired, || format!("right operand of jsonpath operator {} is not a single numeric value", op_name(op)));
        };
        let fast = match (op, &a, &b) {
            (BinOp::Add, Num::Int(x), Num::Int(y)) => x.checked_add(*y),
            (BinOp::Sub, Num::Int(x), Num::Int(y)) => x.checked_sub(*y),
            (BinOp::Mul, Num::Int(x), Num::Int(y)) => x.checked_mul(*y),
            _ => None,
        };
        let n = match fast {
            Some(n) => Num::Int(n),
            None => match self.host.arith(op, &a, &b, self.throw) {
                Some(n) => n,
                None => return Ok(Res::Error),
            },
        };
        if next.is_none() && found.is_none() {
            return Ok(Res::Ok);
        }
        self.next_item(next, JItem::Num(n), found)
    }

    fn unary_arith(&mut self, op: UnOp, arg: ItemId, next: Option<ItemId>, jb: &JItem<'a>, mut found: Found<'_, 'a>) -> R<Res> {
        let mut seq = Vec::new();
        let res = self.exec_opt_unwrap_result(arg, jb, true, &mut seq)?;
        if res == Res::Error {
            return Ok(res);
        }
        let mut res = Res::NotFound;
        for v in &seq {
            let Some(n) = num_of(v)? else {
                if found.is_none() && next.is_none() {
                    continue;
                }
                let name = if op == UnOp::Plus { "+" } else { "-" };
                return self.soft(Code::NumberNotFound, || format!("operand of unary jsonpath operator {name} is not a numeric value"));
            };
            if found.is_none() && next.is_none() {
                return Ok(Res::Ok);
            }
            let n = if op == UnOp::Minus { n.neg() } else { n };
            match self.next_item(next, JItem::Num(n), found.as_deref_mut())? {
                Res::Error => return Ok(Res::Error),
                Res::Ok if found.is_none() => return Ok(Res::Ok),
                Res::Ok => res = Res::Ok,
                Res::NotFound => {}
            }
        }
        Ok(res)
    }

    /// `.key` on an object: text keys only.
    fn member(&self, jb: &JItem<'a>, k: &[u8]) -> R<Option<JItem<'a>>> {
        if let JItem::KeyValue(kv) = jb {
            return Ok(match k {
                b"id" => Some(JItem::Num(Num::Int(kv.id))),
                b"key" => Some(kv.key.clone()),
                b"value" => Some(kv.value.clone()),
                _ => None,
            });
        }
        Ok(match doc_container(jb)? {
            Some(m) => m.map_get(&key_text(k))?.map(JItem::Doc),
            None => None,
        })
    }

    /// Literal or variable value (`getJsonPathItem`); a variable becomes the base object for `.keyvalue()` ids.
    fn get_item(&mut self, id: ItemId) -> R<JItem<'a>> {
        Ok(match &self.path.item(id).kind {
            Kind::Null => JItem::Null,
            Kind::Bool(b) => JItem::Bool(*b),
            Kind::Numeric(n) => JItem::Num(n.clone()),
            Kind::String(s) => JItem::Str(Cow::Borrowed(s)),
            Kind::Variable(name) => {
                let v = match self.vars {
                    Some(vars) => vars.map_get(&key_text(name))?,
                    None => None,
                };
                let Some(v) = v else {
                    return Err(PathError::Sql(Code::UndefinedObject, format!("could not find jsonpath variable \"{}\"", lossy(name))));
                };
                self.base = Base { anchor: self.vars, extra: 0, id: 1 };
                JItem::Doc(v)
            }
            _ => return Err(PathError::Sql(Code::Internal, "unexpected jsonpath item type".into())),
        })
    }

    fn append_bool_result(&mut self, next: Option<ItemId>, found: Found<'_, 'a>, st: Tri) -> R<Res> {
        if next.is_none() && found.is_none() {
            return Ok(Res::Ok);
        }
        let v = match st {
            Tri::Unknown => JItem::Null,
            t => JItem::Bool(t == Tri::True),
        };
        self.next_item(next, v, found)
    }

    /// A subscript's value truncated to an integer; None after a suppressed error.
    fn array_index(&mut self, id: ItemId, jb: &JItem<'a>) -> R<Option<i32>> {
        let mut found = Vec::new();
        if self.exec(id, jb, Some(&mut found))? == Res::Error {
            return Ok(None);
        }
        let n = if found.len() == 1 { num_of(&found[0])? } else { None };
        let Some(n) = n else {
            self.soft(Code::InvalidSubscript, || "jsonpath array subscript is not a single numeric value".into())?;
            return Ok(None);
        };
        match n.trunc_i32() {
            Some(i) => Ok(Some(i)),
            None => {
                self.soft(Code::InvalidSubscript, || "jsonpath array subscript is out of integer range".into())?;
                Ok(None)
            }
        }
    }

    /// `.keyvalue()`: one generated object per entry, each a new base object.
    fn keyvalue(&mut self, next: Option<ItemId>, jb: &JItem<'a>, mut found: Found<'_, 'a>) -> R<Res> {
        if ty(jb)? != Ty::Object {
            return self.soft(Code::ObjectNotFound, || "jsonpath item method .keyvalue() can only be applied to an object".into());
        }
        // The object's id: its base object's id, and its offset inside that base.
        let offset = match (doc_container(jb)?, self.base.anchor) {
            (Some(c), Some(a)) if core::ptr::eq(c.d, a.d) && c.pos >= a.pos => (c.pos - a.pos) as i64 + self.base.extra,
            _ => 0,
        };
        let id = offset + i64::from(self.base.id) * 10_000_000_000;
        let mut entries: Vec<(JItem<'a>, JItem<'a>)> = Vec::new();
        match jb {
            JItem::KeyValue(kv) => {
                entries.push((JItem::Str(Cow::Borrowed(KV_KEYS[0])), JItem::Num(Num::Int(kv.id))));
                entries.push((JItem::Str(Cow::Borrowed(KV_KEYS[1])), kv.key.clone()));
                entries.push((JItem::Str(Cow::Borrowed(KV_KEYS[2])), kv.value.clone()));
            }
            _ => {
                if let Some(m) = doc_container(jb)? {
                    for e in m.map_iter()? {
                        let (k, v) = e?;
                        entries.push((JItem::Doc(k), JItem::Doc(v)));
                    }
                }
            }
        }
        let mut res = Res::NotFound;
        for (key, value) in entries {
            res = Res::Ok;
            if next.is_none() && found.is_none() {
                break;
            }
            let anchor = doc_container(&value)?;
            let obj = JItem::KeyValue(Rc::new(KeyValue { key, value, id }));
            let saved = self.base;
            self.base = Base { anchor, extra: 1, id: self.last_generated_id };
            self.last_generated_id = self.last_generated_id.saturating_add(1);
            let r = self.next_item(next, obj, found.as_deref_mut());
            self.base = saved;
            res = r?;
            if res == Res::Error || (res == Res::Ok && found.is_none()) {
                return Ok(res);
            }
        }
        Ok(res)
    }

    /// The conversion methods `.double()`, `.bigint()`, `.boolean()`, `.integer()`, `.number()`, `.decimal()` and
    /// `.string()`. `Err` carries the result after a suppressed error.
    fn convert(&mut self, m: Method, decimal: Option<(Option<ItemId>, Option<ItemId>)>, jb: &JItem<'a>) -> R<Result<JItem<'a>, Res>> {
        let name = if decimal.is_some() { "decimal" } else { method_name(m) };
        let invalid = |arg: &str, ty: &str| format!("argument \"{arg}\" of jsonpath item method .{name}() is invalid for type {ty}");
        let num = num_of(jb)?;
        let text = str_of(jb)?.map(|s| lossy(s).into_owned());
        macro_rules! fail {
            ($code:expr, $msg:expr) => {
                return Ok(Err(self.soft($code, || $msg)?))
            };
        }
        let only = |kinds: &str| format!("jsonpath item method .{name}() can only be applied to {kinds}");
        Ok(Ok(match m {
            Method::Double => {
                if let Some(n) = &num {
                    let t = n.to_text();
                    match self.host.parse_float8(&t) {
                        None => fail!(Code::NonNumericItem, invalid(&t, "double precision")),
                        Some(f) if !f.is_finite() => fail!(Code::NonNumericItem, format!("NaN or Infinity is not allowed for jsonpath item method .{name}()")),
                        Some(_) => jb.clone(),
                    }
                } else if let Some(t) = &text {
                    match self.host.parse_float8(t) {
                        None => fail!(Code::NonNumericItem, invalid(t, "double precision")),
                        Some(f) if !f.is_finite() => fail!(Code::NonNumericItem, format!("NaN or Infinity is not allowed for jsonpath item method .{name}()")),
                        Some(f) => JItem::Num(self.host.float8_numeric(f)),
                    }
                } else {
                    fail!(Code::NonNumericItem, only("a string or numeric value"))
                }
            }
            Method::Bigint | Method::Integer => {
                let (bits64, ty) = if m == Method::Bigint { (true, "bigint") } else { (false, "integer") };
                let fits = |i: i64| bits64 || i32::try_from(i).is_ok();
                if let Some(n) = &num {
                    match n.round_i64().filter(|&i| fits(i)) {
                        Some(i) => JItem::Num(Num::Int(i)),
                        None => fail!(Code::NonNumericItem, invalid(&n.to_text(), ty)),
                    }
                } else if let Some(t) = &text {
                    match self.host.parse_int(t, bits64) {
                        Some(i) => JItem::Num(Num::Int(i)),
                        None => fail!(Code::NonNumericItem, invalid(t, ty)),
                    }
                } else {
                    fail!(Code::NonNumericItem, only("a string or numeric value"))
                }
            }
            Method::Boolean => {
                if let Some(b) = bool_of(jb)? {
                    JItem::Bool(b)
                } else if let Some(n) = &num {
                    let t = n.to_text();
                    match self.host.parse_int(&t, false) {
                        Some(i) => JItem::Bool(i != 0),
                        None => fail!(Code::NonNumericItem, invalid(&t, "boolean")),
                    }
                } else if let Some(t) = &text {
                    match self.host.parse_bool(t) {
                        Some(b) => JItem::Bool(b),
                        None => fail!(Code::NonNumericItem, invalid(t, "boolean")),
                    }
                } else {
                    fail!(Code::NonNumericItem, only("a boolean, string, or numeric value"))
                }
            }
            Method::Number => {
                let special = || format!("NaN or Infinity is not allowed for jsonpath item method .{name}()");
                let (n, numstr) = if let Some(n) = num {
                    if matches!(n, Num::NaN | Num::Inf { .. }) {
                        fail!(Code::NonNumericItem, special());
                    }
                    let t = n.to_text();
                    (n, t)
                } else if let Some(t) = text {
                    match self.host.parse_numeric(&t, -1) {
                        None => fail!(Code::NonNumericItem, invalid(&t, "numeric")),
                        Some(Num::NaN | Num::Inf { .. }) => fail!(Code::NonNumericItem, special()),
                        Some(n) => (n, t),
                    }
                } else {
                    fail!(Code::NonNumericItem, only("a string or numeric value"))
                };
                match decimal {
                    Some((Some(p), s)) => {
                        let arg = |cx: &Self, id: ItemId| match &cx.path.item(id).kind {
                            Kind::Numeric(n) => n.round_i64().and_then(|i| i32::try_from(i).ok()).ok_or(()),
                            _ => Err(()),
                        };
                        let Ok(precision) = arg(self, p) else {
                            fail!(Code::NonNumericItem, format!("precision of jsonpath item method .{name}() is out of range for type integer"))
                        };
                        let scale = match s {
                            Some(s) => match arg(self, s) {
                                Ok(s) => s,
                                Err(()) => fail!(Code::NonNumericItem, format!("scale of jsonpath item method .{name}() is out of range for type integer")),
                            },
                            None => 0,
                        };
                        let Some(typmod) = self.host.numeric_typmod(precision, scale, self.throw) else { return Ok(Err(Res::Error)) };
                        match self.host.parse_numeric(&numstr, typmod) {
                            Some(n) => JItem::Num(n),
                            None => fail!(Code::NonNumericItem, invalid(&numstr, "numeric")),
                        }
                    }
                    _ => JItem::Num(n),
                }
            }
            Method::String => {
                if text.is_some() {
                    match jb {
                        JItem::Doc(v) => match peel(*v)?.kind()? {
                            VKind::Text(t) => JItem::Str(Cow::Borrowed(t.as_bytes())),
                            _ => jb.clone(),
                        },
                        _ => jb.clone(),
                    }
                } else if let Some(n) = &num {
                    JItem::Str(Cow::Owned(n.to_text().into_bytes()))
                } else if let Some(b) = bool_of(jb)? {
                    JItem::Str(Cow::Borrowed(if b { b"true" } else { b"false" }))
                } else {
                    fail!(Code::NonNumericItem, only("a boolean, string, numeric, or datetime value"))
                }
            }
            _ => return Err(PathError::Sql(Code::Internal, "unexpected jsonpath item method".into())),
        }))
    }
}

/// `compareItems`: items of different types are unknown, except that null is unequal to everything else.
fn compare_items(op: BinOp, a: &JItem, b: &JItem) -> R<Tri> {
    let (ta, tb) = (ty(a)?, ty(b)?);
    if ta != tb {
        if ta == Ty::Null || tb == Ty::Null {
            return Ok(if op == BinOp::Ne { Tri::True } else { Tri::False });
        }
        return Ok(Tri::Unknown);
    }
    let cmp = match ta {
        Ty::Null | Ty::Undefined => Ordering::Equal,
        Ty::Bool => bool_of(a)?.cmp(&bool_of(b)?),
        Ty::Num => match (num_of(a)?, num_of(b)?) {
            (Some(x), Some(y)) => x.cmp_value(&y),
            _ => return Ok(Tri::Unknown),
        },
        // Strings compare by code point, which for UTF-8 is byte order.
        Ty::Str => str_of(a)?.cmp(&str_of(b)?),
        Ty::Bytes | Ty::Simple => scalar_bytes(a)?.cmp(&scalar_bytes(b)?),
        Ty::Array | Ty::Object => return Ok(Tri::Unknown),
    };
    let res = match op {
        BinOp::Eq => cmp == Ordering::Equal,
        BinOp::Ne => cmp != Ordering::Equal,
        BinOp::Lt => cmp == Ordering::Less,
        BinOp::Gt => cmp == Ordering::Greater,
        BinOp::Le => cmp != Ordering::Greater,
        BinOp::Ge => cmp != Ordering::Less,
        _ => return Err(PathError::Sql(Code::Internal, "unrecognized jsonpath operation".into())),
    };
    Ok(if res { Tri::True } else { Tri::False })
}

/// Content of byte strings and the number of simple values, for comparing them.
fn scalar_bytes<'i>(it: &JItem<'i>) -> R<Vec<u8>> {
    Ok(match it {
        JItem::Doc(v) => match peel(*v)?.kind()? {
            VKind::Bytes(b) => b.to_vec(),
            VKind::Simple(n) => vec![n],
            _ => Vec::new(),
        },
        _ => Vec::new(),
    })
}

fn starts_with(whole: &JItem, initial: &JItem) -> R<Tri> {
    Ok(match (str_of(whole)?, str_of(initial)?) {
        (Some(w), Some(i)) if w.starts_with(i) => Tri::True,
        (Some(_), Some(_)) => Tri::False,
        _ => Tri::Unknown,
    })
}

/// `Some(Some(b))` for a boolean, `Some(None)` for null, None for anything else (`jsonb_path_match`'s result).
pub fn bool_or_null(it: &JItem) -> R<Option<Option<bool>>> {
    Ok(match ty(it)? {
        Ty::Null => Some(None),
        Ty::Bool => Some(bool_of(it)?),
        _ => None,
    })
}

/// Appends the body of an item. Numbers keep their scale as `numeric` → `cbor` does.
fn write_body(it: &JItem, out: &mut impl OutBuf) -> crate::Result<()> {
    match it {
        JItem::Doc(v) => out.write_bytes(v.bytes()?),
        JItem::Null => out.write_byte(0xf6),
        JItem::Bool(b) => out.write_byte(if *b { 0xf5 } else { 0xf4 }),
        JItem::Num(Num::Int(i)) if *i >= 0 => write_head(out, MAJOR_UNSIGNED, *i as u64),
        JItem::Num(Num::Int(i)) => write_head(out, MAJOR_NEGATIVE, (-1 - *i) as u64),
        JItem::Num(n) => num::write_numeric(&n.to_text(), out),
        JItem::Str(s) => {
            write_head(out, MAJOR_TEXT, s.len() as u64)?;
            out.write_bytes(s)
        }
        JItem::KeyValue(kv) => {
            write_head(out, MAJOR_MAP, 3)?;
            for (k, v) in KV_KEYS.iter().zip([&JItem::Num(Num::Int(kv.id)), &kv.key, &kv.value]) {
                write_head(out, MAJOR_TEXT, k.len() as u64)?;
                out.write_bytes(k)?;
                write_body(v, out)?;
            }
            Ok(())
        }
    }
}

/// Appends a datum holding a result.
pub fn write_datum(it: &JItem, out: &mut impl OutBuf, max_depth: usize) -> crate::Result<()> {
    if let JItem::Doc(v) = it {
        return datum::write_subvalue(v.d, v.pos, v.end()?, out);
    }
    let base = out.len();
    out.write_byte(datum::VERSION)?;
    write_body(it, out)?;
    datum::finish_index(out, base, max_depth)
}

/// Appends a datum holding an array of results.
pub fn write_array(items: &[JItem], out: &mut impl OutBuf, max_depth: usize) -> crate::Result<()> {
    let base = out.len();
    out.write_byte(datum::VERSION)?;
    write_head(out, MAJOR_ARRAY, items.len() as u64)?;
    for it in items {
        write_body(it, out)?;
    }
    datum::finish_index(out, base, max_depth)
}
