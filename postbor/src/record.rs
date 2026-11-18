//! `cbor_populate_record*` and `cbor_to_record*` (design §8): maps with text keys → rows.

use pgrx::callconv::{BoxRet, FcInfo};
use pgrx::datum::Datum as PgDatum;
use pgrx::pgrx_sql_entity_graph::metadata::{
    ArgumentError, ReturnsError, ReturnsRef, SqlMappingRef, SqlTranslatable, TypeOrigin,
};
use pgrx::prelude::*;
use pgrx::{PgTupleDesc, pg_sys};
use postbor_core::datum::Datum;
use postbor_core::num;
use postbor_core::ops::key_text;
use postbor_core::value::{Kind, Value};

use crate::access::value_out;
use crate::convert::{value_json_text, value_to_date, value_to_timestamptz, value_to_uuid};
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

/// Calls a type's input function on text.
fn input(text: &str, typ: pg_sys::Oid, typmod: i32) -> pg_sys::Datum {
    let cstr = std::ffi::CString::new(text)
        .unwrap_or_else(|_| fail(PgSqlErrorCode::ERRCODE_UNTRANSLATABLE_CHARACTER, "text contains a NUL character"));
    // SAFETY: catalog lookup and input-function call; errors are raised by PostgreSQL.
    unsafe {
        let mut func = pg_sys::InvalidOid;
        let mut ioparam = pg_sys::InvalidOid;
        pg_sys::getTypeInputInfo(typ, &mut func, &mut ioparam);
        pg_sys::OidInputFunctionCall(func, cstr.as_ptr() as *mut _, ioparam, typmod)
    }
}

/// Converts a value to a datum of `typ`; None for null.
fn convert(v: &Value, typ: pg_sys::Oid, typmod: i32) -> Option<pg_sys::Datum> {
    // SAFETY: catalog lookups only; datums are built by PostgreSQL functions.
    unsafe {
        pg_sys::check_stack_depth();
        let kind = v.kind().or_raise(Source::Stored);
        if matches!(kind, Kind::Null) {
            return None;
        }
        let base = pg_sys::getBaseType(typ);
        if base == cbor_oid() {
            return value_out(v).into_datum();
        }
        match base {
            pg_sys::JSONBOID | pg_sys::JSONOID => return Some(input(&value_json_text(v, false), typ, typmod)),
            pg_sys::TIMESTAMPTZOID | pg_sys::TIMESTAMPOID if matches!(kind, Kind::Tag(0 | 1)) => {
                let ts: pg_sys::TimestampTz = value_to_timestamptz(v).into();
                return Some(pg_sys::Datum::from(ts));
            }
            pg_sys::DATEOID if matches!(kind, Kind::Tag(100)) => {
                let d: pg_sys::DateADT = value_to_date(v).into();
                return Some(pg_sys::Datum::from(d));
            }
            pg_sys::UUIDOID if matches!(kind, Kind::Tag(37)) => return value_to_uuid(v).into_datum(),
            pg_sys::BYTEAOID => {
                if let Kind::Bytes(b) = kind {
                    return b.into_datum();
                }
            }
            _ => {}
        }
        let elem = pg_sys::get_element_type(base);
        if elem != pg_sys::InvalidOid
            && let Kind::Array(_) = kind {
                return Some(to_array(v, elem));
            }
        if pg_sys::type_is_rowtype(base)
            && let Kind::Map(_) = kind {
                let desc = pg_sys::lookup_rowtype_tupdesc_domain(typ, typmod, false);
                let d = map_to_tuple(v, desc, None);
                crate::datum::release_tupdesc(desc);
                return Some(d);
            }
        let text = match kind {
            Kind::Text(s) => s.to_string(),
            Kind::True => "true".into(),
            Kind::False => "false".into(),
            _ => match num::to_decimal(v).or_raise(Source::Stored) {
                Some(n) => n,
                None => {
                    let mut out = Vec::new();
                    postbor_core::edn::print(v, &mut out, Default::default()).or_raise(Source::Stored);
                    String::from_utf8(out).unwrap_or_default()
                }
            },
        };
        Some(input(&text, typ, typmod))
    }
}

/// A one-dimensional PostgreSQL array from a CBOR array.
unsafe fn to_array(v: &Value, elem: pg_sys::Oid) -> pg_sys::Datum {
    let items: Vec<Option<pg_sys::Datum>> =
        v.array_iter().or_raise(Source::Stored).map(|e| convert(&e.or_raise(Source::Stored), elem, -1)).collect();
    let mut values: Vec<pg_sys::Datum> = items.iter().map(|d| d.unwrap_or(pg_sys::Datum::from(0usize))).collect();
    let mut nulls: Vec<bool> = items.iter().map(Option::is_none).collect();
    let mut dims = [values.len() as i32];
    let mut lbs = [1i32];
    let (mut len, mut byval, mut align) = (0i16, false, 0 as core::ffi::c_char);
    // SAFETY: arrays sized to `dims`; element layout from the catalog.
    unsafe {
        pg_sys::get_typlenbyvalalign(elem, &mut len, &mut byval, &mut align);
        let a = pg_sys::construct_md_array(
            values.as_mut_ptr(),
            nulls.as_mut_ptr(),
            1,
            dims.as_mut_ptr(),
            lbs.as_mut_ptr(),
            elem,
            len.into(),
            byval,
            align,
        );
        pg_sys::Datum::from(a)
    }
}

/// A row of `desc` from a map: columns take the value under their name, else the base row's value, else NULL.
unsafe fn map_to_tuple(map: &Value, desc: pg_sys::TupleDesc, base: Option<pg_sys::HeapTupleHeader>) -> pg_sys::Datum {
    // SAFETY: `desc` is a pinned descriptor; `base` (when given) is a row of the same type.
    unsafe {
        let natts = (*desc).natts as usize;
        let mut values = vec![pg_sys::Datum::from(0usize); natts];
        let mut nulls = vec![true; natts];
        if let Some(td) = base {
            let mut tuple = pg_sys::HeapTupleData {
                t_len: crate::datum::varsize_4b(td.cast()),
                t_data: td,
                ..Default::default()
            };
            pg_sys::heap_deform_tuple(&mut tuple, desc, values.as_mut_ptr(), nulls.as_mut_ptr());
        }
        let attrs = PgTupleDesc::from_pg_unchecked(desc);
        let is_map = matches!(map.kind().or_raise(Source::Stored), Kind::Map(_));
        for i in 0..natts {
            let Some(a) = attrs.get(i) else { continue };
            if a.is_dropped() || !is_map {
                continue;
            }
            if let Some(v) = map.map_get(&key_text(a.name())).or_raise(Source::Stored) {
                match convert(&v, a.atttypid, a.atttypmod) {
                    Some(d) => {
                        values[i] = d;
                        nulls[i] = false;
                    }
                    None => nulls[i] = true,
                }
            }
        }
        let tuple = pg_sys::heap_form_tuple(desc, values.as_mut_ptr(), nulls.as_mut_ptr());
        pg_sys::HeapTupleHeaderGetDatum((*tuple).t_data)
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

#[pg_extern(sql = false)]
fn cbor_populate_record(fcinfo: pg_sys::FunctionCallInfo) -> RawOut {
    // SAFETY: called through the hand-written SQL below with (anyelement, cbor).
    unsafe {
        let (desc, base) = base_row(fcinfo);
        let Some(d) = cbor_arg(fcinfo, 1) else {
            crate::datum::release_tupdesc(desc);
            let a = pgrx::fcinfo::pg_get_nullable_datum(fcinfo, 0);
            return RawOut((!a.isnull).then_some(a.value));
        };
        let row = map_to_tuple(&Value::root(d), desc, base);
        crate::datum::release_tupdesc(desc);
        RawOut(Some(row))
    }
}

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
        RawOut(Some(map_to_tuple(&Value::root(d), desc, None)))
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
        let v = Value::root(d);
        if !matches!(v.kind().or_raise(Source::Stored), Kind::Array(_)) {
            fail(PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE, "cannot call this function on a non-array");
        }
        for e in v.array_iter().or_raise(Source::Stored) {
            let e = e.or_raise(Source::Stored);
            if !matches!(e.kind().or_raise(Source::Stored), Kind::Map(_)) {
                fail(PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE, "argument of cbor_populate_recordset must be an array of maps");
            }
            let row = map_to_tuple(&e, desc, base);
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
