//! Builders (`cbor_build_*`, `cbor_object`) and aggregates (`cbor_agg*`, `cbor_object_agg*`), design §8.
//! `VARIADIC "any"` and aggregate states need the raw function-call interface, so their SQL is written here.

use pgrx::prelude::*;
use pgrx::{PgMemoryContexts, pg_sys};
use postcbor_core::cursor::MAJOR_TEXT;
use postcbor_core::encode::write_head;
use postcbor_core::normalize::Duplicates;
use postcbor_core::{OutBuf, datum};

use crate::convert::{datum_to_cbor, finish_raw};
use crate::datum::{CborOut, InternalOut, PallocBuf};
use crate::error::{OrRaise, Source, fail};
use crate::settings::PgCtx;

/// Arguments of a `VARIADIC "any"` call: (datum, type, is_null).
fn variadic_args(fcinfo: pg_sys::FunctionCallInfo) -> Vec<(pg_sys::Datum, pg_sys::Oid, bool)> {
    let mut args: *mut pg_sys::Datum = core::ptr::null_mut();
    let mut types: *mut pg_sys::Oid = core::ptr::null_mut();
    let mut nulls: *mut bool = core::ptr::null_mut();
    // SAFETY: extract_variadic_args requires at least one argument (the zero-argument SQL overloads share
    // this function); it fills three arrays of `n` entries, or returns -1 for a NULL variadic array.
    unsafe {
        if (*fcinfo).nargs == 0 {
            return Vec::new();
        }
        let n = pg_sys::extract_variadic_args(fcinfo, 0, true, &mut args, &mut types, &mut nulls);
        if n <= 0 {
            return Vec::new();
        }
        let n = n as usize;
        (0..n).map(|i| (*args.add(i), *types.add(i), *nulls.add(i))).collect()
    }
}

#[pg_extern(sql = false)]
fn cbor_build_array(fcinfo: pg_sys::FunctionCallInfo) -> CborOut {
    let mut raw = vec![0x9f];
    for (d, t, null) in variadic_args(fcinfo) {
        if null { raw.push(0xf6) } else { datum_to_cbor(d, t, &mut raw) }
    }
    raw.push(0xff);
    finish_raw(&raw)
}

#[pg_extern(sql = false)]
fn cbor_build_object(fcinfo: pg_sys::FunctionCallInfo) -> CborOut {
    let args = variadic_args(fcinfo);
    if !args.len().is_multiple_of(2) {
        fail(PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE, "argument list must have even number of elements");
    }
    let mut raw = vec![0xbf];
    for (i, pair) in args.chunks(2).enumerate() {
        let (kd, kt, knull) = pair[0];
        let (vd, vt, vnull) = pair[1];
        if knull {
            fail(PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED, format!("argument {}: key must not be null", i * 2 + 1));
        }
        // Keys keep their type: an integer argument is an integer key.
        datum_to_cbor(kd, kt, &mut raw);
        if vnull { raw.push(0xf6) } else { datum_to_cbor(vd, vt, &mut raw) }
    }
    raw.push(0xff);
    finish_raw(&raw)
}

pgrx::extension_sql!(
    r#"
CREATE FUNCTION cbor_build_array(VARIADIC "any") RETURNS cbor
    STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_build_array_wrapper';
CREATE FUNCTION cbor_build_array() RETURNS cbor
    STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_build_array_wrapper';
CREATE FUNCTION cbor_build_object(VARIADIC "any") RETURNS cbor
    STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_build_object_wrapper';
CREATE FUNCTION cbor_build_object() RETURNS cbor
    STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_build_object_wrapper';
"#,
    name = "cbor_builders",
    requires = ["cbor_type"],
);

fn push_text(raw: &mut Vec<u8>, s: &str) {
    write_head(raw, MAJOR_TEXT, s.len() as u64).or_raise(Source::Stored);
    raw.extend_from_slice(s.as_bytes());
}

/// `cbor_object(text[])`: alternating keys and values, or a two-column 2-D array.
#[pg_extern(immutable, strict, parallel_safe, name = "cbor_object")]
fn cbor_object_pairs(pairs: Array<'_, &str>) -> CborOut {
    let items: Vec<Option<&str>> = pairs.iter().collect();
    if !items.len().is_multiple_of(2) {
        fail(PgSqlErrorCode::ERRCODE_ARRAY_ELEMENT_ERROR, "array must have even number of elements");
    }
    let mut raw = vec![0xbf];
    for kv in items.chunks(2) {
        let Some(k) = kv[0] else { fail(PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED, "null value not allowed for object key") };
        push_text(&mut raw, k);
        match kv[1] {
            Some(v) => push_text(&mut raw, v),
            None => raw.push(0xf6),
        }
    }
    raw.push(0xff);
    finish_raw(&raw)
}

#[pg_extern(immutable, strict, parallel_safe, name = "cbor_object")]
fn cbor_object_keys_values(keys: Array<'_, &str>, values: Array<'_, &str>) -> CborOut {
    if keys.len() != values.len() {
        fail(PgSqlErrorCode::ERRCODE_ARRAY_ELEMENT_ERROR, "mismatched array dimensions");
    }
    let mut raw = vec![0xbf];
    for (k, v) in keys.iter().zip(values.iter()) {
        let Some(k) = k else { fail(PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED, "null value not allowed for object key") };
        push_text(&mut raw, k);
        match v {
            Some(v) => push_text(&mut raw, v),
            None => raw.push(0xf6),
        }
    }
    raw.push(0xff);
    finish_raw(&raw)
}

// ---- aggregates ----

/// Aggregate state: raw CBOR of an indefinite array or map under construction, in the aggregate memory context.
struct AggState {
    raw: PallocBuf,
}

/// The state from argument 0, created on the first call with `opener` as its first byte.
fn state(fcinfo: pg_sys::FunctionCallInfo, opener: u8) -> *mut AggState {
    let aggcx = agg_context(fcinfo);
    state_arg(fcinfo, 0).unwrap_or_else(|| new_state(aggcx, &[opener]))
}

/// Argument `n` as (datum, type, is_null).
fn arg(fcinfo: pg_sys::FunctionCallInfo, n: usize) -> (pg_sys::Datum, pg_sys::Oid, bool) {
    // SAFETY: `n` is within the declared argument count.
    unsafe {
        let a = pgrx::fcinfo::pg_get_nullable_datum(fcinfo, n);
        (a.value, pgrx::fcinfo::pg_getarg_type(fcinfo, n), a.isnull)
    }
}

fn append(st: *mut AggState, raw: &[u8]) {
    // SAFETY: `st` is the live aggregate state; growth reallocates within its memory context.
    unsafe { (*st).raw.write_bytes(raw).or_raise(Source::Stored) };
}

fn agg_trans(fcinfo: pg_sys::FunctionCallInfo, skip_nulls: bool) -> InternalOut {
    let st = state(fcinfo, 0x9f);
    let (d, t, null) = arg(fcinfo, 1);
    let mut raw = Vec::new();
    match null {
        true if skip_nulls => {}
        true => raw.push(0xf6),
        false => datum_to_cbor(d, t, &mut raw),
    }
    append(st, &raw);
    InternalOut(pg_sys::Datum::from(st))
}

#[pg_extern(sql = false)]
fn cbor_agg_transfn(fcinfo: pg_sys::FunctionCallInfo) -> InternalOut {
    agg_trans(fcinfo, false)
}

#[pg_extern(sql = false)]
fn cbor_agg_strict_transfn(fcinfo: pg_sys::FunctionCallInfo) -> InternalOut {
    agg_trans(fcinfo, true)
}

fn obj_trans(fcinfo: pg_sys::FunctionCallInfo, skip_nulls: bool) -> InternalOut {
    let st = state(fcinfo, 0xbf);
    let (kd, kt, knull) = arg(fcinfo, 1);
    let (vd, vt, vnull) = arg(fcinfo, 2);
    if knull {
        fail(PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED, "field name must not be null");
    }
    if !(vnull && skip_nulls) {
        let mut raw = Vec::new();
        datum_to_cbor(kd, kt, &mut raw);
        if vnull { raw.push(0xf6) } else { datum_to_cbor(vd, vt, &mut raw) }
        append(st, &raw);
    }
    InternalOut(pg_sys::Datum::from(st))
}

#[pg_extern(sql = false)]
fn cbor_object_agg_transfn(fcinfo: pg_sys::FunctionCallInfo) -> InternalOut {
    obj_trans(fcinfo, false)
}

#[pg_extern(sql = false)]
fn cbor_object_agg_strict_transfn(fcinfo: pg_sys::FunctionCallInfo) -> InternalOut {
    obj_trans(fcinfo, true)
}

/// New state in the aggregate memory context holding `raw` (opener byte included).
fn new_state(aggcx: pg_sys::MemoryContext, raw: &[u8]) -> *mut AggState {
    // SAFETY: allocates in the aggregate context; the struct is fully written before use.
    unsafe {
        PgMemoryContexts::For(aggcx).switch_to(|_| {
            let st = pg_sys::palloc(core::mem::size_of::<AggState>()) as *mut AggState;
            let mut buf = PallocBuf::cstring(raw.len());
            buf.write_bytes(raw).or_raise(Source::Stored);
            st.write(AggState { raw: buf });
            st
        })
    }
}

fn agg_context(fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::MemoryContext {
    let mut aggcx: pg_sys::MemoryContext = core::ptr::null_mut();
    // SAFETY: only asks the executor for the aggregate context.
    if unsafe { pg_sys::AggCheckCallContext(fcinfo, &mut aggcx) } == 0 {
        fail(PgSqlErrorCode::ERRCODE_INTERNAL_ERROR, "aggregate function called in non-aggregate context");
    }
    aggcx
}

/// State argument `n`, or None when NULL.
fn state_arg(fcinfo: pg_sys::FunctionCallInfo, n: usize) -> Option<*mut AggState> {
    // SAFETY: argument `n` is an aggregate state created by this module.
    unsafe {
        let a = pgrx::fcinfo::pg_get_nullable_datum(fcinfo, n);
        (!a.isnull).then(|| a.value.cast_mut_ptr::<AggState>())
    }
}

/// Parallel aggregation: appends the second partial state's items to the first.
#[pg_extern(sql = false)]
fn cbor_agg_combinefn(fcinfo: pg_sys::FunctionCallInfo) -> Option<InternalOut> {
    let aggcx = agg_context(fcinfo);
    let (a, b) = (state_arg(fcinfo, 0), state_arg(fcinfo, 1));
    let st = match (a, b) {
        (None, None) => return None,
        // SAFETY: states stay valid for the whole aggregation.
        (None, Some(b)) => new_state(aggcx, unsafe { (*b).raw.as_slice() }),
        (Some(a), None) => a,
        (Some(a), Some(b)) => {
            // Skip the second state's opener byte.
            let items = unsafe { (*b).raw.as_slice().get(1..).unwrap_or_default().to_vec() };
            append(a, &items);
            a
        }
    };
    Some(InternalOut(pg_sys::Datum::from(st)))
}

#[pg_extern(sql = false)]
fn cbor_agg_serialfn(fcinfo: pg_sys::FunctionCallInfo) -> crate::datum::ByteaOut {
    let st = state_arg(fcinfo, 0).unwrap_or_else(|| fail(PgSqlErrorCode::ERRCODE_INTERNAL_ERROR, "null aggregate state"));
    // SAFETY: the state stays valid for the whole aggregation.
    let raw = unsafe { (*st).raw.as_slice() };
    let mut out = PallocBuf::varlena(raw.len());
    out.write_bytes(raw).or_raise(Source::Stored);
    crate::datum::ByteaOut(crate::datum::VarlenaOut(out.into_varlena()))
}

#[pg_extern(sql = false)]
fn cbor_agg_deserialfn(fcinfo: pg_sys::FunctionCallInfo) -> InternalOut {
    let aggcx = agg_context(fcinfo);
    // SAFETY: argument 0 is the bytea produced by `cbor_agg_serialfn`.
    let raw = unsafe {
        let a = pgrx::fcinfo::pg_get_nullable_datum(fcinfo, 0);
        <&[u8]>::from_datum(a.value, a.isnull).unwrap_or_default()
    };
    InternalOut(pg_sys::Datum::from(new_state(aggcx, raw)))
}

/// Final function: normalizes a copy of the state, so window functions can call it repeatedly.
fn finish(fcinfo: pg_sys::FunctionCallInfo, dups: Duplicates) -> Option<CborOut> {
    // SAFETY: argument 0 is our state, or NULL when no row was aggregated.
    let st = unsafe {
        let a = pgrx::fcinfo::pg_get_nullable_datum(fcinfo, 0);
        if a.isnull {
            return None;
        }
        a.value.cast_mut_ptr::<AggState>()
    };
    // SAFETY: the state stays valid for the whole aggregation.
    let raw = unsafe { (*st).raw.as_slice() };
    let mut input = Vec::with_capacity(raw.len() + 1);
    input.extend_from_slice(raw);
    input.push(0xff);
    let mut out = PallocBuf::varlena(input.len());
    // Offsets into the aggregate's internal buffer mean nothing to the caller.
    datum::from_cbor_with(&input, &mut out, &mut PgCtx::new(), dups)
        .map_err(|e| postcbor_core::Error::new(e.kind, postcbor_core::error::NO_OFFSET))
        .or_raise(Source::Stored);
    Some(CborOut(out.into_varlena()))
}

#[pg_extern(sql = false)]
fn cbor_agg_finalfn(fcinfo: pg_sys::FunctionCallInfo) -> Option<CborOut> {
    finish(fcinfo, Duplicates::KeepLast)
}

#[pg_extern(sql = false)]
fn cbor_object_agg_unique_finalfn(fcinfo: pg_sys::FunctionCallInfo) -> Option<CborOut> {
    finish(fcinfo, Duplicates::Reject)
}

pgrx::extension_sql!(
    r#"
CREATE FUNCTION cbor_agg_transfn(internal, anyelement) RETURNS internal
    PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_agg_transfn_wrapper';
CREATE FUNCTION cbor_agg_strict_transfn(internal, anyelement) RETURNS internal
    PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_agg_strict_transfn_wrapper';
CREATE FUNCTION cbor_agg_finalfn(internal) RETURNS cbor
    PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_agg_finalfn_wrapper';
CREATE FUNCTION cbor_object_agg_transfn(internal, "any", "any") RETURNS internal
    PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_object_agg_transfn_wrapper';
CREATE FUNCTION cbor_object_agg_strict_transfn(internal, "any", "any") RETURNS internal
    PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_object_agg_strict_transfn_wrapper';
CREATE FUNCTION cbor_object_agg_unique_finalfn(internal) RETURNS cbor
    PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_object_agg_unique_finalfn_wrapper';
CREATE FUNCTION cbor_agg_combinefn(internal, internal) RETURNS internal
    PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_agg_combinefn_wrapper';
CREATE FUNCTION cbor_agg_serialfn(internal) RETURNS bytea
    STRICT PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_agg_serialfn_wrapper';
CREATE FUNCTION cbor_agg_deserialfn(bytea, internal) RETURNS internal
    STRICT PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_agg_deserialfn_wrapper';

CREATE AGGREGATE cbor_agg(anyelement) (SFUNC = cbor_agg_transfn, STYPE = internal, FINALFUNC = cbor_agg_finalfn,
    COMBINEFUNC = cbor_agg_combinefn, SERIALFUNC = cbor_agg_serialfn, DESERIALFUNC = cbor_agg_deserialfn, PARALLEL = SAFE);
CREATE AGGREGATE cbor_agg_strict(anyelement) (SFUNC = cbor_agg_strict_transfn, STYPE = internal, FINALFUNC = cbor_agg_finalfn,
    COMBINEFUNC = cbor_agg_combinefn, SERIALFUNC = cbor_agg_serialfn, DESERIALFUNC = cbor_agg_deserialfn, PARALLEL = SAFE);
CREATE AGGREGATE cbor_object_agg("any", "any") (SFUNC = cbor_object_agg_transfn, STYPE = internal, FINALFUNC = cbor_agg_finalfn,
    COMBINEFUNC = cbor_agg_combinefn, SERIALFUNC = cbor_agg_serialfn, DESERIALFUNC = cbor_agg_deserialfn, PARALLEL = SAFE);
CREATE AGGREGATE cbor_object_agg_strict("any", "any") (SFUNC = cbor_object_agg_strict_transfn, STYPE = internal, FINALFUNC = cbor_agg_finalfn,
    COMBINEFUNC = cbor_agg_combinefn, SERIALFUNC = cbor_agg_serialfn, DESERIALFUNC = cbor_agg_deserialfn, PARALLEL = SAFE);
CREATE AGGREGATE cbor_object_agg_unique("any", "any") (SFUNC = cbor_object_agg_transfn, STYPE = internal, FINALFUNC = cbor_object_agg_unique_finalfn,
    COMBINEFUNC = cbor_agg_combinefn, SERIALFUNC = cbor_agg_serialfn, DESERIALFUNC = cbor_agg_deserialfn, PARALLEL = SAFE);
CREATE AGGREGATE cbor_object_agg_unique_strict("any", "any") (SFUNC = cbor_object_agg_strict_transfn, STYPE = internal, FINALFUNC = cbor_object_agg_unique_finalfn,
    COMBINEFUNC = cbor_agg_combinefn, SERIALFUNC = cbor_agg_serialfn, DESERIALFUNC = cbor_agg_deserialfn, PARALLEL = SAFE);
"#,
    name = "cbor_aggregates",
    requires = ["cbor_type"],
);
