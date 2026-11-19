#![no_main]
//! Ordering laws on three values: antisymmetry, transitivity, equality exactly when bodies are equal.
use core::cmp::Ordering;

use libfuzzer_sys::fuzz_target;
use postbor_core::cmp::compare;
use postbor_core::datum::Datum;
use postbor_core::value::Value;

fuzz_target!(|parts: (Vec<u8>, Vec<u8>, Vec<u8>)| {
    let (Some(a), Some(b), Some(c)) =
        (postbor_fuzz::store(&parts.0), postbor_fuzz::store(&parts.1), postbor_fuzz::store(&parts.2))
    else {
        return;
    };
    let (da, db, dc) = (Datum::parse(&a).unwrap(), Datum::parse(&b).unwrap(), Datum::parse(&c).unwrap());
    let (va, vb, vc) = (Value::root(&da), Value::root(&db), Value::root(&dc));
    let ab = compare(&va, &vb).unwrap();
    assert_eq!(ab, compare(&vb, &va).unwrap().reverse(), "antisymmetry");
    assert_eq!(ab == Ordering::Equal, da.body == db.body, "equality must match bodies");
    let bc = compare(&vb, &vc).unwrap();
    if ab == bc && ab != Ordering::Equal {
        assert_eq!(compare(&va, &vc).unwrap(), ab, "transitivity");
    }
});
