//! GIN operator classes `cbor_ops` (default) and `cbor_path_ops` (design §8). Entries come from `postbor_core::gin`;
//! this module reads the support-function arguments and lays the entries out in PostgreSQL memory.

use pgrx::pg_sys;
use pgrx::prelude::*;
use postbor_core::gin::{self, OpsEntry, Role};
use postbor_core::value::Value;
use postbor_core::{OutBuf, Result, ops};

use crate::datum::{Cbor, InternalOut, PallocBuf, varlena_header};
use crate::error::{OrRaise, Source};
use crate::io::parse_datum;

// Strategy numbers, shared with the operator class definitions below. 7–11 are jsonb's.
const CONTAINS: i16 = 7;
const EXISTS: i16 = 9;
const EXISTS_ANY: i16 = 10;
const EXISTS_ALL: i16 = 11;
const HAS: i16 = 12;
const HAS_ANY: i16 = 13;
const HAS_ALL: i16 = 14;

/// `cbor_ops` entries: varlenas back to back in one block, each 4-byte aligned, then an array of pointers to them.
struct OpsOut {
    block: PallocBuf,
    offsets: Vec<usize>,
}

impl OpsOut {
    fn new(capacity: usize) -> Self {
        OpsOut { block: PallocBuf::raw(capacity), offsets: Vec::new() }
    }

    fn push(&mut self, e: OpsEntry<'_>) -> Result<()> {
        postbor_core::out::try_push(&mut self.offsets, self.block.len())?;
        let total = 4 + e.len();
        let (a, b) = e.parts();
        self.block.write_bytes(&varlena_header(total))?;
        self.block.write_bytes(a)?;
        self.block.write_bytes(b)?;
        self.block.write_bytes(&[0u8; 3][..total.next_multiple_of(4) - total])
    }

    /// The entry array, with its length stored in `*nentries`.
    fn finish(self, nentries: *mut i32) -> InternalOut {
        let base = self.block.into_ptr();
        // SAFETY: each offset is the start of a complete varlena inside `base`'s block.
        let datums = self.offsets.iter().map(|&o| pg_sys::Datum::from(unsafe { base.add(o) }));
        finish_datums(datums, self.offsets.len(), nentries)
    }
}

/// Copies `datums` into a palloc'd array for GIN and sets `*nentries`.
fn finish_datums(datums: impl Iterator<Item = pg_sys::Datum>, n: usize, nentries: *mut i32) -> InternalOut {
    let mut arr = PallocBuf::raw(n * size_of::<pg_sys::Datum>());
    for d in datums {
        arr.write_bytes(&d.value().to_ne_bytes()).or_raise(Source::Stored);
    }
    // SAFETY: GIN passes a valid pointer for the entry count.
    unsafe { *nentries = i32::try_from(n).unwrap_or_else(|_| crate::error::fail(PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED, "too many index entries")) };
    InternalOut(pg_sys::Datum::from(arr.into_ptr()))
}

fn path_out(hashes: &[u32], nentries: *mut i32) -> InternalOut {
    finish_datums(hashes.iter().map(|&h| pg_sys::Datum::from(h as i32)), hashes.len(), nentries)
}

fn ops_of_value(v: &Value, capacity: usize) -> OpsOut {
    let mut out = OpsOut::new(capacity);
    gin::ops_entries(v, |e| out.push(e)).or_raise(Source::Stored);
    out
}

fn paths_of_value(v: &Value) -> Vec<u32> {
    let mut out = Vec::new();
    gin::path_entries(v, |h| postbor_core::out::try_push(&mut out, h)).or_raise(Source::Stored);
    out
}

/// Argument `n` as a raw datum.
fn arg(fcinfo: pg_sys::FunctionCallInfo, n: usize) -> pg_sys::Datum {
    // SAFETY: GIN calls support functions with all their declared arguments.
    unsafe { pgrx::fcinfo::pg_get_nullable_datum(fcinfo, n).value }
}

/// Argument `n` as a pointer of the type the GIN API documents for it.
fn ptr_arg<T>(fcinfo: pg_sys::FunctionCallInfo, n: usize) -> *mut T {
    arg(fcinfo, n).cast_mut_ptr::<T>()
}

fn cbor_arg<'a>(fcinfo: pg_sys::FunctionCallInfo, n: usize) -> Cbor<'a> {
    // SAFETY: the argument is a non-null cbor datum that lives for the call.
    unsafe { Cbor::from_datum(arg(fcinfo, n), false) }.unwrap_or_else(|| crate::error::fail(PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED, "null cbor"))
}

fn strategy(fcinfo: pg_sys::FunctionCallInfo, n: usize) -> i16 {
    arg(fcinfo, n).value() as i16
}

fn set_search_all(fcinfo: pg_sys::FunctionCallInfo) {
    // SAFETY: argument 6 of extractQuery is the int32 search mode.
    unsafe { *ptr_arg::<i32>(fcinfo, 6) = pg_sys::GIN_SEARCH_MODE_ALL as i32 };
}

/// extractValue for `cbor_ops`: `(cbor, internal nentries, internal nullFlags)`.
#[pg_extern(sql = false)]
fn cbor_gin_extract_value(fcinfo: pg_sys::FunctionCallInfo) -> InternalOut {
    let c = cbor_arg(fcinfo, 0);
    let d = parse_datum(c);
    ops_of_value(&Value::root(&d), c.0.len() * 2).finish(ptr_arg(fcinfo, 1))
}

/// extractValue for `cbor_path_ops`.
#[pg_extern(sql = false)]
fn cbor_gin_extract_value_path(fcinfo: pg_sys::FunctionCallInfo) -> InternalOut {
    let d = parse_datum(cbor_arg(fcinfo, 0));
    path_out(&paths_of_value(&Value::root(&d)), ptr_arg(fcinfo, 1))
}

/// extractQuery for `cbor_ops`: `(query, internal nentries, int2 strategy, internal partial, internal extra,
/// internal nullFlags, internal searchMode)`. The query's type depends on the strategy.
#[pg_extern(sql = false)]
fn cbor_gin_extract_query(fcinfo: pg_sys::FunctionCallInfo) -> InternalOut {
    let nentries = ptr_arg::<i32>(fcinfo, 1);
    let mut out = OpsOut::new(64);
    let key = |out: &mut OpsOut, enc: &[u8]| out.push(OpsEntry::new(Role::Key, enc)).or_raise(Source::Stored);
    match strategy(fcinfo, 2) {
        CONTAINS => {
            let d = parse_datum(cbor_arg(fcinfo, 0));
            out = ops_of_value(&Value::root(&d), 64);
        }
        EXISTS => {
            // SAFETY: for `?` the query is a non-null text datum.
            let t = unsafe { crate::datum::TextArg::from_datum(arg(fcinfo, 0), false) }.map(|t| t.0).unwrap_or_default();
            key(&mut out, &ops::key_text(t).to_vec());
        }
        s @ (EXISTS_ANY | EXISTS_ALL) => {
            // SAFETY: for `?|` and `?&` the query is a non-null text[] datum.
            let keys = unsafe { crate::datum::TextArray::from_datum(arg(fcinfo, 0), false) };
            for k in keys.iter().flat_map(|a| a.iter()).flatten() {
                key(&mut out, &ops::key_text(k).to_vec());
            }
            if s == EXISTS_ALL && out.offsets.is_empty() {
                set_search_all(fcinfo);
            }
        }
        HAS => {
            let d = parse_datum(cbor_arg(fcinfo, 0));
            let k = Value::root(&d);
            // Only atomic keys are entries of arrays; other keys can only be checked row by row.
            match gin::atomic_end(&k).or_raise(Source::Stored) {
                Some(_) => key(&mut out, k.bytes().or_raise(Source::Stored)),
                None => set_search_all(fcinfo),
            }
        }
        s @ (HAS_ANY | HAS_ALL) => {
            let d = parse_datum(cbor_arg(fcinfo, 0));
            let mut unindexable = false;
            for k in Value::root(&d).array_iter().or_raise(Source::Stored) {
                let k = k.or_raise(Source::Stored);
                match gin::atomic_end(&k).or_raise(Source::Stored) {
                    Some(_) => key(&mut out, k.bytes().or_raise(Source::Stored)),
                    None => unindexable = true,
                }
            }
            // "Any" cannot rule a row out when one key is unindexable: no entries at all, or rows lacking the other
            // keys would be dropped. "All" just checks fewer keys.
            if s == HAS_ANY && unindexable {
                out = OpsOut::new(0);
                set_search_all(fcinfo);
            } else if s == HAS_ALL && out.offsets.is_empty() {
                set_search_all(fcinfo);
            }
        }
        s => crate::error::fail(PgSqlErrorCode::ERRCODE_INTERNAL_ERROR, format!("unrecognized strategy number: {s}")),
    }
    if out.offsets.is_empty() && !matches!(strategy(fcinfo, 2), EXISTS_ANY | HAS_ANY) {
        // No entries: every row is a candidate (`@> '{}'`).
        set_search_all(fcinfo);
    }
    out.finish(nentries)
}

/// extractQuery for `cbor_path_ops`: only `@>`.
#[pg_extern(sql = false)]
fn cbor_gin_extract_query_path(fcinfo: pg_sys::FunctionCallInfo) -> InternalOut {
    let s = strategy(fcinfo, 2);
    if s != CONTAINS {
        crate::error::fail(PgSqlErrorCode::ERRCODE_INTERNAL_ERROR, format!("unrecognized strategy number: {s}"));
    }
    let d = parse_datum(cbor_arg(fcinfo, 0));
    let hashes = paths_of_value(&Value::root(&d));
    if hashes.is_empty() {
        set_search_all(fcinfo);
    }
    path_out(&hashes, ptr_arg(fcinfo, 1))
}

/// Whether a strategy needs one matching entry rather than all of them.
fn is_any(strategy: i16) -> bool {
    matches!(strategy, EXISTS_ANY | HAS_ANY)
}

/// consistent, both classes: `(internal check, int2 strategy, query, int4 nkeys, internal extra,
/// internal recheck, internal queryKeys, internal nullFlags)`. Entries are lossy, so matches are always rechecked.
#[pg_extern(sql = false)]
fn cbor_gin_consistent(fcinfo: pg_sys::FunctionCallInfo) -> bool {
    let n = arg(fcinfo, 3).value() as i32 as usize;
    // SAFETY: GIN passes `nkeys` check flags and a pointer for the recheck flag.
    let check = unsafe {
        *ptr_arg::<bool>(fcinfo, 5) = true;
        if n == 0 { &[][..] } else { core::slice::from_raw_parts(ptr_arg::<bool>(fcinfo, 0), n) }
    };
    // With no keys (search mode ALL) every row is a candidate.
    n == 0 || if is_any(strategy(fcinfo, 1)) { check.iter().any(|&c| c) } else { check.iter().all(|&c| c) }
}

/// triConsistent, both classes: `(internal check, int2 strategy, query, int4 nkeys, internal extra,
/// internal queryKeys, internal nullFlags)`. Never answers TRUE: matches are rechecked.
#[pg_extern(sql = false)]
fn cbor_gin_triconsistent(fcinfo: pg_sys::FunctionCallInfo) -> i8 {
    let n = arg(fcinfo, 3).value() as i32 as usize;
    // SAFETY: GIN passes `nkeys` ternary check values.
    let check = if n == 0 { &[][..] } else { unsafe { core::slice::from_raw_parts(ptr_arg::<i8>(fcinfo, 0), n) } };
    let (f, maybe) = (pg_sys::GIN_FALSE as i8, pg_sys::GIN_MAYBE as i8);
    let possible = n == 0 || if is_any(strategy(fcinfo, 1)) { check.iter().any(|&c| c != f) } else { check.iter().all(|&c| c != f) };
    if possible { maybe } else { f }
}

pgrx::extension_sql!(
    r#"
CREATE FUNCTION cbor_gin_extract_value(cbor, internal, internal) RETURNS internal
    IMMUTABLE STRICT PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_gin_extract_value_wrapper';
CREATE FUNCTION cbor_gin_extract_query(cbor, internal, int2, internal, internal, internal, internal) RETURNS internal
    IMMUTABLE STRICT PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_gin_extract_query_wrapper';
CREATE FUNCTION cbor_gin_extract_value_path(cbor, internal, internal) RETURNS internal
    IMMUTABLE STRICT PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_gin_extract_value_path_wrapper';
CREATE FUNCTION cbor_gin_extract_query_path(cbor, internal, int2, internal, internal, internal, internal) RETURNS internal
    IMMUTABLE STRICT PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_gin_extract_query_path_wrapper';
CREATE FUNCTION cbor_gin_consistent(internal, int2, cbor, int4, internal, internal, internal, internal) RETURNS bool
    IMMUTABLE STRICT PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_gin_consistent_wrapper';
CREATE FUNCTION cbor_gin_triconsistent(internal, int2, cbor, int4, internal, internal, internal) RETURNS "char"
    IMMUTABLE STRICT PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_gin_triconsistent_wrapper';

CREATE OPERATOR CLASS cbor_ops DEFAULT FOR TYPE cbor USING gin AS
    OPERATOR 7 @>(cbor, cbor),
    OPERATOR 9 ?(cbor, text),
    OPERATOR 10 ?|(cbor, text[]),
    OPERATOR 11 ?&(cbor, text[]),
    OPERATOR 12 ?#(cbor, cbor),
    OPERATOR 13 ?#|(cbor, cbor),
    OPERATOR 14 ?#&(cbor, cbor),
    FUNCTION 1 byteacmp(bytea, bytea),
    FUNCTION 2 cbor_gin_extract_value(cbor, internal, internal),
    FUNCTION 3 cbor_gin_extract_query(cbor, internal, int2, internal, internal, internal, internal),
    FUNCTION 4 cbor_gin_consistent(internal, int2, cbor, int4, internal, internal, internal, internal),
    FUNCTION 6 cbor_gin_triconsistent(internal, int2, cbor, int4, internal, internal, internal),
    STORAGE bytea;

CREATE OPERATOR CLASS cbor_path_ops FOR TYPE cbor USING gin AS
    OPERATOR 7 @>(cbor, cbor),
    FUNCTION 1 btint4cmp(int4, int4),
    FUNCTION 2 cbor_gin_extract_value_path(cbor, internal, internal),
    FUNCTION 3 cbor_gin_extract_query_path(cbor, internal, int2, internal, internal, internal, internal),
    FUNCTION 4 cbor_gin_consistent(internal, int2, cbor, int4, internal, internal, internal, internal),
    FUNCTION 6 cbor_gin_triconsistent(internal, int2, cbor, int4, internal, internal, internal),
    STORAGE int4;
"#,
    name = "cbor_gin",
    requires = [
        "cbor_type",
        access::cbor_contains,
        access::cbor_exists,
        access::cbor_exists_any,
        access::cbor_exists_all,
        access::cbor_has,
        access::cbor_has_any,
        access::cbor_has_all
    ],
);
