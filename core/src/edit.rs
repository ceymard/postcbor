//! Operations that build a new value: concatenation, deletion, set/insert, strip_nulls (design §8).
//! Sub-values are already deterministic, so results are assembled from copied body slices.

use core::cmp::Ordering;

use crate::ctx::Limits;
use crate::cursor::*;
use crate::datum::{self, Datum, VERSION};
use crate::encode::{insert_head, write_head};
use crate::error::{Error, ErrorKind, NO_OFFSET, Result};
use crate::ops::{key_int, key_text, parse_index};
use crate::out::{OutBuf, try_push};
use crate::value::{Kind, Value};

fn invalid(msg: &'static str) -> Error {
    Error::new(ErrorKind::InvalidArgument(msg), NO_OFFSET)
}

fn corrupt(at: usize) -> Error {
    Error::new(ErrorKind::CorruptDatum, at)
}

fn slice(body: &[u8], s: usize, e: usize) -> Result<&[u8]> {
    body.get(s..e).ok_or(corrupt(s))
}

/// Writes a datum whose body is `parts` concatenated, then builds its index.
fn emit(parts: &[&[u8]], out: &mut impl OutBuf, md: usize) -> Result<()> {
    let base = out.len();
    out.write_byte(VERSION)?;
    for p in parts {
        out.write_bytes(p)?;
    }
    datum::finish_index(out, base, md)
}

/// Copies `v` as a datum of its own.
pub fn copy_value(v: &Value, out: &mut impl OutBuf) -> Result<()> {
    datum::write_subvalue(&v.d, v.pos, v.end()?, out)
}

/// A change inside one container: body range `[s, e)` replaced by `new`, the container's entry count moved by `delta`.
#[derive(Clone, Copy)]
struct Change<'n> {
    s: usize,
    e: usize,
    new: &'n [u8],
    delta: i64,
}

/// New datum: `d`'s body with `change` applied inside the container at `inner`.
fn splice(d: &Datum, inner: Option<usize>, change: Change, out: &mut impl OutBuf, md: usize) -> Result<()> {
    let Change { s, e, new, delta } = change;
    let body = d.body;
    let Some(c) = inner else {
        return emit(&[slice(body, 0, s)?, new, slice(body, e, body.len())?], out, md);
    };
    let mut cur = Cursor::at(body, c);
    let h = cur.head().map_err(|e| corrupt(e.offset))?;
    let n = h.arg.checked_add_signed(delta).ok_or(corrupt(c))?;
    let mut head = [0u8; 9];
    let hn = crate::encode::head_bytes(h.major, n, &mut head);
    emit(
        &[slice(body, 0, c)?, &head[..hn], slice(body, cur.pos(), s)?, new, slice(body, e, body.len())?],
        out,
        md,
    )
}

/// Elements of `v` as body slices: an array's elements, or `v` itself.
fn elements<'a>(v: &Value<'a>) -> Result<(u64, &'a [u8])> {
    match v.kind()? {
        Kind::Array(n) => {
            let first = v.head()?.1;
            Ok((n, slice(v.d.body, first, v.end()?)?))
        }
        _ => Ok((1, v.bytes()?)),
    }
}

/// `a || b`: maps merge (right wins); anything else concatenates as arrays.
pub fn concat(a: &Value, b: &Value, out: &mut impl OutBuf, limits: &Limits) -> Result<()> {
    let md = limits.max_depth;
    if let (Kind::Map(_), Kind::Map(_)) = (a.kind()?, b.kind()?) {
        return merge_maps(a, b, out, md);
    }
    let (na, ea) = elements(a)?;
    let (nb, eb) = elements(b)?;
    let mut head = Vec::new();
    write_head(&mut head, MAJOR_ARRAY, na + nb)?;
    emit(&[&head, ea, eb], out, md)
}

fn merge_maps(a: &Value, b: &Value, out: &mut impl OutBuf, md: usize) -> Result<()> {
    // Entries as (key bytes, whole entry bytes).
    let entries = |v: &Value<'_>| -> Result<Vec<(usize, usize, usize)>> {
        let mut r = Vec::new();
        for e in v.map_iter()? {
            let (k, val) = e?;
            try_push(&mut r, (k.pos, val.pos, val.end()?))?;
        }
        Ok(r)
    };
    let (ea, eb) = (entries(a)?, entries(b)?);
    let (ba, bb) = (a.d.body, b.d.body);
    let mut merged: Vec<&[u8]> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < ea.len() || j < eb.len() {
        let ord = match (ea.get(i), eb.get(j)) {
            (Some(x), Some(y)) => slice(ba, x.0, x.1)?.cmp(slice(bb, y.0, y.1)?),
            (Some(_), None) => Ordering::Less,
            _ => Ordering::Greater,
        };
        match ord {
            Ordering::Less => {
                merged.push(slice(ba, ea[i].0, ea[i].2)?);
                i += 1;
            }
            Ordering::Greater => {
                merged.push(slice(bb, eb[j].0, eb[j].2)?);
                j += 1;
            }
            Ordering::Equal => {
                merged.push(slice(bb, eb[j].0, eb[j].2)?);
                i += 1;
                j += 1;
            }
        }
    }
    let mut head = Vec::new();
    write_head(&mut head, MAJOR_MAP, merged.len() as u64)?;
    merged.insert(0, &head);
    emit(&merged, out, md)
}

/// Removes map entries whose key is in `keys`, or array elements equal to one of them.
/// Scalars cannot be deleted from.
pub fn delete_matching(v: &Value, keys: &[Vec<u8>], out: &mut impl OutBuf, limits: &Limits) -> Result<()> {
    let md = limits.max_depth;
    let body = v.d.body;
    let mut kept: Vec<&[u8]> = Vec::new();
    let major = match v.kind()? {
        Kind::Map(_) => {
            for e in v.map_iter()? {
                let (k, val) = e?;
                let kb = k.bytes()?;
                if !keys.iter().any(|x| x.as_slice() == kb) {
                    try_push(&mut kept, slice(body, k.pos, val.end()?)?)?;
                }
            }
            MAJOR_MAP
        }
        Kind::Array(_) => {
            for e in v.array_iter()? {
                let eb = e?.bytes()?;
                if !keys.iter().any(|x| x.as_slice() == eb) {
                    try_push(&mut kept, eb)?;
                }
            }
            MAJOR_ARRAY
        }
        _ => return Err(invalid("cannot delete from scalar")),
    };
    let mut head = Vec::new();
    write_head(&mut head, major, kept.len() as u64)?;
    kept.insert(0, &head);
    emit(&kept, out, md)
}

/// `- int`: array element at an index (negative from the end), or an integer map key.
pub fn delete_index(v: &Value, i: i64, out: &mut impl OutBuf, limits: &Limits) -> Result<()> {
    let md = limits.max_depth;
    match v.kind()? {
        Kind::Array(_) => match v.array_get(i)? {
            Some(e) => splice(&v.d, Some(v.pos), Change { s: e.pos, e: e.end()?, new: &[], delta: -1 }, out, md),
            None => copy_value(v, out),
        },
        Kind::Map(_) => delete_matching(v, &[key_int(i).to_vec()], out, limits),
        _ => Err(invalid("cannot delete from scalar")),
    }
}

/// One path step: a text step (jsonb rules) or a key of any type.
#[derive(Clone, Copy)]
pub enum Step<'p> {
    Text(&'p str),
    Typed(Value<'p>),
    /// Integer subscript: array index or integer map key.
    Int(i64),
    /// Byte-string map key.
    Bytes(&'p [u8]),
}

/// Where a step points inside a container.
enum Target {
    Key(Vec<u8>),
    Index(i64),
}

/// How `step` addresses `container`. None when it cannot address it (scalar, or non-integer on an array).
fn target(container: &Value, step: Step, strict: bool, pos: usize) -> Result<Option<Target>> {
    Ok(match (container.kind()?, step) {
        (Kind::Map(_), Step::Text(s)) => Some(Target::Key(key_text(s).to_vec())),
        (Kind::Map(_), Step::Typed(k)) => Some(Target::Key(k.bytes()?.to_vec())),
        (Kind::Map(_), Step::Int(i)) => Some(Target::Key(key_int(i).to_vec())),
        (Kind::Map(_), Step::Bytes(b)) => Some(Target::Key(crate::ops::key_bytes(b).to_vec())),
        (Kind::Array(_), Step::Int(i)) => Some(Target::Index(i)),
        (Kind::Array(_), Step::Bytes(_)) if strict => return Err(invalid("path element is not an integer")),
        (Kind::Array(_), Step::Text(s)) => match parse_index(s) {
            Some(i) => Some(Target::Index(i)),
            None if strict => {
                let _ = pos;
                return Err(invalid("path element is not an integer"));
            }
            None => None,
        },
        (Kind::Array(_), Step::Typed(k)) => match k.kind()? {
            Kind::Unsigned(n) => i64::try_from(n).ok().map(Target::Index),
            Kind::Negative(n) => i64::try_from(n).ok().map(|n| Target::Index(-1 - n)),
            _ if strict => return Err(invalid("path element is not an integer")),
            _ => None,
        },
        _ => None,
    })
}

fn child<'a>(c: &Value<'a>, t: &Target) -> Result<Option<Value<'a>>> {
    match t {
        Target::Key(k) => c.map_get(k),
        Target::Index(i) => c.array_get(*i),
    }
}

/// Follows `steps`. None when some step is missing; with `strict`, a non-integer step on an array is an error.
pub fn walk<'a>(v: &Value<'a>, steps: &[Step], strict: bool) -> Result<Option<Value<'a>>> {
    let mut cur = *v;
    for (i, s) in steps.iter().enumerate() {
        let Some(t) = target(&cur, *s, strict, i)? else { return Ok(None) };
        match child(&cur, &t)? {
            Some(n) => cur = n,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

/// `#-`: deletes the entry at `path`; unchanged when the path does not exist.
pub fn delete_path(v: &Value, path: &[Step], out: &mut impl OutBuf, limits: &Limits) -> Result<()> {
    let md = limits.max_depth;
    if !matches!(v.kind()?, Kind::Array(_) | Kind::Map(_)) {
        return Err(invalid("cannot delete path in scalar"));
    }
    let Some((last, init)) = path.split_last() else { return copy_value(v, out) };
    let Some(parent) = walk(v, init, false)? else { return copy_value(v, out) };
    let Some(t) = target(&parent, *last, false, init.len())? else { return copy_value(v, out) };
    match (&t, child(&parent, &t)?) {
        (_, None) => copy_value(v, out),
        (Target::Key(k), Some(val)) => {
            let key_start = val.pos - k.len();
            splice_rooted(v, &parent, Change { s: key_start, e: val.end()?, new: &[], delta: -1 }, out, md)
        }
        (Target::Index(_), Some(e)) => splice_rooted(v, &parent, Change { s: e.pos, e: e.end()?, new: &[], delta: -1 }, out, md),
    }
}

/// `splice` for a value that may itself be a sub-value of a larger datum: only `v`'s bytes are kept.
fn splice_rooted(v: &Value, inner: &Value, change: Change, out: &mut impl OutBuf, md: usize) -> Result<()> {
    let Change { s, e, new, delta } = change;
    if v.pos == 0 && v.end()? == v.d.body.len() {
        return splice(&v.d, Some(inner.pos), change, out, md);
    }
    // Rebase onto a standalone copy of `v`.
    let mut copy = Vec::new();
    copy_value(v, &mut copy)?;
    let d = Datum::parse(&copy)?;
    let shift = v.pos;
    splice(&d, Some(inner.pos - shift), Change { s: s - shift, e: e - shift, new, delta }, out, md)
}

/// How `set` behaves at the last step.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SetMode {
    /// `cbor_set`: replace, or add when missing and `create` is true.
    Set { create: bool },
    /// `cbor_insert`: add before (or after) an array position; error on an existing map key.
    Insert { after: bool },
}

/// `cbor_set` / `cbor_insert`: `new` is the body of the value to place at `path`.
pub fn set_path(v: &Value, path: &[Step], new: &[u8], mode: SetMode, out: &mut impl OutBuf, limits: &Limits) -> Result<()> {
    let md = limits.max_depth;
    if !matches!(v.kind()?, Kind::Array(_) | Kind::Map(_)) {
        return Err(invalid("cannot set path in scalar"));
    }
    let Some((last, init)) = path.split_last() else { return copy_value(v, out) };
    let Some(parent) = walk(v, init, true)? else { return copy_value(v, out) };
    let Some(t) = target(&parent, *last, true, init.len())? else { return copy_value(v, out) };
    match t {
        Target::Key(k) => match parent.map_get(&k)? {
            Some(old) => {
                if let SetMode::Insert { .. } = mode {
                    return Err(invalid("cannot replace existing key"));
                }
                splice_rooted(v, &parent, Change { s: old.pos, e: old.end()?, new, delta: 0 }, out, md)
            }
            None => {
                if mode == (SetMode::Set { create: false }) {
                    return copy_value(v, out);
                }
                // Insert the entry before the first larger key.
                let mut at = parent.end()?;
                for e in parent.map_iter()? {
                    let (ek, _) = e?;
                    if ek.bytes()? > k.as_slice() {
                        at = ek.pos;
                        break;
                    }
                }
                let mut entry = Vec::with_capacity(k.len() + new.len());
                entry.extend_from_slice(&k);
                entry.extend_from_slice(new);
                splice_rooted(v, &parent, Change { s: at, e: at, new: &entry, delta: 1 }, out, md)
            }
        },
        Target::Index(i) => {
            let Kind::Array(n) = parent.kind()? else { return copy_value(v, out) };
            let n = n as i64;
            let idx = if i < 0 { i + n } else { i };
            match mode {
                SetMode::Set { create } => {
                    if (0..n).contains(&idx) {
                        let e = parent.array_get(idx)?.ok_or(corrupt(parent.pos))?;
                        return splice_rooted(v, &parent, Change { s: e.pos, e: e.end()?, new, delta: 0 }, out, md);
                    }
                    if !create {
                        return copy_value(v, out);
                    }
                    // Past the end appends; before the start prepends.
                    let at = if idx >= n { parent.end()? } else { parent.head()?.1 };
                    splice_rooted(v, &parent, Change { s: at, e: at, new, delta: 1 }, out, md)
                }
                SetMode::Insert { after } => {
                    let idx = idx + i64::from(after);
                    let at = if idx <= 0 {
                        parent.head()?.1
                    } else if idx >= n {
                        parent.end()?
                    } else {
                        parent.array_get(idx)?.ok_or(corrupt(parent.pos))?.pos
                    };
                    splice_rooted(v, &parent, Change { s: at, e: at, new, delta: 1 }, out, md)
                }
            }
        }
    }
}

/// Whether a step creates an array (integer-like) or a map when the path is missing.
fn creates_array(step: Step) -> Result<bool> {
    Ok(match step {
        Step::Int(_) => true,
        Step::Text(_) | Step::Bytes(_) => false,
        Step::Typed(k) => matches!(k.kind()?, Kind::Unsigned(_) | Kind::Negative(_)),
    })
}

/// Index of an array-creating step.
fn step_index(step: Step) -> Result<i64> {
    match step {
        Step::Int(i) => Ok(i),
        Step::Typed(k) => match k.kind()? {
            Kind::Unsigned(n) => i64::try_from(n).map_err(|_| invalid("array index out of range")),
            Kind::Negative(_) => Err(invalid("array index out of range")),
            _ => Err(invalid("path element is not an integer")),
        },
        _ => Err(invalid("path element is not an integer")),
    }
}

/// The key bytes of a map-creating step.
fn step_key(step: Step) -> Result<Vec<u8>> {
    Ok(match step {
        Step::Text(s) => key_text(s).to_vec(),
        Step::Bytes(b) => crate::ops::key_bytes(b).to_vec(),
        Step::Typed(k) => k.bytes()?.to_vec(),
        Step::Int(i) => key_int(i).to_vec(),
    })
}

/// Body of `new` nested under the missing `steps`: maps for key steps, null-padded arrays for index steps.
fn nest(steps: &[Step], new: &[u8]) -> Result<Vec<u8>> {
    let mut cur = new.to_vec();
    for &s in steps.iter().rev() {
        let mut next = Vec::new();
        if creates_array(s)? {
            let i = step_index(s)?;
            if i < 0 {
                return Err(invalid("array index out of range"));
            }
            write_head(&mut next, MAJOR_ARRAY, i as u64 + 1)?;
            next.extend(core::iter::repeat_n(0xf6, i as usize));
        } else {
            next.write_byte(0xa1)?;
            next.extend_from_slice(&step_key(s)?);
        }
        next.extend_from_slice(&cur);
        cur = next;
    }
    Ok(cur)
}

/// Subscript assignment (`doc['a'][2] = v`): sets `new` at `path`, creating missing maps and arrays and
/// padding arrays with nulls, as jsonb subscripting does. `v` None is a NULL column, treated as empty.
pub fn assign(v: Option<&Value>, path: &[Step], new: &[u8], out: &mut impl OutBuf, limits: &Limits) -> Result<()> {
    let md = limits.max_depth;
    let Some(v) = v else {
        // Build from nothing: the first step decides the root container.
        return emit(&[&nest(path, new)?], out, md);
    };
    let Some((_, init)) = path.split_last() else { return emit(&[new], out, md) };
    // Find the deepest existing container along the path.
    let mut cur = *v;
    let mut depth = 0;
    for s in init {
        let t = target(&cur, *s, true, depth)?;
        match t.map(|t| child(&cur, &t)).transpose()?.flatten() {
            Some(n) if matches!(n.kind()?, Kind::Map(_) | Kind::Array(_)) => {
                cur = n;
                depth += 1;
            }
            Some(_) => return Err(invalid("cannot replace a scalar inside the path")),
            None => break,
        }
    }
    if !matches!(cur.kind()?, Kind::Map(_) | Kind::Array(_)) {
        return Err(invalid("cannot set path in scalar"));
    }
    let step = path[depth];
    let tail = nest(&path[depth + 1..], new)?;
    match cur.kind()? {
        Kind::Map(_) => {
            let k = match target(&cur, step, true, depth)? {
                Some(Target::Key(k)) => k,
                _ => return Err(invalid("invalid map key")),
            };
            if let Some(old) = cur.map_get(&k)? {
                return splice_rooted(v, &cur, Change { s: old.pos, e: old.end()?, new: &tail, delta: 0 }, out, md);
            }
            let mut at = cur.end()?;
            for e in cur.map_iter()? {
                let (ek, _) = e?;
                if ek.bytes()? > k.as_slice() {
                    at = ek.pos;
                    break;
                }
            }
            let mut entry = k;
            entry.extend_from_slice(&tail);
            splice_rooted(v, &cur, Change { s: at, e: at, new: &entry, delta: 1 }, out, md)
        }
        _ => {
            let Kind::Array(n) = cur.kind()? else { return Err(invalid("cannot set path in scalar")) };
            let i = match target(&cur, step, true, depth)? {
                Some(Target::Index(i)) => i,
                _ => return Err(invalid("path element is not an integer")),
            };
            let n = n as i64;
            let idx = if i < 0 { i + n } else { i };
            if idx < 0 {
                return Err(invalid("array index out of range"));
            }
            if idx < n {
                let e = cur.array_get(idx)?.ok_or(corrupt(cur.pos))?;
                return splice_rooted(v, &cur, Change { s: e.pos, e: e.end()?, new: &tail, delta: 0 }, out, md);
            }
            // Past the end: pad with nulls up to the index.
            let pad = (idx - n) as usize;
            let mut bytes = Vec::with_capacity(pad + tail.len());
            bytes.extend(core::iter::repeat_n(0xf6, pad));
            bytes.extend_from_slice(&tail);
            let at = cur.end()?;
            splice_rooted(v, &cur, Change { s: at, e: at, new: &bytes, delta: pad as i64 + 1 }, out, md)
        }
    }
}

/// Container being rebuilt by `strip_nulls`.
struct StripFrame {
    out_start: usize,
    remaining: u64,
    kept: u64,
    is_map: bool,
}

/// Removes map entries whose value is null (and array null elements when `in_arrays`), recursively.
/// `undefined` is treated like null only when `undefined_too`.
pub fn strip_nulls(v: &Value, in_arrays: bool, undefined_too: bool, out: &mut impl OutBuf, limits: &Limits) -> Result<()> {
    let md = limits.max_depth;
    let base = out.len();
    out.write_byte(VERSION)?;
    let body = v.d.body;
    let is_null = |p: usize| -> Result<bool> {
        Ok(match (Value { d: v.d, pos: p }).kind()? {
            Kind::Null => true,
            Kind::Undefined => undefined_too,
            _ => false,
        })
    };
    let mut stack: Vec<StripFrame> = Vec::new();
    let mut pos = v.pos;
    let mut first = true;
    let mut ticker = crate::ctx::Ticker::default();
    loop {
        ticker.tick();
        if !first {
            // Close finished containers, writing their final head before their content.
            loop {
                let Some(top) = stack.last_mut() else { return datum::finish_index(out, base, md) };
                if top.remaining > 0 {
                    break;
                }
                let f = stack.pop().ok_or(corrupt(pos))?;
                insert_head(out, f.out_start, if f.is_map { MAJOR_MAP } else { MAJOR_ARRAY }, f.kept)?;
            }
            let top = stack.last_mut().ok_or(corrupt(pos))?;
            top.remaining -= 1;
            if top.is_map {
                let kend = Value { d: v.d, pos }.end()?;
                if is_null(kend)? {
                    pos = Value { d: v.d, pos: kend }.end()?;
                    continue;
                }
                out.write_bytes(slice(body, pos, kend)?)?;
                pos = kend;
            } else if in_arrays && is_null(pos)? {
                pos = Value { d: v.d, pos }.end()?;
                continue;
            }
            top.kept += 1;
        }
        first = false;
        let item = Value { d: v.d, pos };
        match item.kind()? {
            Kind::Array(n) | Kind::Map(n) if n > 0 => {
                let is_map = matches!(item.kind()?, Kind::Map(_));
                stack.push(StripFrame { out_start: out.len(), remaining: n, kept: 0, is_map });
                pos = item.head()?.1;
            }
            Kind::Tag(t) => {
                // Tags are transparent: copy the head, strip inside the content.
                write_head(out, MAJOR_TAG, t)?;
                pos = item.head()?.1;
                first = true;
                if stack.is_empty() && pos >= item.end()? {
                    return datum::finish_index(out, base, md);
                }
            }
            _ => {
                let end = item.end()?;
                out.write_bytes(slice(body, pos, end)?)?;
                pos = end;
                if stack.is_empty() {
                    return datum::finish_index(out, base, md);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::PlainCtx;
    use crate::edn;
    use crate::edn::PrintOpts;

    fn st(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        edn::to_datum(text.as_bytes(), &mut out, &mut PlainCtx::default()).unwrap();
        out
    }

    fn show(d: &[u8]) -> String {
        let p = Datum::parse(d).unwrap();
        let mut out = Vec::new();
        edn::print(&Value::root(p), &mut out, PrintOpts::default()).unwrap();
        // The result must be exactly what normalizing its text gives (deterministic, index included).
        let s = String::from_utf8(out).unwrap();
        assert_eq!(st(&s), d, "not canonical: {s}");
        s
    }

    fn run(input: &str, f: impl FnOnce(&Value, &mut Vec<u8>) -> Result<()>) -> String {
        let s = st(input);
        let v = Value::root(Datum::parse(&s).unwrap());
        let mut out = Vec::new();
        f(&v, &mut out).unwrap();
        show(&out)
    }

    fn body(text: &str) -> Vec<u8> {
        Datum::parse(&st(text)).unwrap().body.to_vec()
    }

    #[test]
    fn concat_follows_jsonb() {
        let cat = |a: &str, b: &str| {
            let (sa, sb) = (st(a), st(b));
            let mut out = Vec::new();
            concat(&Value::root(Datum::parse(&sa).unwrap()), &Value::root(Datum::parse(&sb).unwrap()), &mut out, &Limits::default()).unwrap();
            show(&out)
        };
        assert_eq!(cat(r#"{"a": 1, "c": 3}"#, r#"{"b": 2, "a": 9}"#), r#"{"a": 9, "b": 2, "c": 3}"#);
        assert_eq!(cat("[1, 2]", "[3]"), "[1, 2, 3]");
        assert_eq!(cat("[1]", "2"), "[1, 2]");
        assert_eq!(cat("1", "[2]"), "[1, 2]");
        assert_eq!(cat("1", "2"), "[1, 2]");
        assert_eq!(cat(r#"{"a": 1}"#, "[2]"), r#"[{"a": 1}, 2]"#);
    }

    #[test]
    fn deletion() {
        assert_eq!(run(r#"{"a": 1, "b": 2}"#, |v, o| delete_matching(v, &[key_text("a").to_vec()], o, &Limits::default())), r#"{"b": 2}"#);
        assert_eq!(run(r#"["a", "b", "a", 1]"#, |v, o| delete_matching(v, &[key_text("a").to_vec()], o, &Limits::default())), r#"["b", 1]"#);
        assert_eq!(run("[1, 2, 3]", |v, o| delete_index(v, -1, o, &Limits::default())), "[1, 2]");
        assert_eq!(run("[1, 2, 3]", |v, o| delete_index(v, 5, o, &Limits::default())), "[1, 2, 3]");
        assert_eq!(run(r#"{1: "x", 2: "y"}"#, |v, o| delete_index(v, 1, o, &Limits::default())), r#"{2: "y"}"#);
        let s = st("1");
        assert!(delete_index(&Value::root(Datum::parse(&s).unwrap()), 0, &mut Vec::new(), &Limits::default()).is_err());
        assert_eq!(
            run(r#"{"a": [1, {"b": 2, "c": 3}]}"#, |v, o| delete_path(v, &[Step::Text("a"), Step::Text("1"), Step::Text("b")], o, &Limits::default())),
            r#"{"a": [1, {"c": 3}]}"#
        );
        assert_eq!(run(r#"{"a": 1}"#, |v, o| delete_path(v, &[Step::Text("x"), Step::Text("y")], o, &Limits::default())), r#"{"a": 1}"#);
    }

    #[test]
    fn set_and_insert() {
        let new = body(r#""new""#);
        let set = |input: &str, path: &[&str], create: bool| {
            let steps: Vec<Step> = path.iter().map(|s| Step::Text(s)).collect();
            run(input, |v, o| set_path(v, &steps, &new, SetMode::Set { create }, o, &Limits::default()))
        };
        assert_eq!(set(r#"{"a": [1, 2]}"#, &["a", "0"], true), r#"{"a": ["new", 2]}"#);
        assert_eq!(set(r#"{"a": [1, 2]}"#, &["a", "-1"], true), r#"{"a": [1, "new"]}"#);
        assert_eq!(set(r#"{"a": [1, 2]}"#, &["a", "9"], true), r#"{"a": [1, 2, "new"]}"#);
        assert_eq!(set(r#"{"a": [1, 2]}"#, &["a", "-9"], true), r#"{"a": ["new", 1, 2]}"#);
        assert_eq!(set(r#"{"a": [1, 2]}"#, &["a", "9"], false), r#"{"a": [1, 2]}"#);
        assert_eq!(set(r#"{"b": 1, "d": 2}"#, &["c"], true), r#"{"b": 1, "c": "new", "d": 2}"#);
        assert_eq!(set(r#"{"b": 1}"#, &["x", "y"], true), r#"{"b": 1}"#);
        let ins = |input: &str, path: &[&str], after: bool| {
            let steps: Vec<Step> = path.iter().map(|s| Step::Text(s)).collect();
            run(input, |v, o| set_path(v, &steps, &new, SetMode::Insert { after }, o, &Limits::default()))
        };
        assert_eq!(ins("[1, 2]", &["1"], false), r#"[1, "new", 2]"#);
        assert_eq!(ins("[1, 2]", &["1"], true), r#"[1, 2, "new"]"#);
        assert_eq!(ins("[1, 2]", &["-1"], true), r#"[1, 2, "new"]"#);
        assert_eq!(ins(r#"{"a": 1}"#, &["b"], false), r#"{"a": 1, "b": "new"}"#);
        let s = st(r#"{"a": 1}"#);
        let e = set_path(&Value::root(Datum::parse(&s).unwrap()), &[Step::Text("a")], &new, SetMode::Insert { after: false }, &mut Vec::new(), &Limits::default());
        assert!(e.is_err());
    }

    #[test]
    fn edits_inside_indexed_containers_keep_the_index_valid() {
        let big: Vec<String> = (0..40).map(|i| format!("\"k{i:02}\": {i}")).collect();
        let input = format!("{{{}}}", big.join(", "));
        let new = body("[1, 2]");
        let out = run(&input, |v, o| set_path(v, &[Step::Text("k20")], &new, SetMode::Set { create: true }, o, &Limits::default()));
        assert!(out.contains(r#""k20": [1, 2]"#));
        let out = run(&input, |v, o| delete_path(v, &[Step::Text("k05")], o, &Limits::default()));
        assert!(!out.contains("k05"));
    }

    #[test]
    fn assignment_creates_missing_levels_like_jsonb_subscripting() {
        let new = body("7");
        let a = |input: Option<&str>, path: &[Step]| -> String {
            let s = input.map(st);
            let v = s.as_ref().map(|s| Value::root(Datum::parse(s).unwrap()));
            let mut out = Vec::new();
            assign(v.as_ref(), path, &new, &mut out, &Limits::default()).unwrap();
            show(&out)
        };
        assert_eq!(a(Some("{}"), &[Step::Text("a"), Step::Text("b")]), r#"{"a": {"b": 7}}"#);
        assert_eq!(a(Some("[1]"), &[Step::Int(3)]), "[1, null, null, 7]");
        assert_eq!(a(Some(r#"{"a": [1]}"#), &[Step::Text("a"), Step::Int(-1)]), r#"{"a": [7]}"#);
        assert_eq!(a(None, &[Step::Int(1), Step::Text("x")]), r#"[null, {"x": 7}]"#);
        assert_eq!(a(Some("{1: {}}"), &[Step::Int(1), Step::Bytes(&[0xff])]), "{1: {h'ff': 7}}");
        let s = st(r#"{"a": 1}"#);
        assert!(assign(Some(&Value::root(Datum::parse(&s).unwrap())), &[Step::Text("a"), Step::Text("b")], &new, &mut Vec::new(), &Limits::default()).is_err());
    }

    #[test]
    fn strip_nulls_recurses_and_honours_options() {
        let s = |input: &str, arrays: bool, undef: bool| run(input, |v, o| strip_nulls(v, arrays, undef, o, &Limits::default()));
        assert_eq!(s(r#"{"a": null, "b": [null, {"c": null, "d": 1}], "e": undefined}"#, false, false),
            r#"{"b": [null, {"d": 1}], "e": undefined}"#);
        assert_eq!(s(r#"{"a": null, "b": [null, {"c": null}]}"#, true, false), r#"{"b": [{}]}"#);
        assert_eq!(s(r#"{"e": undefined, "f": 1(null)}"#, false, true), r#"{"f": 1(null)}"#);
        assert_eq!(s("null", true, true), "null");
        assert_eq!(s(r#"1({"a": null})"#, false, false), "1({})");
    }
}
