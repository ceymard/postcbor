//! Path queries over `cbor` with PostgreSQL's `jsonpath` (design §8 Path queries): the SQL functions and
//! operators, and the host the core evaluator calls for `numeric` arithmetic, input functions and regexes.

use core::ffi::{c_char, c_int};

use pgrx::iter::SetOfIterator;
use pgrx::prelude::*;
use postbor_core::Ctx;
use postbor_core::datum::Datum;
use postbor_core::decimal::Num;
use postbor_core::jsonpath::eval::{self, Code, Host, JItem, PathError, Res};
use postbor_core::jsonpath::{BinOp, Path};
use postbor_core::value::Value;

use crate::datum::{Cbor, CborOut, JsonPathArg, PallocBuf};
use crate::error::{OrRaise, Source, fail, raise};
use crate::io::parse_datum;
use crate::settings::PgCtx;

/// Debug builds only: the decoded path printed back as text, so tests can check that decoding loses nothing.
#[cfg(debug_assertions)]
#[pg_extern(immutable, strict, parallel_safe)]
fn postbor_jsonpath_print(path: JsonPathArg<'_>) -> String {
    let p = Path::decode(path.0).or_raise(Source::Stored);
    postbor_core::jsonpath::print::print(&p)
}

// PostgreSQL functions pgrx does not bind, or binds only as wrappers (input functions are passed by address).
unsafe extern "C-unwind" {
    fn RE_compile_and_execute(text_re: *mut pg_sys::text, dat: *mut c_char, dat_len: c_int, cflags: c_int, collation: pg_sys::Oid, nmatch: c_int, pmatch: *mut core::ffi::c_void) -> bool;
    fn int4in(fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum;
    fn int8in(fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum;
    fn numeric_in(fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum;
}

// regex.h's compile flags.
const REG_ADVANCED: c_int = 0o3;
const REG_QUOTE: c_int = 0o4;
const REG_ICASE: c_int = 0o10;
const REG_NLSTOP: c_int = 0o100;
const REG_NLANCH: c_int = 0o200;

struct PgHost;

/// A `numeric` datum in the current memory context.
fn to_numeric(n: &Num) -> pg_sys::Numeric {
    let mut buf = PallocBuf::raw(16);
    n.write_numeric(&mut buf).or_raise(Source::Stored);
    buf.into_ptr().cast()
}

fn from_numeric(p: pg_sys::Numeric) -> Num {
    // SAFETY: numeric functions return an untoasted value with a 4-byte header.
    let bytes = unsafe { core::slice::from_raw_parts(p.cast::<u8>(), crate::datum::varsize_4b(p.cast()) as usize) };
    Num::from_numeric(bytes).or_raise(Source::Stored)
}

/// A NUL-terminated copy in the current memory context.
fn cstring(s: &str) -> *mut c_char {
    let mut buf = PallocBuf::cstring(s.len());
    postbor_core::OutBuf::write_bytes(&mut buf, s.as_bytes()).or_raise(Source::Stored);
    buf.into_cstr().as_ptr().cast_mut()
}

#[cfg(not(feature = "pg15"))]
fn soft_context() -> pg_sys::ErrorSaveContext {
    pg_sys::ErrorSaveContext { type_: pg_sys::NodeTag::T_ErrorSaveContext, error_occurred: false, details_wanted: false, error_data: core::ptr::null_mut() }
}

/// Calls an input function, reporting failure instead of raising (`DirectInputFunctionCallSafe`).
#[cfg(not(feature = "pg15"))]
fn input_safe(f: unsafe extern "C-unwind" fn(pg_sys::FunctionCallInfo) -> pg_sys::Datum, s: &str, typmod: i32) -> Option<pg_sys::Datum> {
    let mut es = soft_context();
    let mut out = pg_sys::Datum::from(0usize);
    // SAFETY: `f` is an input function taking a C string; the error context outlives the call.
    let ok = unsafe { pg_sys::DirectInputFunctionCallSafe(Some(f), cstring(s), pg_sys::InvalidOid, typmod, (&raw mut es).cast(), &mut out) };
    (ok && !es.error_occurred).then_some(out)
}

impl Host for PgHost {
    fn arith(&mut self, op: BinOp, a: &Num, b: &Num, throw: bool) -> Option<Num> {
        let f = match op {
            BinOp::Add => pg_sys::numeric_add_opt_error,
            BinOp::Sub => pg_sys::numeric_sub_opt_error,
            BinOp::Mul => pg_sys::numeric_mul_opt_error,
            BinOp::Div => pg_sys::numeric_div_opt_error,
            _ => pg_sys::numeric_mod_opt_error,
        };
        let mut error = false;
        let err: *mut bool = if throw { core::ptr::null_mut() } else { &mut error };
        // SAFETY: both arguments are valid numerics; a null error pointer makes failures raise.
        let r = unsafe { f(to_numeric(a), to_numeric(b), err) };
        (!error).then(|| from_numeric(r))
    }

    fn parse_float8(&mut self, s: &str) -> Option<f64> {
        let c = cstring(s);
        let name = c"double precision".as_ptr();
        #[cfg(feature = "pg15")]
        {
            let mut error = false;
            // SAFETY: `c` is a NUL-terminated string in palloc memory.
            let f = unsafe { pg_sys::float8in_internal_opt_error(c, core::ptr::null_mut(), name, c, &mut error) };
            (!error).then_some(f)
        }
        #[cfg(not(feature = "pg15"))]
        {
            let mut es = soft_context();
            // SAFETY: as above; the error context outlives the call.
            let f = unsafe { pg_sys::float8in_internal(c, core::ptr::null_mut(), name, c, (&raw mut es).cast()) };
            (!es.error_occurred).then_some(f)
        }
    }

    fn float8_numeric(&mut self, f: f64) -> Num {
        // SAFETY: float8_numeric takes one float8 and returns a numeric.
        let d = unsafe { pgrx::direct_function_call::<pg_sys::Datum>(pg_sys::float8_numeric, &[f.into_datum()]) };
        from_numeric(d.map(|d| d.cast_mut_ptr()).unwrap_or(core::ptr::null_mut()))
    }

    fn parse_int(&mut self, s: &str, bits64: bool) -> Option<i64> {
        #[cfg(feature = "pg15")]
        {
            let _ = (s, bits64);
            None
        }
        #[cfg(not(feature = "pg15"))]
        {
            let d = input_safe(if bits64 { int8in } else { int4in }, s, -1)?;
            Some(if bits64 { d.value() as i64 } else { i64::from(d.value() as i32) })
        }
    }

    fn parse_bool(&mut self, s: &str) -> Option<bool> {
        let mut b = false;
        // SAFETY: a NUL-terminated string in palloc memory.
        unsafe { pg_sys::parse_bool(cstring(s), &mut b) }.then_some(b)
    }

    fn parse_numeric(&mut self, s: &str, typmod: i32) -> Option<Num> {
        #[cfg(feature = "pg15")]
        {
            let _ = (s, typmod);
            None
        }
        #[cfg(not(feature = "pg15"))]
        {
            input_safe(numeric_in, s, typmod).map(|d| from_numeric(d.cast_mut_ptr()))
        }
    }

    fn numeric_typmod(&mut self, precision: i32, scale: i32, throw: bool) -> Option<i32> {
        #[cfg(any(feature = "pg15", feature = "pg16"))]
        {
            let _ = (precision, scale, throw);
            None
        }
        #[cfg(not(any(feature = "pg15", feature = "pg16")))]
        {
            let mut es = soft_context();
            let ctx: *mut pg_sys::Node = if throw { core::ptr::null_mut() } else { (&raw mut es).cast() };
            // SAFETY: a null context makes invalid values raise; otherwise the context outlives the call.
            let t = unsafe { pg_sys::make_numeric_typmod_safe(precision, scale, ctx) };
            (!es.error_occurred).then_some(t)
        }
    }

    fn regex_match(&mut self, pattern: &[u8], flags: u32, s: &[u8]) -> bool {
        // jspConvertRegexFlags; the path parser already refused the unimplemented `x`.
        let mut cflags = REG_ADVANCED;
        if flags & 0x01 != 0 {
            cflags |= REG_ICASE;
        }
        if flags & 0x10 != 0 {
            cflags = (cflags & !REG_ADVANCED) | REG_QUOTE;
        } else {
            if flags & 0x02 == 0 {
                cflags |= REG_NLSTOP;
            }
            if flags & 0x04 != 0 {
                cflags |= REG_NLANCH;
            }
        }
        let mut re = PallocBuf::varlena(pattern.len());
        postbor_core::OutBuf::write_bytes(&mut re, pattern).or_raise(Source::Stored);
        let re = re.into_varlena();
        // SAFETY: `re` is a text varlena; `s` is valid for its length and only read; the guard turns a raised
        // error (invalid pattern) into a Rust panic.
        unsafe { pg_sys::ffi::pg_guard_ffi_boundary(|| RE_compile_and_execute(re.cast(), s.as_ptr().cast_mut().cast(), s.len() as c_int, cflags, pg_sys::DEFAULT_COLLATION_OID, 0, core::ptr::null_mut())) }
    }

    fn check_stack(&mut self) {
        // SAFETY: raises an error when the stack is too deep.
        unsafe { pg_sys::check_stack_depth() };
    }
}

fn raise_path(e: PathError) -> ! {
    use PgSqlErrorCode::*;
    let (code, msg) = match e {
        PathError::Core(e) => match e.kind {
            postbor_core::ErrorKind::InvalidArgument(m) => (ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE, m.to_string()),
            _ => raise(e, Source::Stored),
        },
        PathError::Sql(code, msg) => (
            match code {
                Code::ArrayNotFound => ERRCODE_SQL_JSON_ARRAY_NOT_FOUND,
                Code::ObjectNotFound => ERRCODE_SQL_JSON_OBJECT_NOT_FOUND,
                Code::MemberNotFound => ERRCODE_SQL_JSON_MEMBER_NOT_FOUND,
                Code::InvalidSubscript => ERRCODE_INVALID_SQL_JSON_SUBSCRIPT,
                Code::SingletonRequired => ERRCODE_SINGLETON_SQL_JSON_ITEM_REQUIRED,
                Code::NonNumericItem => ERRCODE_NON_NUMERIC_SQL_JSON_ITEM,
                Code::NumberNotFound => ERRCODE_SQL_JSON_NUMBER_NOT_FOUND,
                Code::UndefinedObject => ERRCODE_UNDEFINED_OBJECT,
                Code::InvalidParameter => ERRCODE_INVALID_PARAMETER_VALUE,
                Code::NotSupported => ERRCODE_FEATURE_NOT_SUPPORTED,
                Code::Internal => ERRCODE_INTERNAL_ERROR,
            },
            msg,
        ),
    };
    fail(code, msg)
}

/// Runs a path; `vars` None is the operators' form (no variables, `$x` raises).
fn run<'a>(path: &'a Path<'a>, doc: &'a Datum<'a>, vars: Option<&'a Datum<'a>>, silent: bool, found: Option<&mut Vec<JItem<'a>>>) -> Res {
    eval::execute(path, Value::root(doc), vars.map(Value::root), !silent, &mut PgHost, found).unwrap_or_else(|e| raise_path(e))
}

fn out(it: &JItem) -> CborOut {
    let mut buf = PallocBuf::varlena(32);
    eval::write_datum(it, &mut buf, PgCtx::new().limits().max_depth).or_raise(Source::Stored);
    CborOut(buf.into_varlena())
}

fn exists(target: Cbor<'_>, path: JsonPathArg<'_>, vars: Option<Cbor<'_>>, silent: bool) -> Option<bool> {
    let p = Path::decode(path.0).or_raise(Source::Stored);
    let d = parse_datum(target);
    let v = vars.map(parse_datum);
    match run(&p, &d, v.as_ref(), silent, None) {
        Res::Error => None,
        r => Some(r == Res::Ok),
    }
}

fn matches(target: Cbor<'_>, path: JsonPathArg<'_>, vars: Option<Cbor<'_>>, silent: bool) -> Option<bool> {
    let p = Path::decode(path.0).or_raise(Source::Stored);
    let d = parse_datum(target);
    let v = vars.map(parse_datum);
    let mut found = Vec::new();
    run(&p, &d, v.as_ref(), silent, Some(&mut found));
    if let [one] = found.as_slice()
        && let Some(b) = eval::bool_or_null(one).unwrap_or_else(|e| raise_path(e))
    {
        return b;
    }
    if !silent {
        fail(PgSqlErrorCode::ERRCODE_SINGLETON_SQL_JSON_ITEM_REQUIRED, "single boolean result is expected");
    }
    None
}

fn query(target: Cbor<'_>, path: JsonPathArg<'_>, vars: Cbor<'_>, silent: bool) -> Vec<CborOut> {
    let p = Path::decode(path.0).or_raise(Source::Stored);
    let d = parse_datum(target);
    let v = parse_datum(vars);
    let mut found = Vec::new();
    run(&p, &d, Some(&v), silent, Some(&mut found));
    found.iter().map(out).collect()
}

fn query_array(target: Cbor<'_>, path: JsonPathArg<'_>, vars: Cbor<'_>, silent: bool) -> CborOut {
    let p = Path::decode(path.0).or_raise(Source::Stored);
    let d = parse_datum(target);
    let v = parse_datum(vars);
    let mut found = Vec::new();
    run(&p, &d, Some(&v), silent, Some(&mut found));
    let mut buf = PallocBuf::varlena(32);
    eval::write_array(&found, &mut buf, PgCtx::new().limits().max_depth).or_raise(Source::Stored);
    CborOut(buf.into_varlena())
}

fn query_first(target: Cbor<'_>, path: JsonPathArg<'_>, vars: Cbor<'_>, silent: bool) -> Option<CborOut> {
    let p = Path::decode(path.0).or_raise(Source::Stored);
    let d = parse_datum(target);
    let v = parse_datum(vars);
    let mut found = Vec::new();
    run(&p, &d, Some(&v), silent, Some(&mut found));
    found.first().map(out)
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_path_exists(target: Cbor<'_>, path: JsonPathArg<'_>, vars: default!(Cbor<'_>, "'{}'"), silent: default!(bool, false)) -> Option<bool> {
    exists(target, path, Some(vars), silent)
}

#[pg_extern(stable, strict, parallel_safe)]
fn cbor_path_exists_tz(target: Cbor<'_>, path: JsonPathArg<'_>, vars: default!(Cbor<'_>, "'{}'"), silent: default!(bool, false)) -> Option<bool> {
    exists(target, path, Some(vars), silent)
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_path_match(target: Cbor<'_>, path: JsonPathArg<'_>, vars: default!(Cbor<'_>, "'{}'"), silent: default!(bool, false)) -> Option<bool> {
    matches(target, path, Some(vars), silent)
}

#[pg_extern(stable, strict, parallel_safe)]
fn cbor_path_match_tz(target: Cbor<'_>, path: JsonPathArg<'_>, vars: default!(Cbor<'_>, "'{}'"), silent: default!(bool, false)) -> Option<bool> {
    matches(target, path, Some(vars), silent)
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_path_query<'a>(target: Cbor<'a>, path: JsonPathArg<'a>, vars: default!(Cbor<'a>, "'{}'"), silent: default!(bool, false)) -> SetOfIterator<'a, CborOut> {
    SetOfIterator::new(query(target, path, vars, silent))
}

#[pg_extern(stable, strict, parallel_safe)]
fn cbor_path_query_tz<'a>(target: Cbor<'a>, path: JsonPathArg<'a>, vars: default!(Cbor<'a>, "'{}'"), silent: default!(bool, false)) -> SetOfIterator<'a, CborOut> {
    SetOfIterator::new(query(target, path, vars, silent))
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_path_query_array(target: Cbor<'_>, path: JsonPathArg<'_>, vars: default!(Cbor<'_>, "'{}'"), silent: default!(bool, false)) -> CborOut {
    query_array(target, path, vars, silent)
}

#[pg_extern(stable, strict, parallel_safe)]
fn cbor_path_query_array_tz(target: Cbor<'_>, path: JsonPathArg<'_>, vars: default!(Cbor<'_>, "'{}'"), silent: default!(bool, false)) -> CborOut {
    query_array(target, path, vars, silent)
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_path_query_first(target: Cbor<'_>, path: JsonPathArg<'_>, vars: default!(Cbor<'_>, "'{}'"), silent: default!(bool, false)) -> Option<CborOut> {
    query_first(target, path, vars, silent)
}

#[pg_extern(stable, strict, parallel_safe)]
fn cbor_path_query_first_tz(target: Cbor<'_>, path: JsonPathArg<'_>, vars: default!(Cbor<'_>, "'{}'"), silent: default!(bool, false)) -> Option<CborOut> {
    query_first(target, path, vars, silent)
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(@?)]
#[restrict(contsel)]
#[join(contjoinsel)]
fn cbor_path_exists_opr(target: Cbor<'_>, path: JsonPathArg<'_>) -> Option<bool> {
    exists(target, path, None, true)
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(@@)]
#[restrict(contsel)]
#[join(contjoinsel)]
fn cbor_path_match_opr(target: Cbor<'_>, path: JsonPathArg<'_>) -> Option<bool> {
    matches(target, path, None, true)
}
