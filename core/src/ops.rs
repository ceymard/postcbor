//! Read-only operations: key lookup, paths, containment, existence (design §8).

use crate::cursor::*;
use crate::encode::head_bytes;
use crate::error::{Error, ErrorKind, Result};
use crate::value::{EncodedKey, Kind, Value, end_of};

/// Deterministic encoding of a lookup key, as its head plus borrowed content: built per operator call without
/// copying the key.
#[derive(Clone, Copy)]
pub struct Key<'k> {
    head: [u8; 9],
    head_len: usize,
    content: &'k [u8],
}

impl Key<'_> {
    /// The encoding as one buffer, for edits that keep the keys.
    pub fn to_vec(&self) -> Vec<u8> {
        let head = self.head.get(..self.head_len).unwrap_or_default();
        [head, self.content].concat()
    }
}

impl EncodedKey for Key<'_> {
    #[inline(always)]
    fn cmp_stored(&self, stored: &[u8]) -> core::cmp::Ordering {
        let head = self.head.get(..self.head_len).unwrap_or_default();
        match stored.split_at_checked(head.len()) {
            Some((sh, rest)) => {
                // Heads are at most 9 bytes: compared inline; the content only when the heads match.
                for (a, b) in sh.iter().zip(head) {
                    if a != b {
                        return a.cmp(b);
                    }
                }
                rest.cmp(self.content)
            }
            None => stored.cmp(head),
        }
    }
}

fn key_with(major: u8, content: &[u8]) -> Key<'_> {
    let mut head = [0u8; 9];
    let head_len = head_bytes(major, content.len() as u64, &mut head);
    Key { head, head_len, content }
}

/// Deterministic encoding of a text key, given as UTF-8 bytes.
pub fn key_text(s: &(impl AsRef<[u8]> + ?Sized)) -> Key<'_> {
    key_with(MAJOR_TEXT, s.as_ref())
}

/// Deterministic encoding of a byte-string key.
pub fn key_bytes(b: &[u8]) -> Key<'_> {
    key_with(MAJOR_BYTES, b)
}


/// Deterministic encoding of an integer key.
pub fn key_int(i: i64) -> Key<'static> {
    let mut head = [0u8; 9];
    let head_len = if i >= 0 {
        head_bytes(MAJOR_UNSIGNED, i as u64, &mut head)
    } else {
        head_bytes(MAJOR_NEGATIVE, (-1 - i) as u64, &mut head)
    };
    Key { head, head_len, content: &[] }
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
pub fn exists_encoded(v: &Value, key: &(impl EncodedKey + ?Sized)) -> Result<bool> {
    match v.kind()? {
        Kind::Map(_) => Ok(v.map_get(key)?.is_some()),
        Kind::Array(_) => {
            for e in v.array_iter()? {
                if key.cmp_stored(e?.bytes()?).is_eq() {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

/// Containment frames: what the parent is waiting for. Positions only (the two values are fixed for the whole check),
/// so frames are cheap to create and move.
#[derive(Clone, Copy)]
enum Frame {
    /// Every remaining entry of map `b` must exist in map `a` with a contained value. `b_key` is the current
    /// entry's key, `b_val` its value once the entry is being checked.
    Map { a: usize, b_key: usize, b_val: usize, b_left: u64 },
    /// Every remaining element of array `b` must be contained in some element of array `a`. `b_cur` is the current
    /// element, `cand` the element of `a` being tried for it.
    Array { a_first: usize, a_len: u64, b_cur: usize, b_left: u64, cand: usize, cand_left: u64 },
}

/// Outcome of starting a containment check.
enum Step {
    Decided(bool),
    /// The answer needs the entries of a container pair.
    Open(Frame),
}

/// `a @> b` with jsonb semantics; tags contain tags of the same number with contained content.
pub fn contains(a: &Value, b: &Value, ctx: &mut impl crate::ctx::Ctx) -> Result<bool> {
    // Top level only: an array contains a scalar that is one of its elements.
    if a.head()?.0.major == MAJOR_ARRAY && !matches!(b.head()?.0.major, MAJOR_ARRAY | MAJOR_MAP) {
        return exists_encoded(a, b.bytes()?);
    }
    let (da, db) = (a.d, b.d);
    let mut top = match start(*a, *b)? {
        Step::Decided(r) => return Ok(r),
        Step::Open(f) => f,
    };
    // Frames above `top`; one-level checks, the usual case, never allocate.
    let mut parents: Vec<Frame> = Vec::new();
    // Result of `top`'s current child check; None when none is pending.
    let mut result: Option<bool> = None;
    // O(n·m) on arrays: stays cancellable.
    let mut steps: u32 = 0;
    loop {
        steps = steps.wrapping_add(1);
        if steps.is_multiple_of(crate::ctx::INTERRUPT_EVERY) {
            ctx.check_interrupts();
        }
        let mut outcome: Option<bool> = None;
        let mut child: Option<(usize, usize)> = None;
        match &mut top {
            Frame::Map { a, b_key, b_val, b_left } => {
                match result {
                    Some(false) => outcome = Some(false),
                    Some(true) => {
                        // The end of the last entry is never needed: on deep values it costs a walk per level.
                        *b_left -= 1;
                        if *b_left > 0 {
                            *b_key = end_of(db, *b_val)?;
                        }
                    }
                    None => {}
                }
                if outcome.is_none() {
                    if *b_left == 0 {
                        outcome = Some(true);
                    } else {
                        let k = Value { d: db, pos: *b_key };
                        *b_val = k.end()?;
                        match (Value { d: da, pos: *a }).map_get(k.bytes()?)? {
                            None => outcome = Some(false),
                            Some(av) => child = Some((av.pos, *b_val)),
                        }
                    }
                }
            }
            Frame::Array { a_first, a_len, b_cur, b_left, cand, cand_left } => {
                match result {
                    Some(true) => {
                        *b_left -= 1;
                        if *b_left > 0 {
                            *b_cur = end_of(db, *b_cur)?;
                            (*cand, *cand_left) = (*a_first, *a_len);
                        }
                    }
                    Some(false) => {
                        *cand_left -= 1;
                        if *cand_left > 0 {
                            *cand = end_of(da, *cand)?;
                        }
                    }
                    None => {}
                }
                if *b_left == 0 {
                    outcome = Some(true);
                } else if *cand_left == 0 {
                    outcome = Some(false);
                } else {
                    child = Some((*cand, *b_cur));
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
        } else if let Some((pa, pb)) = child {
            match start(Value { d: da, pos: pa }, Value { d: db, pos: pb })? {
                Step::Decided(r) => result = Some(r),
                Step::Open(f) => parents.push(core::mem::replace(&mut top, f)),
            }
        }
    }
}

/// Starts checking that `a` contains `b`: decides scalars directly, opens a frame for containers.
#[inline]
fn start(mut a: Value, mut b: Value) -> Result<Step> {
    loop {
        let ((ha, fa), (hb, fb)) = (a.head()?, b.head()?);
        return Ok(match (ha.major, hb.major) {
            (MAJOR_MAP, MAJOR_MAP) => Step::Open(Frame::Map { a: a.pos, b_key: fb, b_val: fb, b_left: hb.arg }),
            (MAJOR_ARRAY, MAJOR_ARRAY) => {
                Step::Open(Frame::Array { a_first: fa, a_len: ha.arg, b_cur: fb, b_left: hb.arg, cand: fa, cand_left: ha.arg })
            }
            (MAJOR_TAG, MAJOR_TAG) if ha.arg == hb.arg => {
                (a.pos, b.pos) = (fa, fb);
                continue;
            }
            (MAJOR_MAP | MAJOR_ARRAY | MAJOR_TAG, _) | (_, MAJOR_MAP | MAJOR_ARRAY | MAJOR_TAG) => Step::Decided(false),
            _ => Step::Decided(a.bytes()? == b.bytes()?),
        });
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
        f(&Value::root(&Datum::parse(&sa).unwrap()), &Value::root(&Datum::parse(&sb).unwrap()))
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
        let v_d = Datum::parse(&sa).unwrap();
        let v = Value::root(&v_d);
        assert!(contains(&v, &v, &mut PlainCtx::default()).unwrap());
    }

    #[test]
    fn built_keys_order_and_find_like_their_encoding() {
        let mut texts: Vec<String> = [0, 1, 5, 22, 23, 24, 25, 255, 256].iter().map(|&n| "k".repeat(n)).collect();
        // Same length, different content: decided past the head, for 1-byte and 2-byte heads.
        texts.extend(["a", "j", "z", "ab", "ba"].map(String::from));
        texts.extend(["a".repeat(24), "z".repeat(24)]);
        let mut keys: Vec<Key> = texts.iter().map(key_text).collect();
        keys.extend([key_bytes(b""), key_bytes(b"k"), key_int(0), key_int(23), key_int(24), key_int(-1), key_int(i64::MIN)]);
        let encoded: Vec<Vec<u8>> = keys.iter().map(Key::to_vec).collect();
        // Includes stored keys shorter than the built key's head, and heads of different lengths.
        for k in &keys {
            for e in &encoded {
                assert_eq!(k.cmp_stored(e), e.as_slice().cmp(&k.to_vec()), "{:x?} vs {e:x?}", k.to_vec());
            }
        }
        // Maps holding the keys: a small one without an index section, and an indexed one.
        let mut items: Vec<String> = texts.iter().map(|t| format!("\"{t}\": 1")).collect();
        items.extend(["h'': 1", "h'6b': 1", "0: 1", "23: 1", "24: 1", "-1: 1", "-9223372036854775808: 1"].map(String::from));
        items.extend((0..20).map(|i| format!("\"z{i}\": 2")));
        for n in [8, items.len()] {
            let mut d = Vec::new();
            edn::to_datum(format!("{{{}}}", items[..n].join(", ")).as_bytes(), &mut d, &mut PlainCtx::default()).unwrap();
            let pd = Datum::parse(&d).unwrap();
            assert_eq!(pd.index.is_some(), n > 8);
            let v = Value::root(&pd);
            for k in keys.iter().take(n.min(keys.len())) {
                assert!(v.map_get(k).unwrap().is_some(), "{:x?}", k.to_vec());
            }
            assert!(v.map_get(&key_text("kk")).unwrap().is_none());
            assert!(v.map_get(&key_int(1)).unwrap().is_none());
        }
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
