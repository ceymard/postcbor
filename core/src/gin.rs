//! GIN index entries (design §8): what `cbor_ops` and `cbor_path_ops` store for a value, and extract from a query.
//! Entries are on disk: changing anything here changes what existing indexes mean, so it requires a REINDEX.

use crate::cursor::*;
use crate::error::Result;
use crate::ops::is_number_tag;
use crate::value::Value;

/// Role of a `cbor_ops` entry. Array elements and the top-level value share the key role, as in jsonb, so `?`
/// finds array elements through the same entries as map keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Key = 1,
    Value = 2,
}

/// Flag on the role byte of a hashed entry.
const HASHED: u8 = 0x80;
/// Longest unhashed entry, role byte included.
const MAX_PLAIN: usize = 125;

/// One `cbor_ops` entry: the role byte then the item's encoding, or for long items the role with [`HASHED`] and a
/// 4-byte hash of the encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpsEntry<'a> {
    Plain(u8, &'a [u8]),
    Hashed([u8; 5]),
}

impl OpsEntry<'_> {
    pub fn new(role: Role, encoding: &[u8]) -> OpsEntry<'_> {
        if encoding.len() < MAX_PLAIN {
            return OpsEntry::Plain(role as u8, encoding);
        }
        let h = hash(FNV_BASIS, encoding).to_le_bytes();
        OpsEntry::Hashed([role as u8 | HASHED, h[0], h[1], h[2], h[3]])
    }

    pub fn len(&self) -> usize {
        match self {
            OpsEntry::Plain(_, b) => 1 + b.len(),
            OpsEntry::Hashed(h) => h.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// The two parts written one after the other form the entry.
    pub fn parts(&self) -> (&[u8], &[u8]) {
        match self {
            OpsEntry::Plain(r, b) => (core::slice::from_ref(r), b),
            OpsEntry::Hashed(h) => (h, &[]),
        }
    }
}

/// What the walk reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item<'a> {
    /// A map key, whole, whatever its type.
    Key(&'a [u8]),
    /// An atomic item (see [`atomic_end`]) at body offset `pos`, with its role and the hash state of the path leading
    /// to it.
    Leaf { role: Role, pos: usize, bytes: &'a [u8], path: u32 },
}

const FNV_BASIS: u32 = 0x811c_9dc5;
const STEP_KEY: u8 = 1;
const STEP_TAG: u8 = 2;
const STEP_LEAF: u8 = 3;

fn hash(mut h: u32, bytes: &[u8]) -> u32 {
    for &b in bytes {
        h = (h ^ u32::from(b)).wrapping_mul(0x0100_0193);
    }
    h
}

/// Final mixing, so that close paths spread over all 32 bits.
fn finish(mut h: u32) -> u32 {
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^ (h >> 16)
}

/// The `cbor_path_ops` entry of a leaf.
pub fn path_entry(path: u32, leaf: &[u8]) -> u32 {
    finish(hash(hash(path, &[STEP_LEAF]), leaf))
}

/// For an item that containment compares whole, its end; None for arrays, maps, and tags that containment looks
/// into. Atomic: scalars, and tags whose content (through further tags) is atomic, or that are number tags.
pub fn atomic_end(v: &Value) -> Result<Option<usize>> {
    let mut pos = v.pos;
    loop {
        let (h, next) = Value { d: v.d, pos }.head()?;
        match h.major {
            MAJOR_ARRAY | MAJOR_MAP => return Ok(None),
            MAJOR_TAG if !is_number_tag(h.arg) => pos = next,
            _ => return Ok(Some(v.end()?)),
        }
    }
}

struct Frame {
    path: u32,
    left: u64,
    is_map: bool,
}

/// Walks a value depth-first, reporting every map key and every atomic item, in body order.
pub fn walk<'a>(v: &Value<'a>, mut f: impl FnMut(Item<'a>) -> Result<()>) -> Result<()> {
    let body = v.d.body;
    let mut stack: Vec<Frame> = Vec::new();
    let mut ticker = crate::ctx::Ticker::default();
    // The item to visit, its role and its path.
    let (mut pos, mut role, mut path) = (v.pos, Role::Key, FNV_BASIS);
    loop {
        ticker.tick();
        let item = Value { d: v.d, pos };
        if let Some(end) = atomic_end(&item)? {
            f(Item::Leaf { role, pos: item.pos, bytes: item.bytes()?, path })?;
            pos = end;
        } else {
            // Tags that containment looks into are path steps; then a container.
            loop {
                let (h, next) = Value { d: v.d, pos }.head()?;
                pos = next;
                if h.major == MAJOR_TAG {
                    path = hash(hash(path, &[STEP_TAG]), &h.arg.to_be_bytes());
                    continue;
                }
                if h.arg > 0 {
                    crate::out::try_push(&mut stack, Frame { path, left: h.arg, is_map: h.major == MAJOR_MAP })?;
                }
                break;
            }
        }
        // Next item: the next entry of the innermost unfinished container.
        loop {
            let Some(top) = stack.last_mut() else { return Ok(()) };
            if top.left == 0 {
                stack.pop();
                continue;
            }
            top.left -= 1;
            path = top.path;
            role = Role::Key;
            if top.is_map {
                let key = Value { d: v.d, pos };
                let end = key.end()?;
                let bytes = body.get(pos..end).ok_or(crate::error::Error::new(crate::error::ErrorKind::CorruptDatum, pos))?;
                f(Item::Key(bytes))?;
                path = hash(hash(path, &[STEP_KEY]), bytes);
                role = Role::Value;
                pos = end;
            }
            break;
        }
    }
}

/// `cbor_ops` entries of a containment query, possibly repeated (GIN removes duplicates).
pub fn ops_entries<'a>(v: &Value<'a>, mut f: impl FnMut(OpsEntry<'_>) -> Result<()>) -> Result<()> {
    walk(v, |item| match item {
        Item::Key(k) => f(OpsEntry::new(Role::Key, k)),
        Item::Leaf { role, bytes, .. } => f(OpsEntry::new(role, bytes)),
    })
}

/// `cbor_ops` entries of a stored value: those of [`ops_entries`], and for each leaf whose jsonpath equality form
/// differs from its encoding (a float, a decimal fraction, a tagged scalar), that form too, for `@?` and `@@`.
pub fn index_entries<'a>(v: &Value<'a>, mut f: impl FnMut(OpsEntry<'_>) -> Result<()>) -> Result<()> {
    walk(v, |item| match item {
        Item::Key(k) => f(OpsEntry::new(Role::Key, k)),
        Item::Leaf { role, pos, bytes, .. } => {
            f(OpsEntry::new(role, bytes))?;
            match crate::jsonpath::gin::equality_form(Value { d: v.d, pos })? {
                Some(form) if form != bytes => f(OpsEntry::new(role, &form)),
                _ => Ok(()),
            }
        }
    })
}

/// `cbor_path_ops` entries of a value, possibly repeated.
pub fn path_entries(v: &Value, mut f: impl FnMut(u32) -> Result<()>) -> Result<()> {
    walk(v, |item| match item {
        Item::Key(_) => Ok(()),
        Item::Leaf { bytes, path, .. } => f(path_entry(path, bytes)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::PlainCtx;
    use crate::datum::Datum;
    use crate::edn;
    use crate::ops;

    fn st(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        edn::to_datum(text.as_bytes(), &mut out, &mut PlainCtx::default()).unwrap();
        out
    }

    fn ops_of(text: &str) -> Vec<Vec<u8>> {
        let s = st(text);
        let d = Datum::parse(&s).unwrap();
        let mut out = Vec::new();
        ops_entries(&Value::root(&d), |e| {
            let (a, b) = e.parts();
            out.push([a, b].concat());
            Ok(())
        })
        .unwrap();
        out.sort();
        out.dedup();
        out
    }

    fn paths_of(text: &str) -> Vec<u32> {
        let s = st(text);
        let d = Datum::parse(&s).unwrap();
        let mut out = Vec::new();
        path_entries(&Value::root(&d), |h| {
            out.push(h);
            Ok(())
        })
        .unwrap();
        out.sort();
        out.dedup();
        out
    }

    fn body(text: &str) -> Vec<u8> {
        Datum::parse(&st(text)).unwrap().body.to_vec()
    }

    fn key(text: &str) -> Vec<u8> {
        [&[Role::Key as u8][..], &body(text)].concat()
    }

    fn val(text: &str) -> Vec<u8> {
        [&[Role::Value as u8][..], &body(text)].concat()
    }

    #[test]
    fn ops_entries_are_keys_and_atomic_values() {
        let mut want = vec![key("\"a\""), key("1"), key("\"b\""), val("2"), key("\"x\""), key("1(5)"), key("4([-2, 150])"), key("3")];
        want.sort();
        assert_eq!(ops_of(r#"{"a": [1, {"b": 2}, "x", 1(5), 4([-2, 150]), 258([3])]}"#), want);
        // Map keys of any type are entries, whole; the top-level scalar is an element.
        assert_eq!(ops_of(r#"{[1, 2]: null}"#), {
            let mut w = vec![key("[1, 2]"), val("null")];
            w.sort();
            w
        });
        assert_eq!(ops_of("7"), vec![key("7")]);
        assert!(ops_of("{}").is_empty() && ops_of("[[], {}]").is_empty());
    }

    #[test]
    fn long_entries_are_hashed() {
        let long = format!("\"{}\"", "x".repeat(200));
        let e = ops_of(&format!("[{long}]"));
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].len(), 5);
        assert_eq!(e[0][0], Role::Key as u8 | HASHED);
        assert_eq!(ops_of(&format!("[{long}]")), e);
    }

    #[test]
    fn path_entries_follow_keys_and_tags_not_arrays() {
        let doc = paths_of(r#"{"a": [1, {"b": 2}], "t": 258([3])}"#);
        for q in [r#"{"a": [1]}"#, r#"{"a": [{"b": 2}]}"#, r#"{"t": 258([3])}"#, r#"{"a": 1}"#] {
            for h in paths_of(q) {
                assert!(doc.contains(&h), "{q}");
            }
        }
        for q in [r#"{"b": 2}"#, r#"{"a": 2}"#, r#"{"t": 259([3])}"#, r#"{"t": [3]}"#, r#"{1: [1]}"#] {
            assert!(paths_of(q).iter().any(|h| !doc.contains(h)), "{q}");
        }
        // Top level: an array's elements and a scalar share the root path, like `[1, 2] @> 1`.
        assert_eq!(paths_of("1"), paths_of("[1]"));
    }

    /// The index must never miss a match: every entry of a contained value is an entry of the container.
    #[test]
    fn contained_values_have_a_subset_of_entries() {
        let pairs = [
            (r#"{"a": 1, "b": [1, 2, {"c": 3}]}"#, r#"{"b": [{"c": 3}]}"#),
            ("[1, 2]", "1"),
            ("258([1, 2, 3])", "258([3])"),
            (r#"[{"x": 1}, {"x": 1, "y": 2}]"#, r#"[{"x": 1, "y": 2}]"#),
            ("{1: [h'ff']}", "{1: [h'ff']}"),
            (r#"7({"a": 4([-2, 150])})"#, r#"7({"a": 4([-2, 150])})"#),
        ];
        for (a, b) in pairs {
            let (sa, sb) = (st(a), st(b));
            let (da, db) = (Datum::parse(&sa).unwrap(), Datum::parse(&sb).unwrap());
            assert!(ops::contains(&Value::root(&da), &Value::root(&db), &mut PlainCtx::default()).unwrap());
            let (oa, ob) = (ops_of(a), ops_of(b));
            assert!(ob.iter().all(|e| oa.contains(e)), "{a} @> {b}");
            let (pa, pb) = (paths_of(a), paths_of(b));
            assert!(pb.iter().all(|h| pa.contains(h)), "{a} @> {b}");
        }
    }
}
