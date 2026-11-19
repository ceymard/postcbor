//! `to_cbor` for any PostgreSQL type, and the casts between `cbor` and other types (design §8).

use core::ffi::CStr;

use pgrx::datum::{Date, TimestampWithTimeZone};
use pgrx::prelude::*;
use pgrx::{AnyElement, AnyNumeric, Uuid, pg_sys};
use postbor_core::cursor::{MAJOR_BYTES, MAJOR_NEGATIVE, MAJOR_TAG, MAJOR_TEXT, MAJOR_UNSIGNED};
use postbor_core::encode::write_head;
use postbor_core::normalize::Duplicates;
use postbor_core::value::{Kind, Value};
use postbor_core::{datum, edn, json, num};

use crate::datum::{ByteaOut, Cbor, CborOut, JsonArg, JsonOut, JsonbArg, JsonbOut, PallocBuf, cbor_oid};
use crate::error::{OrRaise, Source};
use crate::io::root;
use crate::io::{body_bytea, from_cbor_bytes, parse_datum};
use crate::settings::PgCtx;

/// Seconds between the Unix epoch and PostgreSQL's (2000-01-01).
const PG_EPOCH_SECS: i64 = 946_684_800;
const PG_EPOCH_DAYS: i64 = 10_957;


fn wrong_type(what: &str) -> ! {
    ereport!(ERROR, PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE, format!("cbor value is not {what}"));
}

fn out_of_range(what: &str) -> ! {
    ereport!(ERROR, PgSqlErrorCode::ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE, format!("cbor value out of range for {what}"));
}

/// Normalizes raw CBOR built by this module (duplicate keys keep the last, as jsonb does).
pub fn finish_raw(raw: &[u8]) -> CborOut {
    let mut out = PallocBuf::varlena(raw.len() + 1);
    datum::from_cbor_with(raw, &mut out, &mut PgCtx::new(), Duplicates::KeepLast)
        .map_err(|e| postbor_core::Error::new(e.kind, postbor_core::error::NO_OFFSET))
        .or_raise(Source::Stored);
    CborOut(out.into_varlena())
}

fn push_int(out: &mut Vec<u8>, i: i64) {
    let r = if i >= 0 { write_head(out, MAJOR_UNSIGNED, i as u64) } else { write_head(out, MAJOR_NEGATIVE, (-1 - i) as u64) };
    r.or_raise(Source::Stored);
}

fn push_text(out: &mut Vec<u8>, s: &str) {
    write_head(out, MAJOR_TEXT, s.len() as u64).or_raise(Source::Stored);
    out.extend_from_slice(s.as_bytes());
}

/// Epoch seconds of a PostgreSQL timestamp (microseconds since 2000-01-01), as tag 1 with a float.
fn push_timestamp(out: &mut Vec<u8>, micros: i64) {
    write_head(out, MAJOR_TAG, 1).or_raise(Source::Stored);
    let secs = match micros {
        i64::MIN => f64::NEG_INFINITY,
        i64::MAX => f64::INFINITY,
        m => (m as f64 + (PG_EPOCH_SECS * 1_000_000) as f64) / 1e6,
    };
    num::write_float(secs, out).or_raise(Source::Stored);
}

/// Appends raw CBOR for a PostgreSQL value. Recursive over arrays and composites, guarded by the stack check.
pub fn datum_to_cbor(d: pg_sys::Datum, oid: pg_sys::Oid, out: &mut Vec<u8>) {
    // SAFETY: plain catalog lookups and conversions on a datum of type `oid`, as handed over by PostgreSQL.
    unsafe {
        pg_sys::check_stack_depth();
        let oid = pg_sys::getBaseType(oid);
        match oid {
            pg_sys::BOOLOID => out.push(if bool::from_datum(d, false).unwrap_or(false) { 0xf5 } else { 0xf4 }),
            pg_sys::INT2OID => push_int(out, i64::from(i16::from_datum(d, false).unwrap_or(0))),
            pg_sys::INT4OID => push_int(out, i64::from(i32::from_datum(d, false).unwrap_or(0))),
            pg_sys::INT8OID => push_int(out, i64::from_datum(d, false).unwrap_or(0)),
            pg_sys::FLOAT4OID => num::write_float(f64::from(f32::from_datum(d, false).unwrap_or(0.0)), out).or_raise(Source::Stored),
            pg_sys::FLOAT8OID => num::write_float(f64::from_datum(d, false).unwrap_or(0.0), out).or_raise(Source::Stored),
            pg_sys::NUMERICOID => {
                let n = AnyNumeric::from_datum(d, false).unwrap_or_default();
                num::write_numeric(&n.to_string(), out).or_raise(Source::Stored);
            }
            pg_sys::BYTEAOID => {
                let b = <&[u8]>::from_datum(d, false).unwrap_or_default();
                write_head(out, MAJOR_BYTES, b.len() as u64).or_raise(Source::Stored);
                out.extend_from_slice(b);
            }
            pg_sys::TEXTOID | pg_sys::VARCHAROID | pg_sys::BPCHAROID => {
                push_text(out, <&str>::from_datum(d, false).unwrap_or_default());
            }
            pg_sys::TIMESTAMPTZOID | pg_sys::TIMESTAMPOID => push_timestamp(out, d.value() as i64),
            pg_sys::DATEOID => {
                let days = d.value() as i32;
                write_head(out, MAJOR_TAG, 100).or_raise(Source::Stored);
                match days {
                    i32::MIN => num::write_float(f64::NEG_INFINITY, out).or_raise(Source::Stored),
                    i32::MAX => num::write_float(f64::INFINITY, out).or_raise(Source::Stored),
                    _ => push_int(out, i64::from(days) + PG_EPOCH_DAYS),
                }
            }
            pg_sys::UUIDOID => {
                let u = Uuid::from_datum(d, false).unwrap_or_else(|| Uuid::from_bytes([0; 16]));
                out.extend_from_slice(&[0xd8, 37, 0x50]);
                out.extend_from_slice(&u[..]);
            }
            pg_sys::JSONBOID | pg_sys::JSONOID => {
                let text = output_text(d, oid);
                edn::parse::parse(text.as_bytes(), out, &mut PgCtx::new()).or_raise(Source::Text);
            }
            o if o == cbor_oid() => {
                let c = Cbor::from_datum(d, false).unwrap_or(crate::datum::cbor_bytes(&[]));
                out.extend_from_slice(parse_datum(c).body);
            }
            o if pg_sys::get_element_type(o) != pg_sys::InvalidOid => array_to_cbor(d, o, out),
            o if pg_sys::type_is_rowtype(o) => composite_to_cbor(d, out),
            o => push_text(out, &output_text(d, o)),
        }
    }
}

/// Text from the type's output function.
pub fn output_text(d: pg_sys::Datum, oid: pg_sys::Oid) -> String {
    // SAFETY: output functions take the datum and return a palloc'd C string.
    unsafe {
        let mut func = pg_sys::InvalidOid;
        let mut varlena = false;
        pg_sys::getTypeOutputInfo(oid, &mut func, &mut varlena);
        let p = pg_sys::OidOutputFunctionCall(func, d);
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// Arrays: nested CBOR arrays for each dimension; NULL elements become null.
fn array_to_cbor(d: pg_sys::Datum, oid: pg_sys::Oid, out: &mut Vec<u8>) {
    // SAFETY: `d` is an array datum of type `oid`; deconstruct_array returns `n` element datums and null flags.
    unsafe {
        let arr = pg_sys::pg_detoast_datum(d.cast_mut_ptr()) as *mut pg_sys::ArrayType;
        let elem = pg_sys::get_element_type(oid);
        let raw = pgrx::array::RawArray::from_ptr(core::ptr::NonNull::new_unchecked(arr));
        let dims: Vec<usize> = raw.dims().iter().map(|&x| x as usize).collect();
        let (mut len, mut byval, mut align) = (0i16, false, 0 as core::ffi::c_char);
        pg_sys::get_typlenbyvalalign(elem, &mut len, &mut byval, &mut align);
        let mut values: *mut pg_sys::Datum = core::ptr::null_mut();
        let mut nulls: *mut bool = core::ptr::null_mut();
        let mut n = 0;
        pg_sys::deconstruct_array(arr, elem, len.into(), byval, align, &mut values, &mut nulls, &mut n);
        let n = n as usize;
        let values = core::slice::from_raw_parts(values, n);
        let nulls = core::slice::from_raw_parts(nulls, n);
        if dims.is_empty() {
            out.push(0x80);
            return;
        }
        let mut idx = 0;
        nested(&dims, values, nulls, elem, &mut idx, out);
    }
}

fn nested(dims: &[usize], values: &[pg_sys::Datum], nulls: &[bool], elem: pg_sys::Oid, idx: &mut usize, out: &mut Vec<u8>) {
    let Some((&n, rest)) = dims.split_first() else { return };
    write_head(out, postbor_core::cursor::MAJOR_ARRAY, n as u64).or_raise(Source::Stored);
    for _ in 0..n {
        if rest.is_empty() {
            match (values.get(*idx), nulls.get(*idx)) {
                (Some(_), Some(true)) | (None, _) => out.push(0xf6),
                (Some(v), _) => datum_to_cbor(*v, elem, out),
            }
            *idx += 1;
        } else {
            nested(rest, values, nulls, elem, idx, out);
        }
    }
}

/// Composites and records: a map from column name to value.
fn composite_to_cbor(d: pg_sys::Datum, out: &mut Vec<u8>) {
    // SAFETY: `d` is a composite datum; its row type gives the tuple descriptor used to deform it.
    unsafe {
        let td = pg_sys::pg_detoast_datum(d.cast_mut_ptr()) as pg_sys::HeapTupleHeader;
        let typid = (*td).t_choice.t_datum.datum_typeid;
        let typmod = (*td).t_choice.t_datum.datum_typmod;
        let desc = pg_sys::lookup_rowtype_tupdesc(typid, typmod);
        let natts = (*desc).natts as usize;
        let mut tuple = pg_sys::HeapTupleData {
            t_len: crate::datum::varsize_4b(td.cast()),
            t_data: td,
            ..Default::default()
        };
        let mut values = vec![pg_sys::Datum::from(0usize); natts];
        let mut nulls = vec![false; natts];
        pg_sys::heap_deform_tuple(&mut tuple, desc, values.as_mut_ptr(), nulls.as_mut_ptr());
        let attrs = pgrx::PgTupleDesc::from_pg_unchecked(desc);
        let live: Vec<usize> = (0..natts).filter(|&i| attrs.get(i).is_some_and(|a| !a.is_dropped())).collect();
        out.push(0xbf);
        for i in live {
            let Some(a) = attrs.get(i) else { continue };
            push_text(out, a.name());
            if nulls[i] {
                out.push(0xf6);
            } else {
                datum_to_cbor(values[i], a.atttypid, out);
            }
        }
        out.push(0xff);
        crate::datum::release_tupdesc(desc);
    }
}

// Stable, like to_jsonb: other types go through their output functions, which may depend on settings.
#[pg_extern(stable, strict, parallel_safe)]
fn to_cbor(value: AnyElement) -> CborOut {
    let mut raw = Vec::new();
    datum_to_cbor(value.datum(), value.oid(), &mut raw);
    finish_raw(&raw)
}

// ---- casts from cbor ----

#[pg_cast(immutable, parallel_safe)]
fn cbor_to_bool(c: Cbor<'_>) -> bool {
    match root!(c).kind().or_raise(Source::Stored) {
        Kind::True => true,
        Kind::False => false,
        _ => wrong_type("a boolean"),
    }
}

fn as_i64(c: Cbor<'_>, what: &str) -> i64 {
    match root!(c).kind().or_raise(Source::Stored) {
        Kind::Unsigned(n) => i64::try_from(n).unwrap_or_else(|_| out_of_range(what)),
        Kind::Negative(n) => i64::try_from(n).map(|n| -1 - n).unwrap_or_else(|_| out_of_range(what)),
        Kind::Tag(2 | 3) => out_of_range(what),
        _ => wrong_type("an integer"),
    }
}

#[pg_cast(immutable, parallel_safe)]
fn cbor_to_int2(c: Cbor<'_>) -> i16 {
    i16::try_from(as_i64(c, "smallint")).unwrap_or_else(|_| out_of_range("smallint"))
}

#[pg_cast(immutable, parallel_safe)]
fn cbor_to_int4(c: Cbor<'_>) -> i32 {
    i32::try_from(as_i64(c, "integer")).unwrap_or_else(|_| out_of_range("integer"))
}

#[pg_cast(immutable, parallel_safe)]
fn cbor_to_int8(c: Cbor<'_>) -> i64 {
    as_i64(c, "bigint")
}

fn as_f64(c: Cbor<'_>) -> f64 {
    match root!(c).kind().or_raise(Source::Stored) {
        Kind::Unsigned(n) => n as f64,
        Kind::Negative(n) => -1.0 - n as f64,
        Kind::Float(f) => f,
        _ => wrong_type("a number"),
    }
}

#[pg_cast(immutable, parallel_safe)]
fn cbor_to_float4(c: Cbor<'_>) -> f32 {
    as_f64(c) as f32
}

#[pg_cast(immutable, parallel_safe)]
fn cbor_to_float8(c: Cbor<'_>) -> f64 {
    as_f64(c)
}

#[pg_cast(immutable, parallel_safe)]
fn cbor_to_numeric(c: Cbor<'_>) -> AnyNumeric {
    let text = num::to_decimal(&root!(c)).or_raise(Source::Stored).unwrap_or_else(|| wrong_type("a number"));
    AnyNumeric::try_from(text.as_str()).unwrap_or_else(|_| out_of_range("numeric"))
}

#[pg_cast(stable, parallel_safe)]
fn cbor_to_timestamptz(c: Cbor<'_>) -> TimestampWithTimeZone {
    value_to_timestamptz(&root!(c))
}

/// Tag 0 (RFC 3339 text) or tag 1 (epoch seconds) as a timestamptz.
pub fn value_to_timestamptz(v: &Value) -> TimestampWithTimeZone {
    let Kind::Tag(t @ (0 | 1)) = v.kind().or_raise(Source::Stored) else { wrong_type("a date-time (tag 0 or 1)") };
    let content = v.tag_content().or_raise(Source::Stored);
    let k = content.kind().or_raise(Source::Stored);
    let micros: i64 = match (t, k) {
        (0, Kind::Text(s)) => {
            let cs = std::ffi::CString::new(s).unwrap_or_else(|_| wrong_type("a date-time"));
            // SAFETY: timestamptz_in parses a C string; errors are raised by PostgreSQL.
            let ts = unsafe {
                pgrx::fcinfo::direct_function_call::<TimestampWithTimeZone>(
                    pg_sys::timestamptz_in,
                    &[cs.as_c_str().into_datum(), pg_sys::InvalidOid.into_datum(), (-1i32).into_datum()],
                )
            };
            return ts.unwrap_or_else(|| wrong_type("a date-time"));
        }
        (1, Kind::Unsigned(n)) => i64::try_from(n).ok().and_then(|s| (s - PG_EPOCH_SECS).checked_mul(1_000_000)).unwrap_or_else(|| out_of_range("timestamptz")),
        (1, Kind::Negative(n)) => i64::try_from(n).ok().and_then(|n| (-1 - n - PG_EPOCH_SECS).checked_mul(1_000_000)).unwrap_or_else(|| out_of_range("timestamptz")),
        (1, Kind::Float(f)) if f.is_finite() => {
            let m = ((f - PG_EPOCH_SECS as f64) * 1e6).round();
            if !(-9.2e18..9.2e18).contains(&m) {
                out_of_range("timestamptz");
            }
            m as i64
        }
        (1, Kind::Float(f)) if f.is_infinite() => if f > 0.0 { i64::MAX } else { i64::MIN },
        _ => wrong_type("a date-time"),
    };
    TimestampWithTimeZone::try_from(micros).unwrap_or_else(|_| out_of_range("timestamptz"))
}

#[pg_cast(immutable, parallel_safe)]
fn cbor_to_date(c: Cbor<'_>) -> Date {
    value_to_date(&root!(c))
}

/// Tag 100 (days since 1970-01-01) as a date.
pub fn value_to_date(v: &Value) -> Date {
    let Kind::Tag(100) = v.kind().or_raise(Source::Stored) else { wrong_type("a date (tag 100)") };
    let days = match v.tag_content().or_raise(Source::Stored).kind().or_raise(Source::Stored) {
        Kind::Unsigned(n) => i64::try_from(n).unwrap_or(i64::MAX),
        Kind::Negative(n) => i64::try_from(n).map(|n| -1 - n).unwrap_or(i64::MIN),
        Kind::Float(f) if f.is_infinite() => if f > 0.0 { i64::from(i32::MAX) + PG_EPOCH_DAYS } else { i64::from(i32::MIN) + PG_EPOCH_DAYS },
        _ => wrong_type("a date"),
    };
    let pg = i32::try_from(days - PG_EPOCH_DAYS).unwrap_or_else(|_| out_of_range("date"));
    Date::try_from(pg).unwrap_or_else(|_| out_of_range("date"))
}

#[pg_cast(immutable, parallel_safe)]
fn cbor_to_uuid(c: Cbor<'_>) -> Uuid {
    value_to_uuid(&root!(c))
}

/// Tag 37 (16 bytes) as a uuid.
pub fn value_to_uuid(v: &Value) -> Uuid {
    let Kind::Tag(37) = v.kind().or_raise(Source::Stored) else { wrong_type("a UUID (tag 37)") };
    match v.tag_content().or_raise(Source::Stored).kind().or_raise(Source::Stored) {
        Kind::Bytes(b) => Uuid::from_slice(b).unwrap_or_else(|_| wrong_type("a 16-byte UUID")),
        _ => wrong_type("a UUID"),
    }
}

#[pg_cast(immutable, parallel_safe)]
fn cbor_to_bytea(c: Cbor<'_>) -> ByteaOut {
    ByteaOut(body_bytea(&parse_datum(c)))
}

#[pg_cast(immutable, parallel_safe)]
fn bytea_to_cbor(b: &[u8]) -> CborOut {
    from_cbor_bytes(b, Source::Binary)
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_from_bytea(b: &[u8]) -> CborOut {
    from_cbor_bytes(b, Source::Binary)
}

fn json_text(c: Cbor<'_>, strict: bool) -> String {
    value_json_text(&root!(c), strict)
}

/// JSON text of a value (lossy unless `strict`).
pub fn value_json_text(v: &Value, strict: bool) -> String {
    let mut out = Vec::new();
    json::print(v, &mut out, strict).or_raise(Source::Stored);
    String::from_utf8(out).unwrap_or_default()
}

/// Runs a type input function on JSON text.
fn json_in(text: &str, input: unsafe fn(pg_sys::FunctionCallInfo) -> pg_sys::Datum) -> pg_sys::Datum {
    let cs = std::ffi::CString::new(text).unwrap_or_else(|_| wrong_type("convertible to JSON"));
    // SAFETY: json_in/jsonb_in parse a C string and return a datum in the current memory context.
    unsafe { pgrx::fcinfo::direct_function_call_as_datum(input, &[cs.as_c_str().into_datum()]) }
        .unwrap_or_else(|| wrong_type("convertible to JSON"))
}

#[pg_cast(immutable, parallel_safe)]
fn cbor_to_jsonb(c: Cbor<'_>) -> JsonbOut {
    JsonbOut(json_in(&json_text(c, false), pg_sys::jsonb_in))
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_to_jsonb_strict(c: Cbor<'_>) -> JsonbOut {
    JsonbOut(json_in(&json_text(c, true), pg_sys::jsonb_in))
}

#[pg_cast(immutable, parallel_safe)]
fn cbor_to_json(c: Cbor<'_>) -> JsonOut {
    JsonOut(json_in(&json_text(c, false), pg_sys::json_in))
}

fn from_json_text(text: &str) -> CborOut {
    let mut out = PallocBuf::varlena(text.len());
    edn::to_datum_with(text.as_bytes(), &mut out, &mut PgCtx::new(), Duplicates::KeepLast).or_raise(Source::Text);
    CborOut(out.into_varlena())
}

#[pg_cast(immutable, parallel_safe)]
fn jsonb_to_cbor(j: JsonbArg) -> CborOut {
    from_json_text(&output_text(j.0, pg_sys::JSONBOID))
}

#[pg_cast(immutable, parallel_safe)]
fn json_to_cbor(j: JsonArg) -> CborOut {
    from_json_text(&output_text(j.0, pg_sys::JSONOID))
}
