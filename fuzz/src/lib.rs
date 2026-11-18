//! Invariants every stored value must satisfy, shared by the fuzz targets.

use core::cmp::Ordering;

use postbor_core::ctx::{Limits, PlainCtx};
use postbor_core::datum::{self, Datum};
use postbor_core::value::{Kind, Value};
use postbor_core::{cmp, edn, json};

/// Small limits keep each run fast and still exercise the limit paths.
pub fn ctx() -> PlainCtx {
    PlainCtx(Limits { max_depth: 64, max_expansion_ratio: 16, max_expanded_size: 1 << 20 })
}

/// Normalizes untrusted bytes; None when they are rejected.
pub fn store(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    datum::from_cbor(input, &mut out, &mut ctx()).ok().map(|_| out)
}

/// Checks a stored datum: idempotent normalization, EDN round trip, self-comparison,
/// index lookups agreeing with plain iteration, and JSON conversion not failing in lossy mode.
pub fn check_stored(stored: &[u8]) {
    let d = Datum::parse(stored).expect("stored datum parses");
    let again = store(d.body).expect("a stored body normalizes");
    assert_eq!(again, stored, "normalization is not idempotent");

    let root = Value::root(d);
    assert_eq!(root.end().unwrap(), d.body.len());
    assert_eq!(cmp::compare(&root, &root).unwrap(), Ordering::Equal);

    let mut text = Vec::new();
    edn::print(&root, &mut text, Default::default()).unwrap();
    let mut back = Vec::new();
    edn::to_datum(&text, &mut back, &mut PlainCtx::default())
        .unwrap_or_else(|e| panic!("EDN output does not read back: {e} in {}", String::from_utf8_lossy(&text)));
    assert_eq!(back, stored, "EDN round trip changed the value: {}", String::from_utf8_lossy(&text));

    json::print(&root, &mut Vec::new(), false).unwrap();
    check_lookups(&root);
}

/// Every element and key found by iteration is found again by direct lookup (index or scan).
fn check_lookups(v: &Value) {
    let mut todo = vec![*v];
    while let Some(v) = todo.pop() {
        match v.kind().unwrap() {
            Kind::Array(n) => {
                for (i, e) in v.array_iter().unwrap().enumerate() {
                    let e = e.unwrap();
                    assert_eq!(v.array_get(i as i64).unwrap().unwrap().pos, e.pos);
                    assert_eq!(v.array_get(i as i64 - n as i64).unwrap().unwrap().pos, e.pos);
                    todo.push(e);
                }
            }
            Kind::Map(_) => {
                for e in v.map_iter().unwrap() {
                    let (k, val) = e.unwrap();
                    assert_eq!(v.map_get(k.bytes().unwrap()).unwrap().unwrap().pos, val.pos);
                    todo.push(k);
                    todo.push(val);
                }
            }
            Kind::Tag(_) => todo.push(v.tag_content().unwrap()),
            _ => {}
        }
    }
}
