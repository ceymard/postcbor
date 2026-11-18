//! Read-only operations: key lookup, paths, containment, existence (design §8).

use crate::cursor::*;
use crate::encode::head_bytes;
use crate::error::{Error, ErrorKind, Result};
use crate::value::{ArrayIter, Kind, MapIter, Value};

/// Inline capacity of [`Key`]: covers the keys of nearly every real map without a heap allocation.
const KEY_INLINE: usize = 40;

/// Deterministic encoding of a lookup key, built per operator call: inline when short, so `doc -> 'name'`
/// allocates nothing.
pub struct Key {
    inline: [u8; KEY_INLINE],
    len: usize,
    heap: Vec<u8>,
}

impl core::ops::Deref for Key {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        if self.heap.is_empty() { &self.inline[..self.len] } else { &self.heap }
    }
}

impl Key {
    fn new(head: &[u8], content: &[u8]) -> Key {
        let len = head.len() + content.len();
        let mut k = Key { inline: [0; KEY_INLINE], len, heap: Vec::new() };
        if len <= KEY_INLINE {
            k.inline[..head.len()].copy_from_slice(head);
            k.inline[head.len()..len].copy_from_slice(content);
        } else {
            k.heap.reserve_exact(len);
            k.heap.extend_from_slice(head);
            k.heap.extend_from_slice(content);
        }
        k
    }
}

/// Deterministic encoding of a text key, given as UTF-8 bytes.
pub fn key_text(s: &(impl AsRef<[u8]> + ?Sized)) -> Key {
    key_with(MAJOR_TEXT, s.as_ref())
}

/// Deterministic encoding of a byte-string key.
pub fn key_bytes(b: &[u8]) -> Key {
    key_with(MAJOR_BYTES, b)
}

fn key_with(major: u8, content: &[u8]) -> Key {
    let mut h = [0u8; 9];
    let n = head_bytes(major, content.len() as u64, &mut h);
    Key::new(&h[..n], content)
}

/// Deterministic encoding of an integer key.
pub fn key_int(i: i64) -> Key {
    let mut h = [0u8; 9];
    let n = if i >= 0 {
        head_bytes(MAJOR_UNSIGNED, i as u64, &mut h)
    } else {
        head_bytes(MAJOR_NEGATIVE, (-1 - i) as u64, &mut h)
    };
    Key::new(&h[..n], &[])
}

/// `-> int`: array index (negative counts from the end) or integer map key.
pub fn get_int<'a>(v: &Value<'a>, i: i64) -> Result<Option<Value<'a>>> {
    match v.kind()? {
        Kind::Array(_) => v.array_get(i),
        Kind::Map(_) => v.map_get(&key_int(i)),
        _ => Ok(None),
    }
}

/// Key of any type: an integer indexes arrays; any key looks up maps.
pub fn get_key<'a>(v: &Value<'a>, key: &Value) -> Result<Option<Value<'a>>> {
    match v.kind()? {
        Kind::Array(_) => match as_index(key)? {
            Some(i) => v.array_get(i),
            None => Ok(None),
        },
        Kind::Map(_) => v.map_get(key.bytes()?),
        _ => Ok(None),
    }
}

/// An integer key usable as an array index.
fn as_index(key: &Value) -> Result<Option<i64>> {
    Ok(match key.kind()? {
        Kind::Unsigned(n) => i64::try_from(n).ok(),
        Kind::Negative(n) => i64::try_from(n).ok().map(|n| -1 - n),
        _ => None,
    })
}

/// `#> text[]`: text keys on maps, integer steps on arrays. A NULL step or a non-integer step on an array gives None.
pub fn get_text_path<'a>(v: &Value<'a>, path: &[Option<&str>]) -> Result<Option<Value<'a>>> {
    let mut cur = *v;
    for step in path {
        let Some(step) = step else { return Ok(None) };
        let next = match cur.kind()? {
            Kind::Map(_) => cur.map_get(&key_text(step))?,
            Kind::Array(_) => match parse_index(step) {
                Some(i) => cur.array_get(i)?,
                None => None,
            },
            _ => None,
        };
        match next {
            Some(n) => cur = n,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

/// Integer path step, as jsonb accepts it (optional sign, decimal digits, surrounding spaces).
pub fn parse_index(s: &str) -> Option<i64> {
    s.trim().parse::<i64>().ok()
}

/// Typed path: `path` is an array whose elements are keys of any type.
pub fn get_typed_path<'a>(v: &Value<'a>, path: &Value) -> Result<Option<Value<'a>>> {
    if !matches!(path.kind()?, Kind::Array(_)) {
        return Err(Error::new(ErrorKind::Syntax("path must be a cbor array"), crate::error::NO_OFFSET));
    }
    let mut cur = *v;
    for step in path.array_iter()? {
        match get_key(&cur, &step?)? {
            Some(n) => cur = n,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

/// `? text`: the map has this text key, or the array has this text string as an element.
pub fn exists_text(v: &Value, s: &(impl AsRef<[u8]> + ?Sized)) -> Result<bool> {
    exists_encoded(v, &key_text(s))
}

/// Key or element equal to the given deterministic encoding.
pub fn exists_encoded(v: &Value, key: &[u8]) -> Result<bool> {
    match v.kind()? {
        Kind::Map(_) => Ok(v.map_get(key)?.is_some()),
        Kind::Array(_) => {
            for e in v.array_iter()? {
                if e?.bytes()? == key {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

/// Containment frames: what the parent is waiting for.
enum Frame<'a> {
    /// Every entry of `b` must exist in `a` with a contained value.
    Map { a: Value<'a>, b: MapIter<'a> },
    /// Every element of `b` must be contained in some element of `a`.
    Array { a: Value<'a>, b: ArrayIter<'a>, cur: Option<Value<'a>>, cands: ArrayIter<'a> },
}

/// Outcome of starting a containment check.
// Boxing the frame would allocate for every container pair.
#[allow(clippy::large_enum_variant)]
enum Step<'a> {
    Decided(bool),
    /// The answer needs the entries of a container pair.
    Open(Frame<'a>),
}

/// `a @> b` with jsonb semantics; tags contain tags of the same number with contained content.
pub fn contains(a: &Value, b: &Value, ctx: &mut impl crate::ctx::Ctx) -> Result<bool> {
    // Top level only: an array contains a scalar that is one of its elements.
    if a.head()?.0.major == MAJOR_ARRAY && !matches!(b.head()?.0.major, MAJOR_ARRAY | MAJOR_MAP) {
        return exists_encoded(a, b.bytes()?);
    }
    let mut top = match start(a, b)? {
        Step::Decided(r) => return Ok(r),
        Step::Open(f) => f,
    };
    // Frames above `top`; one-level checks, the usual case, never allocate.
    let mut parents: Vec<Frame> = Vec::new();
    // Result of `top`'s last child check; None right after it was opened.
    let mut result: Option<bool> = None;
    // O(n·m) on arrays: stays cancellable.
    let mut steps: u32 = 0;
    loop {
        steps = steps.wrapping_add(1);
        if steps.is_multiple_of(crate::ctx::INTERRUPT_EVERY) {
            ctx.check_interrupts();
        }
        let mut outcome: Option<bool> = None;
        let mut child: Option<(Value, Value)> = None;
        match &mut top {
            Frame::Map { a, b } => {
                if result == Some(false) {
                    outcome = Some(false);
                } else {
                    match b.next() {
                        None => outcome = Some(true),
                        Some(e) => {
                            let (k, bv) = e?;
                            match a.map_get(k.bytes()?)? {
                                None => outcome = Some(false),
                                Some(av) => child = Some((av, bv)),
                            }
                        }
                    }
                }
            }
            Frame::Array { a, b, cur, cands } => {
                if result == Some(true) {
                    *cur = None;
                }
                if cur.is_none() {
                    match b.next() {
                        None => outcome = Some(true),
                        Some(e) => {
                            *cur = Some(e?);
                            *cands = a.array_iter()?;
                        }
                    }
                }
                if let (None, Some(be)) = (outcome, *cur) {
                    match cands.next() {
                        None => outcome = Some(false),
                        Some(c) => child = Some((c?, be)),
                    }
                }
            }
        }
        result = None;
        if let Some(o) = outcome {
            match parents.pop() {
                None => return Ok(o),
                Some(p) => top = p,
            }
            result = Some(o);
        } else if let Some((ca, cb)) = child {
            match start(&ca, &cb)? {
                Step::Decided(r) => result = Some(r),
                Step::Open(f) => parents.push(core::mem::replace(&mut top, f)),
            }
        }
    }
}

/// Starts checking that `a` contains `b`: decides scalars directly, opens a frame for containers.
fn start<'a>(a: &Value<'a>, b: &Value<'a>) -> Result<Step<'a>> {
    let (mut a, mut b) = (*a, *b);
    loop {
        let ((ha, _), (hb, _)) = (a.head()?, b.head()?);
        return match (ha.major, hb.major) {
            (MAJOR_MAP, MAJOR_MAP) => Ok(Step::Open(Frame::Map { a, b: b.map_iter()? })),
            (MAJOR_ARRAY, MAJOR_ARRAY) => {
                Ok(Step::Open(Frame::Array { a, b: b.array_iter()?, cur: None, cands: a.array_iter()? }))
            }
            (MAJOR_TAG, MAJOR_TAG) if ha.arg == hb.arg => {
                a = a.tag_content()?;
                b = b.tag_content()?;
                continue;
            }
            (MAJOR_MAP | MAJOR_ARRAY | MAJOR_TAG, _) | (_, MAJOR_MAP | MAJOR_ARRAY | MAJOR_TAG) => Ok(Step::Decided(false)),
            _ => Ok(Step::Decided(a.bytes()? == b.bytes()?)),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::PlainCtx;
    use crate::datum::Datum;
    use crate::edn;

    fn st(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        edn::to_datum(text.as_bytes(), &mut out, &mut PlainCtx::default()).unwrap();
        out
    }

    fn with<R>(a: &str, b: &str, f: impl FnOnce(&Value, &Value) -> R) -> R {
        let (sa, sb) = (st(a), st(b));
        f(&Value::root(Datum::parse(&sa).unwrap()), &Value::root(Datum::parse(&sb).unwrap()))
    }

    fn c(a: &str, b: &str) -> bool {
        with(a, b, |x, y| contains(x, y, &mut PlainCtx::default()).unwrap())
    }

    #[test]
    fn containment_follows_jsonb_rules() {
        assert!(c(r#"{"a": 1, "b": [1, 2, {"c": 3}]}"#, r#"{"b": [{"c": 3}]}"#));
        assert!(c(r#"{"a": 1, "b": 2}"#, r#"{}"#));
        assert!(!c(r#"{"a": 1}"#, r#"{"a": 1, "b": 2}"#));
        assert!(c("[1, 2, 3]", "[3, 1, 1]"));
        assert!(c("[1, [2, 3]]", "[[3]]"));
        assert!(!c("[1, [2, 3]]", "[3]"));
        assert!(c("[1, 2]", "1"));
        assert!(!c("[[1, 2]]", "[1]"));
        assert!(!c("1", "1.0"));
        assert!(c("[1(5), 2]", "[1(5)]"));
        assert!(c(r#"1({"a": 1, "b": 2})"#, r#"1({"a": 1})"#));
        assert!(!c(r#"1({"a": 1})"#, r#"7({"a": 1})"#));
        // Backtracking: the first candidate that matches "x" fails on "y".
        assert!(c(r#"[{"x": 1}, {"x": 1, "y": 2}]"#, r#"[{"x": 1, "y": 2}]"#));
        assert!(!c(r#"[{"x": 1}, {"y": 2}]"#, r#"[{"x": 1, "y": 2}]"#));
        assert!(c("{1: [h'ff']}", "{1: [h'ff']}"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "too slow under Miri")]
    fn deep_containment_does_not_recurse() {
        let depth = 100_000;
        let a = "[".repeat(depth) + &"]".repeat(depth);
        let mut ctx = PlainCtx(crate::Limits { max_depth: depth + 1, ..Default::default() });
        let mut sa = Vec::new();
        edn::to_datum(a.as_bytes(), &mut sa, &mut ctx).unwrap();
        let v = Value::root(Datum::parse(&sa).unwrap());
        assert!(contains(&v, &v, &mut PlainCtx::default()).unwrap());
    }

    #[test]
    fn paths_and_keys() {
        with(r#"{"a": [10, {"b": h'01'}], 1: "one", h'ff': 2}"#, "[\"a\", 1, \"b\"]", |v, p| {
            assert_eq!(get_typed_path(v, p).unwrap().unwrap().kind().unwrap(), Kind::Bytes(&[1]));
            assert_eq!(get_int(v, 1).unwrap().unwrap().kind().unwrap(), Kind::Text("one"));
            assert_eq!(v.map_get(&key_bytes(&[0xff])).unwrap().unwrap().kind().unwrap(), Kind::Unsigned(2));
            let tp = [Some("a"), Some("-1"), Some("b")];
            assert_eq!(get_text_path(v, &tp).unwrap().unwrap().kind().unwrap(), Kind::Bytes(&[1]));
            assert!(get_text_path(v, &[Some("a"), Some("x")]).unwrap().is_none());
            assert!(get_text_path(v, &[Some("a"), None]).unwrap().is_none());
            assert!(exists_text(v, "a").unwrap());
            assert!(!exists_text(v, "one").unwrap());
        });
        with(r#"["a", 1, "b"]"#, "0", |v, _| {
            assert!(exists_text(v, "b").unwrap());
            assert!(!exists_text(v, "1").unwrap());
        });
    }
}
