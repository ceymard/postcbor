//! Running a decoded path over a value (design §8 Path queries). A port of PostgreSQL's `jsonpath_exec.c`, keeping
//! its functions, flags and evaluation order: results, early exits and which errors silent mode suppresses all
//! depend on them. Recursion follows the path expression only, whose nesting the decoder caps; `.**` walks the
//! document with an explicit stack.

use core::cmp::Ordering;
use std::borrow::Cow;
use std::rc::Rc;

use super::{BinOp, DatetimeMethod, ItemId, Kind, Method, Path, UnOp};
use crate::ctx::Ticker;
use crate::cursor::{MAJOR_ARRAY, MAJOR_BYTES, MAJOR_MAP, MAJOR_NEGATIVE, MAJOR_SIMPLE, MAJOR_TAG, MAJOR_TEXT, MAJOR_UNSIGNED};
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
    DatetimeArgument,
    Internal,
}

/// Why a path evaluation stopped. Errors raised by the host itself (PostgreSQL's own functions) never come back.
/// Boxed: results carry it on every return, and it is rare.
#[derive(Debug)]
pub struct PathError(pub Box<PathFail>);

#[derive(Debug)]
pub enum PathFail {
    /// A corrupt stored value, or a number beyond `numeric`'s range.
    Core(Error),
    Sql(Code, String),
}

impl PathError {
    fn sql(code: Code, msg: impl Into<String>) -> Self {
        PathError(Box::new(PathFail::Sql(code, msg.into())))
    }
}

impl From<Error> for PathError {
    fn from(e: Error) -> Self {
        PathError(Box::new(PathFail::Core(e)))
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
    /// A datetime method once its input string is known (`executeDateTimeMethod`): parsing, then the conversion
    /// to the method's type. Errors PostgreSQL raises whatever the mode (a cast that needs the time zone outside
    /// `_tz` functions) are raised by the host.
    fn datetime(&mut self, m: DatetimeMethod, arg: DtArg, s: &[u8], throw: bool, use_tz: bool) -> DtOut;
    /// `compareDatetime`: None when the types cannot be compared.
    fn datetime_cmp(&mut self, a: &Datetime, b: &Datetime, use_tz: bool) -> Option<Ordering>;
    /// `JsonEncodeDateTime`: ISO 8601 text.
    fn datetime_text(&mut self, d: &Datetime) -> String;
    /// `timestamptz_in` of a tag 0 string: None when it is not a valid timestamp.
    fn timestamptz_in(&mut self, s: &str) -> Option<i64>;
    /// `to_timestamp` of a tag 1 number of seconds, within the range `timestamptz` holds.
    fn epoch_timestamptz(&mut self, secs: f64) -> i64;
    /// Raises when the stack is close to its limit.
    fn check_stack(&mut self);
}

/// PostgreSQL's datetime types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtType {
    Date,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
}

/// A datetime value as PostgreSQL holds it: `value` is the date (days), time or timestamp (microseconds); `zone`
/// the time zone of a `timetz`; `tz` the offset (seconds west of UTC) `timestamptz` values are shown in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Datetime {
    pub typ: DtType,
    pub value: i64,
    pub zone: i32,
    pub typmod: i32,
    pub tz: i32,
}

/// The argument of a datetime method.
#[derive(Clone, Copy, Debug)]
pub enum DtArg<'p> {
    None,
    Template(&'p [u8]),
    Precision(i32),
}

/// Outcome of [`Host::datetime`].
#[derive(Debug)]
pub enum DtOut {
    Ok(Datetime),
    /// A parse error suppressed in silent mode.
    Suppressed,
    /// An error silent mode suppresses (`RETURN_ERROR`).
    Fail(Code, String),
}

/// A generated `.keyvalue()` object: `{"id": id, "key": key, "value": value}`.
#[derive(Debug)]
pub struct KeyValue<'a> {
    pub key: JItem<'a>,
    pub value: JItem<'a>,
    pub id: i64,
}

/// An SQL/JSON item: a value inside the document or `vars`, or one computed by the path.
#[derive(Debug)]
pub enum JItem<'a> {
    Doc(Value<'a>),
    Null,
    Bool(bool),
    Num(Num),
    Str(&'a [u8]),
    /// A string the path computed (`.string()`, `.type()` of datetimes).
    OwnedStr(Box<[u8]>),
    KeyValue(Rc<KeyValue<'a>>),
    /// Boxed, like long decimals, to keep items small.
    Datetime(Box<Datetime>),
}

const _: () = assert!(core::mem::size_of::<JItem>() <= 24, "path items are moved often: keep them small");

impl Clone for JItem<'_> {
    /// Inline for the common variants; boxed ones call out.
    #[inline]
    fn clone(&self) -> Self {
        match self {
            JItem::Doc(v) => JItem::Doc(*v),
            JItem::Null => JItem::Null,
            JItem::Bool(b) => JItem::Bool(*b),
            JItem::Num(n) => JItem::Num(n.clone()),
            JItem::Str(s) => JItem::Str(s),
            other => other.clone_boxed(),
        }
    }
}

impl JItem<'_> {
    #[inline(never)]
    fn clone_boxed(&self) -> Self {
        match self {
            JItem::OwnedStr(s) => JItem::OwnedStr(s.clone()),
            JItem::KeyValue(kv) => JItem::KeyValue(kv.clone()),
            JItem::Datetime(d) => JItem::Datetime(d.clone()),
            JItem::Doc(v) => JItem::Doc(*v),
            JItem::Null => JItem::Null,
            JItem::Bool(b) => JItem::Bool(*b),
            JItem::Num(n) => JItem::Num(n.clone()),
            JItem::Str(s) => JItem::Str(s),
        }
    }
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
    Datetime,
}

/// Tags the path sees through: all but numbers (2–5) and datetimes (0, 1).
#[inline(always)]
pub(crate) fn peel(v: Value) -> R<Value> {
    let mut v = v;
    loop {
        let (h, next) = v.head()?;
        if h.major != MAJOR_TAG || (h.arg <= 5 && typed_tag(v, h.arg)?) {
            return Ok(v);
        }
        v = Value { d: v.d, pos: next };
    }
}

/// Whether a tag gives its content a type: tags 2 and 3 always (normalization checks their content); 4 and 5 when
/// they hold `[integer, integer or bignum]`; 0 around RFC 3339 text and 1 around a number of seconds that
/// `timestamptz` can hold.
#[inline(never)]
fn typed_tag(v: Value, t: u64) -> R<bool> {
    Ok(match t {
        0 => v.tag_content()?.text_bytes()?.and_then(|t| core::str::from_utf8(t).ok()).and_then(rfc3339_offset).is_some(),
        1 => epoch_seconds(v.tag_content()?)?.is_some(),
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

#[inline]
fn ty(it: &JItem) -> R<Ty> {
    Ok(match it {
        JItem::Null => Ty::Null,
        JItem::Bool(_) => Ty::Bool,
        JItem::Num(_) => Ty::Num,
        JItem::Str(_) | JItem::OwnedStr(_) => Ty::Str,
        JItem::KeyValue(_) => Ty::Object,
        JItem::Datetime(_) => Ty::Datetime,
        JItem::Doc(v) => {
            // From the head alone: `kind` would also check the UTF-8 of text.
            let (h, _) = peel(*v)?.head()?;
            match h.major {
                MAJOR_UNSIGNED | MAJOR_NEGATIVE => Ty::Num,
                MAJOR_BYTES => Ty::Bytes,
                MAJOR_TEXT => Ty::Str,
                MAJOR_ARRAY => Ty::Array,
                MAJOR_MAP => Ty::Object,
                MAJOR_TAG if h.arg <= 1 => Ty::Datetime,
                MAJOR_TAG => Ty::Num,
                _ => match h.info {
                    20 | 21 => Ty::Bool,
                    22 => Ty::Null,
                    23 => Ty::Undefined,
                    25..=27 => Ty::Num,
                    _ => Ty::Simple,
                },
            }
        }
    })
}

/// An item seen as jsonpath compares it, with tags peeled once.
enum Sv<'i> {
    Null,
    Bool(bool),
    NumRef(&'i Num),
    Num(Num),
    Str(&'i [u8]),
    Bytes(&'i [u8]),
    Undefined,
    Simple(u8),
    Container(Ty),
    Datetime,
}

impl Sv<'_> {
    fn ty(&self) -> Ty {
        match self {
            Sv::Null => Ty::Null,
            Sv::Bool(_) => Ty::Bool,
            Sv::Num(_) | Sv::NumRef(_) => Ty::Num,
            Sv::Str(_) => Ty::Str,
            Sv::Bytes(_) => Ty::Bytes,
            Sv::Undefined => Ty::Undefined,
            Sv::Simple(_) => Ty::Simple,
            Sv::Container(t) => *t,
            Sv::Datetime => Ty::Datetime,
        }
    }
}

#[inline(always)]
fn scalar<'i>(it: &'i JItem) -> R<Sv<'i>> {
    let v = match it {
        JItem::Null => return Ok(Sv::Null),
        JItem::Bool(b) => return Ok(Sv::Bool(*b)),
        JItem::Num(n) => return Ok(Sv::NumRef(n)),
        JItem::Str(s) => return Ok(Sv::Str(s)),
        JItem::OwnedStr(s) => return Ok(Sv::Str(s)),
        JItem::KeyValue(_) => return Ok(Sv::Container(Ty::Object)),
        JItem::Datetime(_) => return Ok(Sv::Datetime),
        JItem::Doc(v) => peel(*v)?,
    };
    if let Some(t) = v.text_bytes()? {
        return Ok(Sv::Str(t));
    }
    Ok(match v.kind()? {
        VKind::Unsigned(n) if n <= i64::MAX as u64 => Sv::Num(Num::Int(n as i64)),
        VKind::Negative(n) if n < i64::MAX as u64 => Sv::Num(Num::Int(-1 - n as i64)),
        VKind::Tag(0 | 1) => Sv::Datetime,
        VKind::Unsigned(_) | VKind::Negative(_) | VKind::Float(_) | VKind::Tag(_) => match num_of(&JItem::Doc(v))? {
            Some(n) => Sv::Num(n),
            // Not reachable for tags `peel` stopped at; compares as unknown.
            None => Sv::Container(Ty::Num),
        },
        VKind::Text(t) => Sv::Str(t.as_bytes()),
        VKind::Bytes(b) => Sv::Bytes(b),
        VKind::Array(_) => Sv::Container(Ty::Array),
        VKind::Map(_) => Sv::Container(Ty::Object),
        VKind::False => Sv::Bool(false),
        VKind::True => Sv::Bool(true),
        VKind::Null => Sv::Null,
        VKind::Undefined => Sv::Undefined,
        VKind::Simple(n) => Sv::Simple(n),
    })
}

/// `.type()`: datetime items name their PostgreSQL type; tags 0 and 1 are `timestamptz`.
fn type_name(it: &JItem) -> R<&'static str> {
    Ok(match it {
        JItem::Datetime(d) => match d.typ {
            DtType::Date => "date",
            DtType::Time => "time without time zone",
            DtType::TimeTz => "time with time zone",
            DtType::Timestamp => "timestamp without time zone",
            DtType::TimestampTz => "timestamp with time zone",
        },
        _ => ty_name(ty(it)?),
    })
}

fn ty_name(t: Ty) -> &'static str {
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
        Ty::Datetime => "timestamp with time zone",
    }
}

/// timestamptz's range: 4714-11-24 BC to 294276 AD, in seconds since 1970.
const EPOCH_RANGE: core::ops::Range<f64> = -210_866_803_200.0..9_224_318_016_000.0;

/// The seconds of a tag 1 datetime, if its content is a number within `timestamptz`'s range.
fn epoch_seconds(v: Value) -> R<Option<f64>> {
    let secs = match v.kind()? {
        VKind::Unsigned(n) => n as f64,
        VKind::Negative(n) => -1.0 - n as f64,
        VKind::Float(f) => f,
        _ => return Ok(None),
    };
    Ok(EPOCH_RANGE.contains(&secs).then_some(secs))
}

/// The number an item holds, if it is one. Numbers without a `numeric` value (a decimal fraction beyond its range)
/// give None, which comparisons treat as unknown.
pub(crate) fn num_of(it: &JItem) -> R<Option<Num>> {
    let v = match it {
        JItem::Num(n) => return Ok(Some(n.clone())),
        JItem::Doc(v) => peel(*v)?,
        _ => return Ok(None),
    };
    Ok(match v.kind()? {
        VKind::Unsigned(n) => Some(i64::try_from(n).map_or_else(|_| Num::Dec(Box::new(crate::decimal::Dec { neg: false, coef: n.to_string().into_bytes(), scale: 0 })), Num::Int)),
        VKind::Negative(n) if n < i64::MAX as u64 => Some(Num::Int(-1 - n as i64)),
        VKind::Float(f) if f.is_nan() => Some(Num::NaN),
        VKind::Float(f) if f.is_infinite() => Some(Num::Inf { neg: f < 0.0 }),
        VKind::Negative(_) | VKind::Float(_) | VKind::Tag(_) => match num::to_decimal(&v) {
            Ok(Some(t)) => Num::parse(&t).ok(),
            Ok(None) => None,
            Err(e) if e.kind == crate::error::ErrorKind::CorruptDatum => return Err(e.into()),
            Err(_) => None,
        },
        _ => None,
    })
}

/// The text an item holds, if it is a string.
#[inline]
fn str_of<'i>(it: &'i JItem) -> R<Option<&'i [u8]>> {
    Ok(match it {
        JItem::Str(s) => Some(s),
        JItem::OwnedStr(s) => Some(s),
        JItem::Doc(v) => peel(*v)?.text_bytes()?,
        _ => None,
    })
}

#[inline]
fn bool_of(it: &JItem) -> R<Option<bool>> {
    Ok(match it {
        JItem::Bool(b) => Some(*b),
        JItem::Doc(v) => match peel(*v)?.head()?.0 {
            h if h.major == MAJOR_SIMPLE && h.info == 21 => Some(true),
            h if h.major == MAJOR_SIMPLE && h.info == 20 => Some(false),
            _ => None,
        },
        _ => None,
    })
}

/// The document container behind an item, tags peeled.
#[inline(always)]
fn doc_container<'a>(it: &JItem<'a>) -> R<Option<Value<'a>>> {
    Ok(match it {
        JItem::Doc(v) => {
            let v = peel(*v)?;
            matches!(v.head()?.0.major, MAJOR_ARRAY | MAJOR_MAP).then_some(v)
        }
        _ => None,
    })
}

/// `JsonbArraySize`: the length of an array item, or None.
#[inline(always)]
fn array_size(it: &JItem) -> R<Option<u64>> {
    Ok(match it {
        JItem::Doc(v) => {
            let (h, _) = peel(*v)?.head()?;
            (h.major == MAJOR_ARRAY).then_some(h.arg)
        }
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

/// A list of results that holds one item without allocating, as PostgreSQL's `JsonValueList` does.
#[derive(Debug, Default)]
pub struct Seq<'a> {
    first: Option<JItem<'a>>,
    rest: Vec<JItem<'a>>,
}

impl<'a> Seq<'a> {
    #[inline]
    pub fn push(&mut self, it: JItem<'a>) {
        if self.first.is_none() {
            self.first = Some(it);
        } else {
            self.rest.push(it);
        }
    }

    pub fn len(&self) -> usize {
        usize::from(self.first.is_some()) + self.rest.len()
    }

    pub fn is_empty(&self) -> bool {
        self.first.is_none()
    }

    pub fn first(&self) -> Option<&JItem<'a>> {
        self.first.as_ref()
    }

    /// The only item, if there is exactly one.
    #[inline]
    pub fn single(&self) -> Option<&JItem<'a>> {
        if self.rest.is_empty() { self.first.as_ref() } else { None }
    }

    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = &JItem<'a>> {
        self.first.iter().chain(self.rest.iter())
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut JItem<'a>> {
        self.first.iter_mut().chain(self.rest.iter_mut())
    }

    fn truncate(&mut self, len: usize) {
        if len == 0 {
            self.first = None;
        }
        self.rest.truncate(len.saturating_sub(1));
    }

    /// Removes and returns the items from index `at` on.
    fn split_off(&mut self, at: usize) -> Vec<JItem<'a>> {
        if at == 0 {
            return core::mem::take(self).into_iter().collect();
        }
        self.rest.split_off((at - 1).min(self.rest.len()))
    }
}

impl<'a> IntoIterator for Seq<'a> {
    type Item = JItem<'a>;
    type IntoIter = core::iter::Chain<core::option::IntoIter<JItem<'a>>, std::vec::IntoIter<JItem<'a>>>;
    fn into_iter(self) -> Self::IntoIter {
        self.first.into_iter().chain(self.rest)
    }
}

type Found<'r, 'a> = Option<&'r mut Seq<'a>>;

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
    /// The root's container, the base object `$` sets.
    root_anchor: Option<Value<'a>>,
    current: JItem<'a>,
    base: Base<'a>,
    last_generated_id: i32,
    innermost_array_size: i32,
    lax: bool,
    ignore_structural: bool,
    throw: bool,
    use_tz: bool,
    ticker: Ticker,
    /// Counts recursive calls, to check the stack every [`STACK_CHECK_EVERY`] of them.
    calls: u32,
}

/// Recursion is bounded by the nesting cap; the host's stack check is a safety net, and a call across FFI.
const STACK_CHECK_EVERY: u32 = 16;

/// Runs `path` over `doc` (`executeJsonPath`). With `found`, every result is appended; without, evaluation stops
/// at the first result. `throw` false suppresses the errors jsonpath's silent mode suppresses, returning
/// [`Res::Error`] with the results found so far. `vars`, when given, must be a map. `use_tz` allows the datetime
/// casts that depend on the session time zone (the `_tz` functions).
#[allow(clippy::too_many_arguments)]
pub fn execute<'a, H: Host>(path: &'a Path<'a>, doc: Value<'a>, vars: Option<Value<'a>>, throw: bool, use_tz: bool, host: &mut H, found: Found<'_, 'a>) -> R<Res> {
    let vars = match vars {
        Some(v) => {
            let v = peel(v)?;
            if !matches!(v.kind()?, VKind::Map(_)) {
                return Err(PathError::sql(Code::InvalidParameter, "\"vars\" argument is not an object"));
            }
            Some(v)
        }
        None => None,
    };
    let mut cx = Exec {
        path,
        host,
        vars,
        current: JItem::Doc(doc),
        root: JItem::Doc(doc),
        root_anchor: doc_container(&JItem::Doc(doc))?,
        base: Base { anchor: None, extra: 0, id: 0 },
        last_generated_id: 1 + i32::from(vars.is_some()),
        innermost_array_size: -1,
        lax: path.lax,
        ignore_structural: path.lax,
        throw,
        use_tz,
        ticker: Ticker::default(),
        calls: 0,
    };
    let root = JItem::Doc(doc);
    if !cx.lax && found.is_none() {
        // Strict mode must see every result to know there is no error.
        let mut vals = Seq::default();
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
        if self.throw { Err(PathError::sql(code, msg())) } else { Ok(Res::Error) }
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

    /// Every recursive call: the stack check, and the interrupt check (through the ticker) at a sixteenth of the rate.
    #[inline(always)]
    fn enter(&mut self) {
        self.calls = self.calls.wrapping_add(1);
        if self.calls.is_multiple_of(STACK_CHECK_EVERY) {
            self.host.check_stack();
            self.ticker.tick();
        }
    }

    fn exec_opt_unwrap_target(&mut self, id: ItemId, jb: &JItem<'a>, mut found: Found<'_, 'a>, unwrap: bool) -> R<Res> {
        self.enter();
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
                self.base = Base { anchor: self.root_anchor, extra: 0, id: 0 };
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
                let t = type_name(jb)?;
                self.next_item(next, JItem::Str(t.as_bytes()), found)
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
            Kind::Datetime(m, arg) => {
                if unwrap && array_size(jb)?.is_some() {
                    return self.unwrap_target_array(Some(id), jb, found, false);
                }
                self.datetime(*m, *arg, next, jb, found)
            }
            Kind::Last => {
                if self.innermost_array_size < 0 {
                    return Err(PathError::sql(Code::Internal, "evaluating jsonpath LAST outside of array subscript"));
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
    fn exec_opt_unwrap_result(&mut self, id: ItemId, jb: &JItem<'a>, unwrap: bool, found: &mut Seq<'a>) -> R<Res> {
        if unwrap && self.lax {
            // Results go straight into `found`; only array results are then replaced by their elements.
            let start = found.len();
            let res = self.exec(id, jb, Some(found))?;
            if res == Res::Error {
                found.truncate(start);
                return Ok(res);
            }
            let mut arrays = false;
            for it in found.iter().skip(start) {
                arrays |= array_size(it)?.is_some();
            }
            if arrays {
                for item in found.split_off(start) {
                    if array_size(&item)?.is_some() {
                        self.unwrap_target_array(None, &item, Some(found), false)?;
                    } else {
                        found.push(item);
                    }
                }
            }
            return Ok(Res::Ok);
        }
        self.exec(id, jb, Some(found))
    }

    fn exec_opt_unwrap_result_no_throw(&mut self, id: ItemId, jb: &JItem<'a>, unwrap: bool, found: &mut Seq<'a>) -> R<Res> {
        let saved = self.throw;
        self.throw = false;
        let res = self.exec_opt_unwrap_result(id, jb, unwrap, found);
        self.throw = saved;
        res
    }

    fn bool_item(&mut self, id: ItemId, jb: &JItem<'a>, can_have_next: bool) -> R<Tri> {
        self.enter();
        let item = self.path.item(id);
        if !can_have_next && item.next.is_some() {
            return Err(PathError::sql(Code::Internal, "boolean jsonpath item cannot have next item"));
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
                let mut vals = Seq::default();
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
            _ => return Err(PathError::sql(Code::Internal, "invalid boolean jsonpath item type")),
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
        let mut lseq = Seq::default();
        match self.literal(larg) {
            Some(v) => lseq.push(v),
            None => {
                if self.exec_opt_unwrap_result_no_throw(larg, jb, true, &mut lseq)? == Res::Error {
                    return Ok(Tri::Unknown);
                }
            }
        }
        let mut rseq = Seq::default();
        if let Some(r) = rarg {
            match self.literal(r) {
                Some(v) => rseq.push(v),
                None => {
                    if self.exec_opt_unwrap_result_no_throw(r, jb, unwrap_right, &mut rseq)? == Res::Error {
                        return Ok(Tri::Unknown);
                    }
                }
            }
        }
        let (mut error, mut found) = (false, false);
        for l in lseq.iter() {
            let rights: &mut dyn Iterator<Item = Option<&JItem<'a>>> = if rarg.is_some() { &mut rseq.iter().map(Some) } else { &mut core::iter::once(None) };
            for r in rights {
                let res = match pred {
                    Pred::Cmp(op) => self.compare_items(op, l, r.unwrap_or(&JItem::Null))?,
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

    /// A literal operand, which evaluates to itself without error (a shortcut, not a semantic difference).
    #[inline]
    fn literal(&self, id: ItemId) -> Option<JItem<'a>> {
        let item = self.path.item(id);
        if item.next.is_some() {
            return None;
        }
        Some(match &item.kind {
            Kind::Null => JItem::Null,
            Kind::Bool(b) => JItem::Bool(*b),
            Kind::Numeric(n) => JItem::Num(n.clone()),
            Kind::String(s) => JItem::Str(s),
            _ => return None,
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
        let mut lseq = Seq::default();
        let res = self.exec_opt_unwrap_result(l, jb, true, &mut lseq)?;
        if res == Res::Error {
            return Ok(res);
        }
        let mut rseq = Seq::default();
        let res = self.exec_opt_unwrap_result(r, jb, true, &mut rseq)?;
        if res == Res::Error {
            return Ok(res);
        }
        let single = |seq: &Seq| -> R<Option<Num>> { seq.single().map_or(Ok(None), num_of) };
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
        let mut seq = Seq::default();
        let res = self.exec_opt_unwrap_result(arg, jb, true, &mut seq)?;
        if res == Res::Error {
            return Ok(res);
        }
        let mut res = Res::NotFound;
        for v in seq.iter() {
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
    #[inline(always)]
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
    #[inline]
    fn get_item(&mut self, id: ItemId) -> R<JItem<'a>> {
        Ok(match &self.path.item(id).kind {
            Kind::Null => JItem::Null,
            Kind::Bool(b) => JItem::Bool(*b),
            Kind::Numeric(n) => JItem::Num(n.clone()),
            Kind::String(s) => JItem::Str(s),
            Kind::Variable(name) => {
                let v = match self.vars {
                    Some(vars) => vars.map_get(&key_text(name))?,
                    None => None,
                };
                let Some(v) = v else {
                    return Err(PathError::sql(Code::UndefinedObject, format!("could not find jsonpath variable \"{}\"", lossy(name))));
                };
                self.base = Base { anchor: self.vars, extra: 0, id: 1 };
                JItem::Doc(v)
            }
            _ => return Err(PathError::sql(Code::Internal, "unexpected jsonpath item type")),
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
        let mut found = Seq::default();
        if self.exec(id, jb, Some(&mut found))? == Res::Error {
            return Ok(None);
        }
        let n = found.single().map_or(Ok(None), num_of)?;
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
                entries.push((JItem::Str(KV_KEYS[0]), JItem::Num(Num::Int(kv.id))));
                entries.push((JItem::Str(KV_KEYS[1]), kv.key.clone()));
                entries.push((JItem::Str(KV_KEYS[2]), kv.value.clone()));
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

    /// `compareItems`: items of different types are unknown, except that null is unequal to everything else.
    fn compare_items(&mut self, op: BinOp, a: &JItem<'a>, b: &JItem<'a>) -> R<Tri> {
        let (sa, sb) = (scalar(a)?, scalar(b)?);
        let (ta, tb) = (sa.ty(), sb.ty());
        if ta != tb {
            if ta == Ty::Null || tb == Ty::Null {
                return Ok(if op == BinOp::Ne { Tri::True } else { Tri::False });
            }
            return Ok(Tri::Unknown);
        }
        let cmp = match (&sa, &sb) {
            (Sv::Bool(x), Sv::Bool(y)) => x.cmp(y),
            (Sv::NumRef(x), Sv::NumRef(y)) => x.cmp_value(y),
            (Sv::Num(x), Sv::NumRef(y)) => x.cmp_value(y),
            (Sv::NumRef(x), Sv::Num(y)) => x.cmp_value(y),
            (Sv::Num(x), Sv::Num(y)) => x.cmp_value(y),
            // Strings compare by code point, which for UTF-8 is byte order; so do byte strings.
            (Sv::Str(x), Sv::Str(y)) | (Sv::Bytes(x), Sv::Bytes(y)) => x.cmp(y),
            (Sv::Simple(x), Sv::Simple(y)) => x.cmp(y),
            (Sv::Datetime, Sv::Datetime) => match (self.datetime_of(a)?, self.datetime_of(b)?) {
                (Some(x), Some(y)) => match self.host.datetime_cmp(&x, &y, self.use_tz) {
                    Some(o) => o,
                    None => return Ok(Tri::Unknown),
                },
                _ => return Ok(Tri::Unknown),
            },
            (Sv::Container(_), _) | (_, Sv::Container(_)) => return Ok(Tri::Unknown),
            _ => Ordering::Equal,
        };
        let res = match op {
            BinOp::Eq => cmp == Ordering::Equal,
            BinOp::Ne => cmp != Ordering::Equal,
            BinOp::Lt => cmp == Ordering::Less,
            BinOp::Gt => cmp == Ordering::Greater,
            BinOp::Le => cmp != Ordering::Greater,
            BinOp::Ge => cmp != Ordering::Less,
            _ => return Err(PathError::sql(Code::Internal, "unrecognized jsonpath operation")),
        };
        Ok(if res { Tri::True } else { Tri::False })
    }

    /// The datetime value of an item: computed ones, and tags 0 and 1 of the document (None when their content is
    /// not a valid timestamp).
    fn datetime_of(&mut self, it: &JItem<'a>) -> R<Option<Datetime>> {
        let v = match it {
            JItem::Datetime(d) => return Ok(Some(**d)),
            JItem::Doc(v) => peel(*v)?,
            _ => return Ok(None),
        };
        let at = |value: i64, tz: i32| Datetime { typ: DtType::TimestampTz, value, zone: 0, typmod: -1, tz };
        Ok(match v.kind()? {
            VKind::Tag(0) => {
                let VKind::Text(t) = v.tag_content()?.kind()? else { return Ok(None) };
                let Some(tz) = rfc3339_offset(t) else { return Ok(None) };
                self.host.timestamptz_in(t).map(|value| at(value, tz))
            }
            VKind::Tag(1) => epoch_seconds(v.tag_content()?)?.map(|secs| at(self.host.epoch_timestamptz(secs), 0)),
            _ => None,
        })
    }

    /// `executeDateTimeMethod`.
    fn datetime(&mut self, m: DatetimeMethod, arg: Option<ItemId>, next: Option<ItemId>, jb: &JItem<'a>, found: Found<'_, 'a>) -> R<Res> {
        let name = match m {
            DatetimeMethod::Datetime => "datetime",
            DatetimeMethod::Date => "date",
            DatetimeMethod::Time => "time",
            DatetimeMethod::TimeTz => "time_tz",
            DatetimeMethod::Timestamp => "timestamp",
            DatetimeMethod::TimestampTz => "timestamp_tz",
        };
        let Some(s) = str_of(jb)? else {
            return self.soft(Code::DatetimeArgument, || format!("jsonpath item method .{name}() can only be applied to a string"));
        };
        let arg = match (m, arg.map(|a| &self.path.item(a).kind)) {
            (DatetimeMethod::Datetime, Some(Kind::String(t))) => DtArg::Template(t),
            (DatetimeMethod::Datetime | DatetimeMethod::Date, _) | (_, None) => DtArg::None,
            (_, Some(Kind::Numeric(n))) => match n.round_i64().and_then(|i| i32::try_from(i).ok()) {
                Some(p) => DtArg::Precision(p),
                None => return self.soft(Code::DatetimeArgument, || format!("time precision of jsonpath item method .{name}() is out of range for type integer")),
            },
            _ => return Err(PathError::sql(Code::Internal, format!("invalid jsonpath item type for .{name}() argument"))),
        };
        match self.host.datetime(m, arg, s, self.throw, self.use_tz) {
            DtOut::Ok(d) => {
                if next.is_none() && found.is_none() {
                    return Ok(Res::Ok);
                }
                self.next_item(next, JItem::Datetime(Box::new(d)), found)
            }
            DtOut::Suppressed => Ok(Res::Error),
            DtOut::Fail(code, msg) => self.soft(code, || msg),
        }
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
                        JItem::Doc(v) => match peel(*v)?.text_bytes()? {
                            Some(t) => JItem::Str(t),
                            None => jb.clone(),
                        },
                        _ => jb.clone(),
                    }
                } else if let Some(n) = &num {
                    JItem::OwnedStr(n.to_text().into_bytes().into())
                } else if let Some(b) = bool_of(jb)? {
                    JItem::Str(if b { b"true" } else { b"false" })
                } else if let Some(d) = self.datetime_of(jb)? {
                    JItem::OwnedStr(self.host.datetime_text(&d).into_bytes().into())
                } else {
                    fail!(Code::NonNumericItem, only("a boolean, string, numeric, or datetime value"))
                }
            }
            _ => return Err(PathError::sql(Code::Internal, "unexpected jsonpath item method")),
        }))
    }
}

fn starts_with(whole: &JItem, initial: &JItem) -> R<Tri> {
    Ok(match (str_of(whole)?, str_of(initial)?) {
        (Some(w), Some(i)) if w.starts_with(i) => Tri::True,
        (Some(_), Some(_)) => Tri::False,
        _ => Tri::Unknown,
    })
}

/// For RFC 3339 timestamp text (`2013-03-21T20:04:00Z`, fraction and numeric offset allowed), its offset in
/// seconds west of UTC, as PostgreSQL counts them; None for anything else.
fn rfc3339_offset(t: &str) -> Option<i32> {
    let b = t.as_bytes();
    let digits = |r: core::ops::Range<usize>| b.get(r).is_some_and(|d| d.iter().all(u8::is_ascii_digit));
    let at = |i: usize, c: &[u8]| b.get(i).is_some_and(|x| c.contains(x));
    if !(digits(0..4) && at(4, b"-") && digits(5..7) && at(7, b"-") && digits(8..10) && at(10, b"Tt ") && digits(11..13) && at(13, b":") && digits(14..16) && at(16, b":") && digits(17..19)) {
        return None;
    }
    let num = |r: core::ops::Range<usize>| b.get(r).map_or(0, |d| d.iter().fold(0u32, |a, &c| a * 10 + u32::from(c - b'0')));
    let (year, month, day) = (num(0..4), num(5..7), num(8..10));
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    // PostgreSQL has no year 0 and accepts a leap second.
    if year == 0 || !(1..=12).contains(&month) || !(1..=days).contains(&day) || num(11..13) > 23 || num(14..16) > 59 || num(17..19) > 60 {
        return None;
    }
    let mut i = 19;
    if at(i, b".") {
        i += 1;
        let start = i;
        while at(i, b"0123456789") {
            i += 1;
        }
        if i == start {
            return None;
        }
    }
    match b.get(i..)? {
        b"Z" | b"z" => Some(0),
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] if [h1, h2, m1, m2].iter().all(|d| d.is_ascii_digit()) => {
            let n = |a: &u8, b: &u8| i32::from(a - b'0') * 10 + i32::from(b - b'0');
            if n(h1, h2) > 15 || n(m1, m2) > 59 {
                return None;
            }
            let east = n(h1, h2) * 3600 + n(m1, m2) * 60;
            Some(if *sign == b'+' { -east } else { east })
        }
        _ => None,
    }
}

/// Replaces computed datetimes by their ISO 8601 text, as jsonb returns them.
pub fn datetimes_to_text<H: Host>(items: &mut Seq, host: &mut H) {
    for it in items.iter_mut() {
        if let JItem::Datetime(d) = it {
            *it = JItem::OwnedStr(host.datetime_text(d).into_bytes().into());
        }
    }
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
        JItem::OwnedStr(s) => {
            write_head(out, MAJOR_TEXT, s.len() as u64)?;
            out.write_bytes(s)
        }
        // Callers convert datetimes first (`datetimes_to_text`).
        JItem::Datetime(_) => out.write_byte(0xf6),
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
pub fn write_array(items: &Seq, out: &mut impl OutBuf, max_depth: usize) -> crate::Result<()> {
    let base = out.len();
    out.write_byte(datum::VERSION)?;
    write_head(out, MAJOR_ARRAY, items.len() as u64)?;
    for it in items.iter() {
        write_body(it, out)?;
    }
    datum::finish_index(out, base, max_depth)
}

/// A host without PostgreSQL, for tests and fuzzing: integer arithmetic only, Rust's number parsing, and
/// substring search for `like_regex`.
#[derive(Default)]
pub struct PlainHost;

impl Host for PlainHost {
    fn arith(&mut self, op: BinOp, a: &Num, b: &Num, _throw: bool) -> Option<Num> {
        let (Num::Int(a), Num::Int(b)) = (a, b) else { return None };
        let (a, b) = (i128::from(*a), i128::from(*b));
        let r = match op {
            BinOp::Add => a + b,
            BinOp::Sub => a - b,
            BinOp::Mul => a * b,
            BinOp::Div if b != 0 && a % b == 0 => a / b,
            BinOp::Mod if b != 0 => a % b,
            _ => return None,
        };
        i64::try_from(r).ok().map(Num::Int)
    }
    fn parse_float8(&mut self, s: &str) -> Option<f64> {
        s.trim().parse().ok()
    }
    fn float8_numeric(&mut self, f: f64) -> Num {
        Num::parse(&format!("{f:?}")).unwrap_or(Num::NaN)
    }
    fn parse_int(&mut self, s: &str, bits64: bool) -> Option<i64> {
        let i: i64 = s.trim().parse().ok()?;
        (bits64 || i32::try_from(i).is_ok()).then_some(i)
    }
    fn parse_bool(&mut self, s: &str) -> Option<bool> {
        match s.trim() {
            "true" | "t" => Some(true),
            "false" | "f" => Some(false),
            _ => None,
        }
    }
    fn parse_numeric(&mut self, s: &str, _typmod: i32) -> Option<Num> {
        Num::parse(s.trim()).ok()
    }
    fn numeric_typmod(&mut self, _precision: i32, _scale: i32, _throw: bool) -> Option<i32> {
        None
    }
    fn regex_match(&mut self, pattern: &[u8], _flags: u32, s: &[u8]) -> bool {
        pattern.is_empty() || s.windows(pattern.len()).any(|w| w == pattern)
    }
    fn datetime(&mut self, _m: DatetimeMethod, _arg: DtArg, _s: &[u8], _throw: bool, _use_tz: bool) -> DtOut {
        DtOut::Fail(Code::NotSupported, "no datetimes without PostgreSQL".into())
    }
    fn datetime_cmp(&mut self, a: &Datetime, b: &Datetime, _use_tz: bool) -> Option<Ordering> {
        (a.typ == b.typ).then(|| a.value.cmp(&b.value))
    }
    fn datetime_text(&mut self, d: &Datetime) -> String {
        d.value.to_string()
    }
    fn timestamptz_in(&mut self, _s: &str) -> Option<i64> {
        None
    }
    fn epoch_timestamptz(&mut self, secs: f64) -> i64 {
        secs as i64 * 1_000_000
    }
    fn check_stack(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::super::test_path::{P, compile};
    use super::*;
    use crate::ctx::PlainCtx;
    use crate::datum::Datum;
    use crate::edn;

    fn doc(text: &str) -> Vec<u8> {
        let mut d = Vec::new();
        edn::to_datum(text.as_bytes(), &mut d, &mut PlainCtx::default()).unwrap();
        d
    }

    /// Results as EDN, or the outcome when there are none.
    fn run(lax: bool, chain: &[P], text: &str, throw: bool) -> Result<Vec<String>, String> {
        let bin = compile(lax, chain);
        let path = Path::decode(&bin).unwrap();
        let d = doc(text);
        let d = Datum::parse(&d).unwrap();
        let mut found = Seq::default();
        let res = execute(&path, Value::root(&d), None, throw, false, &mut PlainHost, Some(&mut found)).map_err(|e| format!("{e:?}"))?;
        if res == Res::Error {
            return Err(format!("suppressed after {}", found.len()));
        }
        Ok(found
            .iter()
            .map(|it| {
                let mut out = Vec::new();
                write_datum(it, &mut out, 1000).unwrap();
                let mut s = Vec::new();
                edn::print(&Value::root(&Datum::parse(&out).unwrap()), &mut s, Default::default()).unwrap();
                String::from_utf8(s).unwrap()
            })
            .collect())
    }

    fn exists(lax: bool, chain: &[P], text: &str) -> Res {
        let bin = compile(lax, chain);
        let path = Path::decode(&bin).unwrap();
        let d = doc(text);
        let d = Datum::parse(&d).unwrap();
        execute(&path, Value::root(&d), None, false, false, &mut PlainHost, None).unwrap()
    }

    const GT: u8 = 11;
    const ADD: u8 = 14;
    const EQ: u8 = 8;

    #[test]
    fn filters_keep_items_whose_condition_holds() {
        let path = [P::Root, P::Key("a"), P::AnyArray, P::Filter(vec![P::Bin(GT, vec![P::Cur], vec![P::Num("1")])])];
        assert_eq!(run(true, &path, r#"{"a": [1, 2, 4([-1, 15]), "x"]}"#, true).unwrap(), ["2", "4([-1, 15])"]);
    }

    #[test]
    fn lax_mode_unwraps_arrays_and_strict_mode_errors() {
        let path = [P::Root, P::Key("a"), P::Key("b")];
        let d = r#"{"a": [{"b": 1}, {"b": 2}]}"#;
        assert_eq!(run(true, &path, d, true).unwrap(), ["1", "2"]);
        assert!(run(false, &path, d, true).unwrap_err().contains("MemberNotFound"));
        assert_eq!(run(false, &path, d, false).unwrap_err(), "suppressed after 0");
    }

    #[test]
    fn silent_mode_keeps_results_found_before_an_error() {
        let path = [P::Root, P::AnyArray, P::Key("a")];
        assert_eq!(run(false, &path, r#"[{"a": 1}, 2, {"a": 3}]"#, false).unwrap_err(), "suppressed after 1");
    }

    #[test]
    fn any_visits_every_level_depth_first() {
        let path = [P::Root, P::Any(0, u32::MAX)];
        assert_eq!(run(true, &path, r#"[[1, [2]], {"a": 3}]"#, true).unwrap(), ["[[1, [2]], {\"a\": 3}]", "[1, [2]]", "1", "[2]", "2", "{\"a\": 3}", "3"]);
        let path = [P::Root, P::Any(2, 2)];
        assert_eq!(run(true, &path, r#"[[1, [2]], {"a": 3}]"#, true).unwrap(), ["1", "[2]", "3"]);
    }

    #[test]
    fn any_descends_a_deep_document_without_recursion() {
        let n = if cfg!(miri) { 200 } else { 20_000 };
        let text = format!("{}1{}", "[".repeat(n), "]".repeat(n));
        let mut d = Vec::new();
        let mut ctx = PlainCtx::default();
        ctx.0.max_depth = n + 1;
        edn::to_datum(text.as_bytes(), &mut d, &mut ctx).unwrap();
        let bin = compile(false, &[P::Root, P::Any(0, u32::MAX)]);
        let path = Path::decode(&bin).unwrap();
        let d = Datum::parse(&d).unwrap();
        let mut found = Seq::default();
        execute(&path, Value::root(&d), None, true, false, &mut PlainHost, Some(&mut found)).unwrap();
        assert_eq!(found.len(), n + 1);
    }

    #[test]
    fn existence_checks_stop_at_the_first_match() {
        // The second element would raise in strict mode; the first match ends evaluation before it.
        let path = [P::Root, P::AnyArray, P::Filter(vec![P::Bin(EQ, vec![P::Cur, P::Key("a")], vec![P::Num("1")])])];
        assert_eq!(exists(true, &path, r#"[{"a": 1}, 2]"#), Res::Ok);
        assert_eq!(exists(true, &path, r#"[{"a": 2}]"#), Res::NotFound);
    }

    #[test]
    fn rfc3339_offsets_are_seconds_west() {
        assert_eq!(rfc3339_offset("2013-03-21T20:04:00Z"), Some(0));
        assert_eq!(rfc3339_offset("2013-03-21T20:04:00.5+01:30"), Some(-5400));
        assert_eq!(rfc3339_offset("2013-03-21 20:04:00-05:00"), Some(18000));
        assert_eq!(rfc3339_offset("2012-02-29T23:59:60Z"), Some(0));
        for bad in ["2013-03-21T20:04:00", "2013-03-21T20:04:00.Z", "2013-3-21T20:04:00Z", "2013-03-21T20:04:00+0100", "x", "2013-02-29T00:00:00Z", "2013-04-31T00:00:00Z", "0000-01-01T00:00:00Z", "2013-01-01T24:00:00Z", "2013-01-01T00:00:00+16:00"] {
            assert_eq!(rfc3339_offset(bad), None, "{bad}");
        }
    }

    #[test]
    fn arithmetic_and_methods() {
        assert_eq!(run(true, &[P::Bin(ADD, vec![P::Root, P::Key("a")], vec![P::Num("1")])], r#"{"a": 2}"#, true).unwrap(), ["3"]);
        assert_eq!(run(true, &[P::Root, P::AnyArray, P::Method(31)], r#"[1, "x", h'00', undefined, {}]"#, true).unwrap(), [r#""number""#, r#""string""#, r#""bytes""#, r#""undefined""#, r#""object""#]);
        assert_eq!(run(true, &[P::Root, P::Method(38)], r#"{"a": 1, 2: [3]}"#, true).unwrap(), [r#"{"id": 0, "key": 2, "value": [3]}"#, r#"{"id": 0, "key": "a", "value": 1}"#]);
        assert_eq!(run(true, &[P::Root, P::Method(34)], "-1.5", true).unwrap(), ["-2"]);
    }
}
