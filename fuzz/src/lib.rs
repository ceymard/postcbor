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

    let root = Value::root(&d);
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

/// Checks `ops::contains` against a direct recursive reading of the jsonb rules, and that a value contains itself.
pub fn check_contains(a: &Value, b: &Value) {
    let got = postbor_core::ops::contains(a, b, &mut ctx()).unwrap();
    assert_eq!(got, contains_ref(a, b, true), "containment disagrees with the reference");
    assert!(postbor_core::ops::contains(a, a, &mut ctx()).unwrap(), "a value does not contain itself");
}

/// Reference containment. Recursive: fuzz values are limited to depth 64.
fn contains_ref(a: &Value, b: &Value, top: bool) -> bool {
    let is_container = |k: &Kind| matches!(k, Kind::Array(_) | Kind::Map(_) | Kind::Tag(_));
    match (a.kind().unwrap(), b.kind().unwrap()) {
        // Top level only: an array contains a scalar that is one of its elements.
        (Kind::Array(_), kb) if top && !matches!(kb, Kind::Array(_) | Kind::Map(_)) => {
            a.array_iter().unwrap().any(|e| e.unwrap().bytes().unwrap() == b.bytes().unwrap())
        }
        (Kind::Map(_), Kind::Map(_)) => b.map_iter().unwrap().all(|e| {
            let (k, bv) = e.unwrap();
            a.map_get(k.bytes().unwrap()).unwrap().is_some_and(|av| contains_ref(&av, &bv, false))
        }),
        (Kind::Array(_), Kind::Array(_)) => b
            .array_iter()
            .unwrap()
            .all(|be| a.array_iter().unwrap().any(|ae| contains_ref(&ae.unwrap(), be.as_ref().unwrap(), false))),
        (Kind::Tag(x), Kind::Tag(y)) if x == y && postbor_core::ops::is_number_tag(x) => a.bytes().unwrap() == b.bytes().unwrap(),
        (Kind::Tag(x), Kind::Tag(y)) => x == y && contains_ref(&a.tag_content().unwrap(), &b.tag_content().unwrap(), false),
        (ka, kb) if is_container(&ka) || is_container(&kb) => false,
        _ => a.bytes().unwrap() == b.bytes().unwrap(),
    }
}
