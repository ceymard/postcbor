//! Equality, ordering and hashing (design §7), with the default btree and hash operator classes.

use core::cmp::Ordering;

use pgrx::pg_sys;
use pgrx::prelude::*;
use postbor_core::cmp::compare;
use postbor_core::value::Value;

use crate::datum::Cbor;
use crate::error::{OrRaise, Source};
use crate::io::parse_datum;

/// Sorts and hash joins call these functions many times in one memory context: detoasted copies are freed.
fn cmp(a: Cbor<'_>, b: Cbor<'_>) -> Ordering {
    let (da, db) = (parse_datum(a), parse_datum(b));
    // Equal bodies are equal values; skip the walk.
    let r = if da.body == db.body { Ordering::Equal } else { compare(&Value::root(da), &Value::root(db)).or_raise(Source::Stored) };
    a.free_copy();
    b.free_copy();
    r
}

fn bodies_equal(a: Cbor<'_>, b: Cbor<'_>) -> bool {
    let r = parse_datum(a).body == parse_datum(b).body;
    a.free_copy();
    b.free_copy();
    r
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_cmp(a: Cbor<'_>, b: Cbor<'_>) -> i32 {
    cmp(a, b) as i32
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(=)]
#[commutator(=)]
#[negator(<>)]
#[restrict(eqsel)]
#[join(eqjoinsel)]
#[hashes]
#[merges]
fn cbor_eq(a: Cbor<'_>, b: Cbor<'_>) -> bool {
    bodies_equal(a, b)
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(<>)]
#[commutator(<>)]
#[negator(=)]
#[restrict(neqsel)]
#[join(neqjoinsel)]
fn cbor_ne(a: Cbor<'_>, b: Cbor<'_>) -> bool {
    !bodies_equal(a, b)
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(<)]
#[commutator(>)]
#[negator(>=)]
#[restrict(scalarltsel)]
#[join(scalarltjoinsel)]
fn cbor_lt(a: Cbor<'_>, b: Cbor<'_>) -> bool {
    cmp(a, b) == Ordering::Less
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(<=)]
#[commutator(>=)]
#[negator(>)]
#[restrict(scalarlesel)]
#[join(scalarlejoinsel)]
fn cbor_le(a: Cbor<'_>, b: Cbor<'_>) -> bool {
    cmp(a, b) != Ordering::Greater
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(>)]
#[commutator(<)]
#[negator(<=)]
#[restrict(scalargtsel)]
#[join(scalargtjoinsel)]
fn cbor_gt(a: Cbor<'_>, b: Cbor<'_>) -> bool {
    cmp(a, b) == Ordering::Greater
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(>=)]
#[commutator(<=)]
#[negator(<)]
#[restrict(scalargesel)]
#[join(scalargejoinsel)]
fn cbor_ge(a: Cbor<'_>, b: Cbor<'_>) -> bool {
    cmp(a, b) != Ordering::Less
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_hash(c: Cbor<'_>) -> i32 {
    let body = parse_datum(c).body;
    // SAFETY: hash_bytes reads exactly `len` bytes.
    let h = unsafe { pg_sys::hash_bytes(body.as_ptr(), body.len() as i32) as i32 };
    c.free_copy();
    h
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_hash_extended(c: Cbor<'_>, seed: i64) -> i64 {
    let body = parse_datum(c).body;
    // SAFETY: as above.
    let h = unsafe { pg_sys::hash_bytes_extended(body.as_ptr(), body.len() as i32, seed as u64) as i64 };
    c.free_copy();
    h
}

pgrx::extension_sql!(
    r#"
CREATE OPERATOR CLASS cbor_ops DEFAULT FOR TYPE cbor USING btree AS
    OPERATOR 1 <,
    OPERATOR 2 <=,
    OPERATOR 3 =,
    OPERATOR 4 >=,
    OPERATOR 5 >,
    FUNCTION 1 cbor_cmp(cbor, cbor);

CREATE OPERATOR CLASS cbor_ops DEFAULT FOR TYPE cbor USING hash AS
    OPERATOR 1 =,
    FUNCTION 1 cbor_hash(cbor),
    FUNCTION 2 cbor_hash_extended(cbor, bigint);
"#,
    name = "cbor_opclasses",
    requires = [cbor_cmp, cbor_eq, cbor_ne, cbor_lt, cbor_le, cbor_gt, cbor_ge, cbor_hash, cbor_hash_extended],
);
