//! `cbor_populate_record*` and `cbor_to_record*` (design §8): maps with text keys → rows.

use pgrx::callconv::{BoxRet, FcInfo};
use pgrx::datum::Datum as PgDatum;
use pgrx::pgrx_sql_entity_graph::metadata::{
    ArgumentError, ReturnsError, ReturnsRef, SqlMappingRef, SqlTranslatable, TypeOrigin,
};
use pgrx::prelude::*;
use pgrx::{PgTupleDesc, pg_sys};
use postcbor_core::datum::Datum;
use postcbor_core::num;
use postcbor_core::ops::key_text;
use postcbor_core::value::{Kind, Value};

use crate::access::value_out;
use crate::convert::{ConvError, TagTime, tag_time, value_json_text, value_to_date, value_to_uuid};
use crate::datum::{Cbor, cbor_oid};
use crate::error::{OrRaise, Source, fail};
use crate::io::parse_datum;

/// A raw datum result (row, set placeholder) for functions whose SQL is written by hand.
pub struct RawOut(Option<pg_sys::Datum>);

unsafe impl BoxRet for RawOut {
    unsafe fn box_into<'fcx>(self, fcinfo: &mut FcInfo<'fcx>) -> PgDatum<'fcx> {
        // SAFETY: the datum was built in the current memory context.
        unsafe { fcinfo.return_optional_datum(self.0) }
    }
}

unsafe impl SqlTranslatable for RawOut {
    const TYPE_IDENT: &'static str = pgrx::pgrx_resolved_type!(RawOut);
    const TYPE_ORIGIN: TypeOrigin = TypeOrigin::External;
    const ARGUMENT_SQL: Result<SqlMappingRef, ArgumentError> = Ok(SqlMappingRef::literal("internal"));
    const RETURN_SQL: Result<ReturnsRef, ReturnsError> = Ok(ReturnsRef::One(SqlMappingRef::literal("internal")));
}

/// Where conversion errors go: raised, or, for `cbor_populate_record_valid` (PostgreSQL 17+, which has soft domain
/// checks), recorded in a soft-error context.
struct Errs {
    #[cfg(any(feature = "pg17", feature = "pg18"))]
    soft: Option<pg_sys::ErrorSaveContext>,
}

/// A conversion that failed softly; the reason is in the error context.
struct Failed;

type Conv<T> = Result<T, Failed>;

impl Errs {
    fn raising() -> Self {
        Errs {
            #[cfg(any(feature = "pg17", feature = "pg18"))]
            soft: None,
        }
    }

    #[cfg(any(feature = "pg17", feature = "pg18"))]
    fn soft() -> Self {
        Errs { soft: Some(pg_sys::ErrorSaveContext { type_: pg_sys::NodeTag::T_ErrorSaveContext, error_occurred: false, details_wanted: false, error_data: core::ptr::null_mut() }) }
    }

    /// The context to pass PostgreSQL's soft-error functions; null raises.
    fn node(&mut self) -> *mut pg_sys::Node {
        #[cfg(any(feature = "pg17", feature = "pg18"))]
        if let Some(c) = &mut self.soft {
            return (c as *mut pg_sys::ErrorSaveContext).cast();
        }
        core::ptr::null_mut()
    }

    /// Raises the error, or records it.
    fn fail(&mut self, code: PgSqlErrorCode, msg: impl Into<String>) -> Failed {
        #[cfg(any(feature = "pg17", feature = "pg18"))]
        if let Some(c) = &mut self.soft {
            c.error_occurred = true;
            return Failed;
        }
        fail(code, msg)
    }

    fn conv(&mut self, e: ConvError) -> Failed {
        #[cfg(any(feature = "pg17", feature = "pg18"))]
        if let Some(c) = &mut self.soft {
            c.error_occurred = true;
            return Failed;
        }
        e.raise()
    }
}

/// Calls a type's input function on text.
fn input(text: &str, typ: pg_sys::Oid, typmod: i32, errs: &mut Errs) -> Conv<pg_sys::Datum> {
    let Ok(cstr) = std::ffi::CString::new(text) else { return Err(errs.fail(PgSqlErrorCode::ERRCODE_UNTRANSLATABLE_CHARACTER, "text contains a NUL character")) };
    // SAFETY: catalog lookup and input-function call; with a null context, errors are raised by PostgreSQL.
    unsafe {
        let mut func = pg_sys::InvalidOid;
        let mut ioparam = pg_sys::InvalidOid;
        pg_sys::getTypeInputInfo(typ, &mut func, &mut ioparam);
        let ctx = errs.node();
        if ctx.is_null() {
            return Ok(pg_sys::OidInputFunctionCall(func, cstr.as_ptr() as *mut _, ioparam, typmod));
        }
        #[cfg(any(feature = "pg17", feature = "pg18"))]
        {
            let mut finfo: pg_sys::FmgrInfo = core::mem::zeroed();
            pg_sys::fmgr_info(func, &mut finfo);
            let mut out = pg_sys::Datum::from(0usize);
            if pg_sys::InputFunctionCallSafe(&mut finfo, cstr.as_ptr() as *mut _, ioparam, typmod, ctx, &mut out) {
                return Ok(out);
            }
        }
        Err(Failed)
    }
}

/// Checks a domain's constraints, NOT NULL included.
fn domain_check(value: Option<pg_sys::Datum>, typ: pg_sys::Oid, errs: &mut Errs) -> Conv<()> {
    let mut extra: *mut core::ffi::c_void = core::ptr::null_mut();
    let d = value.unwrap_or(pg_sys::Datum::from(0usize));
    // SAFETY: `typ` is a domain and `d` a value of its base type (or NULL); with a null context, failures raise.
    unsafe {
        let ctx = errs.node();
        if ctx.is_null() {
            pg_sys::domain_check(d, value.is_none(), typ, &mut extra, pg_sys::CurrentMemoryContext);
            return Ok(());
        }
        #[cfg(any(feature = "pg17", feature = "pg18"))]
        if pg_sys::domain_check_safe(d, value.is_none(), typ, &mut extra, pg_sys::CurrentMemoryContext, ctx) {
            return Ok(());
        }
        Err(Failed)
    }
}

/// Converts a value to a datum of `typ`; None for null. A missing value (`v` None) is null, which domains check.
fn convert(v: Option<&Value>, typ: pg_sys::Oid, typmod: i32, errs: &mut Errs) -> Conv<Option<pg_sys::Datum>> {
    // SAFETY: stack check and catalog lookup.
    let (base, base_typmod) = unsafe {
        pg_sys::check_stack_depth();
        let mut t = typmod;
        (pg_sys::getBaseTypeAndTypmod(typ, &mut t), t)
    };
    let v = match v {
        Some(v) if !matches!(v.kind().or_raise(Source::Stored), Kind::Null) => Some(v),
        _ => None,
    };
    if base != typ {
        // A domain: convert to the base type, then check the constraints, NULL included.
        let d = match v {
            Some(v) => convert_base(v, base, base_typmod, errs)?,
            None => None,
        };
        domain_check(d, typ, errs)?;
        return Ok(d);
    }
    match v {
        Some(v) => convert_base(v, typ, typmod, errs),
        None => Ok(None),
    }
}

/// [`convert`] for a type that is not a domain, of a non-null value.
fn convert_base(v: &Value, typ: pg_sys::Oid, typmod: i32, errs: &mut Errs) -> Conv<Option<pg_sys::Datum>> {
    let kind = v.kind().or_raise(Source::Stored);
    if typ == cbor_oid() {
        return Ok(value_out(v).into_datum());
    }
    match typ {
        pg_sys::JSONBOID | pg_sys::JSONOID => return input(&value_json_text(v, false), typ, typmod, errs).map(Some),
        pg_sys::TIMESTAMPTZOID | pg_sys::TIMESTAMPOID if matches!(kind, Kind::Tag(0 | 1)) => {
            return match tag_time(v) {
                Ok(TagTime::Time(t)) => Ok(Some(pg_sys::Datum::from(pg_sys::TimestampTz::from(t)))),
                Ok(TagTime::Text(s)) => input(s, pg_sys::TIMESTAMPTZOID, -1, errs).map(Some),
                Err(e) => Err(errs.conv(e)),
            };
        }
        pg_sys::DATEOID if matches!(kind, Kind::Tag(100)) => {
            return value_to_date(v).map(|d| Some(pg_sys::Datum::from(pg_sys::DateADT::from(d)))).map_err(|e| errs.conv(e));
        }
        pg_sys::UUIDOID if matches!(kind, Kind::Tag(37)) => return value_to_uuid(v).map(|u| u.into_datum()).map_err(|e| errs.conv(e)),
        pg_sys::BYTEAOID => {
            if let Kind::Bytes(b) = kind {
                return Ok(b.into_datum());
            }
        }
        _ => {}
    }
    // SAFETY: catalog lookups only.
    let (elem, is_row) = unsafe { (pg_sys::get_element_type(typ), pg_sys::type_is_rowtype(typ)) };
    if elem != pg_sys::InvalidOid {
        match kind {
            Kind::Array(_) => return to_array(v, elem, errs).map(Some),
            // Text goes through the array's input function; anything else is not an array.
            Kind::Text(_) => {}
            _ => return Err(errs.fail(PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE, "expected an array")),
        }
    }
    if is_row && !matches!(kind, Kind::Text(_)) {
        // SAFETY: `typ` is a row type; the descriptor is released before returning.
        unsafe {
            let desc = pg_sys::lookup_rowtype_tupdesc_domain(typ, typmod, false);
            let d = map_to_tuple(v, desc, None, errs);
            crate::datum::release_tupdesc(desc);
            return d.map(Some);
        }
    }
    let text = match kind {
        Kind::Text(s) => s.to_string(),
        Kind::True => "true".into(),
        Kind::False => "false".into(),
        _ => match num::to_decimal(v).or_raise(Source::Stored) {
            Some(n) => n,
            None => {
                let mut out = Vec::new();
                postcbor_core::edn::print(v, &mut out, Default::default()).or_raise(Source::Stored);
                String::from_utf8(out).unwrap_or_default()
            }
        },
    };
    input(&text, typ, typmod, errs).map(Some)
}

/// A PostgreSQL array from a CBOR array, as jsonb's `populate_array`: nested arrays are dimensions, taken from the
/// first element at each level; every sub-array must match them.
fn to_array(v: &Value, elem: pg_sys::Oid, errs: &mut Errs) -> Conv<pg_sys::Datum> {
    const MAXDIM: usize = 6;
    let mut dims: Vec<usize> = Vec::new();
    let mut cur = *v;
    while let Kind::Array(n) = cur.kind().or_raise(Source::Stored) {
        dims.push(n as usize);
        if dims.len() > MAXDIM {
            return Err(errs.fail(PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED, format!("number of array dimensions ({}) exceeds the maximum allowed ({MAXDIM})", dims.len())));
        }
        match cur.array_get(0).or_raise(Source::Stored) {
            Some(first) => cur = first,
            None => break,
        }
    }
    let mut items: Vec<Option<pg_sys::Datum>> = Vec::new();
    // Depth-first over the sub-arrays; leaves are the elements of the innermost dimension.
    let mut stack = vec![(v.array_iter().or_raise(Source::Stored), 0usize)];
    while let Some((it, level)) = stack.last_mut() {
        let level = *level;
        let Some(e) = it.next() else {
            stack.pop();
            continue;
        };
        let e = e.or_raise(Source::Stored);
        if level + 1 == dims.len() {
            items.push(convert(Some(&e), elem, -1, errs)?);
            continue;
        }
        match e.kind().or_raise(Source::Stored) {
            Kind::Array(n) if n as usize == dims[level + 1] => stack.push((e.array_iter().or_raise(Source::Stored), level + 1)),
            Kind::Array(_) => {
                return Err(errs.fail(PgSqlErrorCode::ERRCODE_INVALID_TEXT_REPRESENTATION, "malformed array: sub-arrays must have matching dimensions"));
            }
            _ => return Err(errs.fail(PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE, "expected an array")),
        }
    }
    let mut values: Vec<pg_sys::Datum> = items.iter().map(|d| d.unwrap_or(pg_sys::Datum::from(0usize))).collect();
    let mut nulls: Vec<bool> = items.iter().map(Option::is_none).collect();
    // An empty innermost dimension (`[[]]`) is an empty array, as in jsonb.
    let ndims = if dims.contains(&0) { 0 } else { dims.len() };
    let mut pg_dims: Vec<i32> = dims.iter().map(|&d| d as i32).collect();
    let mut lbs = vec![1i32; dims.len()];
    let (mut len, mut byval, mut align) = (0i16, false, 0 as core::ffi::c_char);
    // SAFETY: `values` holds the product of `dims` elements; element layout from the catalog.
    unsafe {
        pg_sys::get_typlenbyvalalign(elem, &mut len, &mut byval, &mut align);
        let a = pg_sys::construct_md_array(values.as_mut_ptr(), nulls.as_mut_ptr(), ndims as i32, pg_dims.as_mut_ptr(), lbs.as_mut_ptr(), elem, len.into(), byval, align);
        Ok(pg_sys::Datum::from(a))
    }
}

/// A row of `desc` from a map, as jsonb's `populate_record`: columns take the value under their name; without it,
/// the base row's value, or NULL (which domains check) when there is no base row. A non-map fails.
unsafe fn map_to_tuple(map: &Value, desc: pg_sys::TupleDesc, base: Option<pg_sys::HeapTupleHeader>, errs: &mut Errs) -> Conv<pg_sys::Datum> {
    let n = match map.kind().or_raise(Source::Stored) {
        Kind::Map(n) => n,
        k => {
            let what = if matches!(k, Kind::Array(_)) { "an array" } else { "a scalar" };
            return Err(errs.fail(PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE, format!("cannot call populate_composite on {what}")));
        }
    };
    // SAFETY: `desc` is a pinned descriptor; `base` (when given) is a row of the same type.
    unsafe {
        if let (Some(td), 0) = (base, n) {
            return Ok(pg_sys::Datum::from(td));
        }
        let natts = (*desc).natts as usize;
        let mut values = vec![pg_sys::Datum::from(0usize); natts];
        let mut nulls = vec![true; natts];
        if let Some(td) = base {
            let mut tuple = pg_sys::HeapTupleData { t_len: crate::datum::varsize_4b(td.cast()), t_data: td, ..Default::default() };
            pg_sys::heap_deform_tuple(&mut tuple, desc, values.as_mut_ptr(), nulls.as_mut_ptr());
        }
        let attrs = PgTupleDesc::from_pg_unchecked(desc);
        for i in 0..natts {
            let Some(a) = attrs.get(i) else { continue };
            if a.is_dropped() {
                nulls[i] = true;
                continue;
            }
            let field = map.map_get(&key_text(a.name())).or_raise(Source::Stored);
            if base.is_some() && field.is_none() {
                continue;
            }
            match convert(field.as_ref(), a.atttypid, a.atttypmod, errs)? {
                Some(d) => {
                    values[i] = d;
                    nulls[i] = false;
                }
                None => nulls[i] = true,
            }
        }
        let tuple = pg_sys::heap_form_tuple(desc, values.as_mut_ptr(), nulls.as_mut_ptr());
        Ok(pg_sys::HeapTupleHeaderGetDatum((*tuple).t_data))
    }
}

/// Row descriptor of argument 0's type, and argument 0's row when not NULL.
unsafe fn base_row(fcinfo: pg_sys::FunctionCallInfo) -> (pg_sys::TupleDesc, Option<pg_sys::HeapTupleHeader>) {
    // SAFETY: argument 0 is declared anyelement; it must be a row type.
    unsafe {
        let typ = pg_sys::get_fn_expr_argtype((*fcinfo).flinfo, 0);
        if !pg_sys::type_is_rowtype(typ) {
            fail(PgSqlErrorCode::ERRCODE_DATATYPE_MISMATCH, "first argument must be a row type");
        }
        let a = pgrx::fcinfo::pg_get_nullable_datum(fcinfo, 0);
        let (base, typmod) = if a.isnull {
            (None, -1)
        } else {
            let td = pg_sys::pg_detoast_datum(a.value.cast_mut_ptr()) as pg_sys::HeapTupleHeader;
            (Some(td), (*td).t_choice.t_datum.datum_typmod)
        };
        (pg_sys::lookup_rowtype_tupdesc_domain(typ, typmod, false), base)
    }
}

/// The `cbor` argument `n`, or None when NULL.
unsafe fn cbor_arg<'a>(fcinfo: pg_sys::FunctionCallInfo, n: usize) -> Option<Datum<'a>> {
    // SAFETY: argument `n` is declared cbor.
    unsafe {
        let a = pgrx::fcinfo::pg_get_nullable_datum(fcinfo, n);
        if a.isnull {
            return None;
        }
        let c = Cbor::from_datum(a.value, false)?;
        Some(parse_datum(c))
    }
}

/// `cbor_populate_record`, raising or (for `_valid`) recording errors; None for a NULL result.
unsafe fn populate(fcinfo: pg_sys::FunctionCallInfo, errs: &mut Errs) -> Conv<Option<pg_sys::Datum>> {
    // SAFETY: called through the hand-written SQL below with (anyelement, cbor).
    unsafe {
        let (desc, base) = base_row(fcinfo);
        let Some(d) = cbor_arg(fcinfo, 1) else {
            crate::datum::release_tupdesc(desc);
            let a = pgrx::fcinfo::pg_get_nullable_datum(fcinfo, 0);
            return Ok((!a.isnull).then_some(a.value));
        };
        let row = map_to_tuple(&Value::root(&d), desc, base, errs);
        crate::datum::release_tupdesc(desc);
        row.map(Some)
    }
}

#[pg_extern(sql = false)]
fn cbor_populate_record(fcinfo: pg_sys::FunctionCallInfo) -> RawOut {
    // SAFETY: called through the hand-written SQL below.
    RawOut(unsafe { populate(fcinfo, &mut Errs::raising()) }.unwrap_or(None))
}

/// Whether `cbor_populate_record` would succeed on these arguments: conversion and domain errors make it false.
/// NULL when both arguments are NULL.
#[cfg(any(feature = "pg17", feature = "pg18"))]
#[pg_extern(sql = false)]
fn cbor_populate_record_valid(fcinfo: pg_sys::FunctionCallInfo) -> Option<bool> {
    // SAFETY: called through the hand-written SQL below with (anyelement, cbor).
    match unsafe { populate(fcinfo, &mut Errs::soft()) } {
        Ok(Some(_)) => Some(true),
        // A NULL base and a NULL value: NULL, as jsonb_populate_record_valid returns.
        Ok(None) => None,
        Err(Failed) => Some(false),
    }
}

#[cfg(any(feature = "pg17", feature = "pg18"))]
pgrx::extension_sql!(
    r#"
CREATE FUNCTION cbor_populate_record_valid(base anyelement, from_cbor cbor) RETURNS bool
    STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_populate_record_valid_wrapper';
"#,
    name = "cbor_populate_record_valid",
    requires = ["cbor_type"],
);

#[pg_extern(sql = false)]
fn cbor_to_record(fcinfo: pg_sys::FunctionCallInfo) -> RawOut {
    // SAFETY: the result type comes from the column definition list of the call.
    unsafe {
        let mut desc: pg_sys::TupleDesc = core::ptr::null_mut();
        if pg_sys::get_call_result_type(fcinfo, core::ptr::null_mut(), &mut desc) != pg_sys::TypeFuncClass::TYPEFUNC_COMPOSITE {
            fail(PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED, "a column definition list is required for functions returning \"record\"");
        }
        let desc = pg_sys::BlessTupleDesc(desc);
        let Some(d) = cbor_arg(fcinfo, 0) else { return RawOut(None) };
        RawOut(map_to_tuple(&Value::root(&d), desc, None, &mut Errs::raising()).ok())
    }
}

/// Fills the materialized result set with one row per array element.
unsafe fn recordset(fcinfo: pg_sys::FunctionCallInfo, arg: usize, base: Option<pg_sys::HeapTupleHeader>) {
    // SAFETY: InitMaterializedSRF sets up resultinfo with the expected descriptor and a tuplestore.
    unsafe {
        pg_sys::InitMaterializedSRF(fcinfo, 0);
        let rsi = (*fcinfo).resultinfo as *mut pg_sys::ReturnSetInfo;
        let desc = (*rsi).setDesc;
        let Some(d) = cbor_arg(fcinfo, arg) else { return };
        let v = Value::root(&d);
        if !matches!(v.kind().or_raise(Source::Stored), Kind::Array(_)) {
            fail(PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE, "cannot call this function on a non-array");
        }
        for e in v.array_iter().or_raise(Source::Stored) {
            let e = e.or_raise(Source::Stored);
            if !matches!(e.kind().or_raise(Source::Stored), Kind::Map(_)) {
                fail(PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE, "argument of cbor_populate_recordset must be an array of maps");
            }
            let Ok(row) = map_to_tuple(&e, desc, base, &mut Errs::raising()) else { return };
            let td = row.cast_mut_ptr::<pg_sys::HeapTupleHeaderData>();
            let mut tuple = pg_sys::HeapTupleData { t_len: crate::datum::varsize_4b(td.cast()), t_data: td, ..Default::default() };
            pg_sys::tuplestore_puttuple((*rsi).setResult, &mut tuple);
        }
    }
}

#[pg_extern(sql = false)]
fn cbor_populate_recordset(fcinfo: pg_sys::FunctionCallInfo) -> RawOut {
    // SAFETY: called through the hand-written SQL below with (anyelement, cbor).
    unsafe {
        let (desc, base) = base_row(fcinfo);
        crate::datum::release_tupdesc(desc);
        recordset(fcinfo, 1, base);
    }
    RawOut(None)
}

#[pg_extern(sql = false)]
fn cbor_to_recordset(fcinfo: pg_sys::FunctionCallInfo) -> RawOut {
    // SAFETY: called through the hand-written SQL below with (cbor).
    unsafe { recordset(fcinfo, 0, None) };
    RawOut(None)
}

pgrx::extension_sql!(
    r#"
CREATE FUNCTION cbor_populate_record(base anyelement, from_cbor cbor) RETURNS anyelement
    STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_populate_record_wrapper';
CREATE FUNCTION cbor_populate_recordset(base anyelement, from_cbor cbor) RETURNS SETOF anyelement
    STABLE PARALLEL SAFE ROWS 100 LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_populate_recordset_wrapper';
CREATE FUNCTION cbor_to_record(cbor) RETURNS record
    STABLE STRICT PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_to_record_wrapper';
CREATE FUNCTION cbor_to_recordset(cbor) RETURNS SETOF record
    STABLE STRICT PARALLEL SAFE ROWS 100 LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_to_recordset_wrapper';
"#,
    name = "cbor_records",
    requires = ["cbor_type"],
);

/// A pointer returned as `internal` (subscripting routines).
pub struct RawOutPtr(pub *mut core::ffi::c_void);

unsafe impl BoxRet for RawOutPtr {
    unsafe fn box_into<'fcx>(self, fcinfo: &mut FcInfo<'fcx>) -> PgDatum<'fcx> {
        // SAFETY: points at static data.
        unsafe { fcinfo.return_raw_datum(pg_sys::Datum::from(self.0)) }
    }
}

unsafe impl SqlTranslatable for RawOutPtr {
    const TYPE_IDENT: &'static str = pgrx::pgrx_resolved_type!(RawOutPtr);
    const TYPE_ORIGIN: TypeOrigin = TypeOrigin::External;
    const ARGUMENT_SQL: Result<SqlMappingRef, ArgumentError> = Ok(SqlMappingRef::literal("internal"));
    const RETURN_SQL: Result<ReturnsRef, ReturnsError> = Ok(ReturnsRef::One(SqlMappingRef::literal("internal")));
}
