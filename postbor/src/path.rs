//! Path queries over `cbor` with PostgreSQL's `jsonpath` (design §8 Path queries): the SQL functions and
//! operators, and the host the core evaluator calls for `numeric` arithmetic, input functions and regexes.

use core::ffi::{c_char, c_int};

use pgrx::iter::SetOfIterator;
use pgrx::prelude::*;
use postbor_core::Ctx;
use postbor_core::datum::Datum;
use postbor_core::decimal::Num;
use core::cmp::Ordering;

use postbor_core::jsonpath::eval::{self, Code, Datetime, DtArg, DtOut, DtType, Host, JItem, PathError, PathFail, Res, Seq};
use postbor_core::jsonpath::{BinOp, DatetimeMethod, Path};
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
    #[cfg(not(feature = "pg15"))]
    fn int4in(fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum;
    #[cfg(not(feature = "pg15"))]
    fn int8in(fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum;
    #[cfg(not(feature = "pg15"))]
    fn numeric_in(fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum;
    #[cfg(not(feature = "pg15"))]
    fn timestamptz_in(fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum;
    #[cfg(feature = "pg15")]
    fn parse_datetime(date_txt: *mut pg_sys::text, fmt: *mut pg_sys::text, collid: pg_sys::Oid, strict: bool, typid: *mut pg_sys::Oid, typmod: *mut i32, tz: *mut c_int, have_error: *mut bool) -> pg_sys::Datum;
    #[cfg(not(feature = "pg15"))]
    fn parse_datetime(date_txt: *mut pg_sys::text, fmt: *mut pg_sys::text, collid: pg_sys::Oid, strict: bool, typid: *mut pg_sys::Oid, typmod: *mut i32, tz: *mut c_int, escontext: *mut pg_sys::Node) -> pg_sys::Datum;
}

/// The ISO formats `.datetime()` and the typed methods try in turn without a template, as PostgreSQL does. Before
/// PostgreSQL 17 the offset was spelled `TZH:TZM` or `TZH`.
#[cfg(not(any(feature = "pg15", feature = "pg16")))]
const ISO_FORMATS: &[&str] = &[
    "yyyy-mm-dd",
    "HH24:MI:SS.USTZ",
    "HH24:MI:SSTZ",
    "HH24:MI:SS.US",
    "HH24:MI:SS",
    "yyyy-mm-dd HH24:MI:SS.USTZ",
    "yyyy-mm-dd HH24:MI:SSTZ",
    "yyyy-mm-dd\"T\"HH24:MI:SS.USTZ",
    "yyyy-mm-dd\"T\"HH24:MI:SSTZ",
    "yyyy-mm-dd HH24:MI:SS.US",
    "yyyy-mm-dd HH24:MI:SS",
    "yyyy-mm-dd\"T\"HH24:MI:SS.US",
    "yyyy-mm-dd\"T\"HH24:MI:SS",
];
#[cfg(any(feature = "pg15", feature = "pg16"))]
const ISO_FORMATS: &[&str] = &[
    "yyyy-mm-dd",
    "HH24:MI:SS.USTZH:TZM",
    "HH24:MI:SS.USTZH",
    "HH24:MI:SSTZH:TZM",
    "HH24:MI:SSTZH",
    "HH24:MI:SS.US",
    "HH24:MI:SS",
    "yyyy-mm-dd HH24:MI:SS.USTZH:TZM",
    "yyyy-mm-dd HH24:MI:SS.USTZH",
    "yyyy-mm-dd HH24:MI:SSTZH:TZM",
    "yyyy-mm-dd HH24:MI:SSTZH",
    "yyyy-mm-dd\"T\"HH24:MI:SS.USTZH:TZM",
    "yyyy-mm-dd\"T\"HH24:MI:SS.USTZH",
    "yyyy-mm-dd\"T\"HH24:MI:SSTZH:TZM",
    "yyyy-mm-dd\"T\"HH24:MI:SSTZH",
    "yyyy-mm-dd HH24:MI:SS.US",
    "yyyy-mm-dd HH24:MI:SS",
    "yyyy-mm-dd\"T\"HH24:MI:SS.US",
    "yyyy-mm-dd\"T\"HH24:MI:SS",
];

/// A `text` value in the current memory context.
fn text(s: &[u8]) -> *mut pg_sys::text {
    let mut buf = PallocBuf::varlena(s.len());
    postbor_core::OutBuf::write_bytes(&mut buf, s).or_raise(Source::Stored);
    buf.into_varlena().cast()
}

type Fmgr = unsafe fn(pg_sys::FunctionCallInfo) -> pg_sys::Datum;

fn call1(f: Fmgr, a: pg_sys::Datum) -> pg_sys::Datum {
    // SAFETY: callers pass a function taking one argument of `a`'s type.
    unsafe { pgrx::direct_function_call::<pg_sys::Datum>(f, &[Some(a)]) }.unwrap_or(pg_sys::Datum::from(0usize))
}

fn call2(f: Fmgr, a: pg_sys::Datum, b: pg_sys::Datum) -> i32 {
    // SAFETY: callers pass a comparison function taking two arguments of these types.
    unsafe { pgrx::direct_function_call::<i32>(f, &[Some(a), Some(b)]) }.unwrap_or(0)
}

fn type_oid(t: DtType) -> pg_sys::Oid {
    match t {
        DtType::Date => pg_sys::DATEOID,
        DtType::Time => pg_sys::TIMEOID,
        DtType::TimeTz => pg_sys::TIMETZOID,
        DtType::Timestamp => pg_sys::TIMESTAMPOID,
        DtType::TimestampTz => pg_sys::TIMESTAMPTZOID,
    }
}

fn dt_datum(d: &Datetime) -> pg_sys::Datum {
    match d.typ {
        DtType::Date => pg_sys::Datum::from(d.value as i32),
        DtType::TimeTz => {
            // SAFETY: palloc returns a block of the requested size or raises.
            let p = unsafe { pg_sys::palloc(core::mem::size_of::<pg_sys::TimeTzADT>()) }.cast::<pg_sys::TimeTzADT>();
            // SAFETY: `p` points to a writable TimeTzADT.
            unsafe { p.write(pg_sys::TimeTzADT { time: d.value, zone: d.zone }) };
            pg_sys::Datum::from(p)
        }
        _ => pg_sys::Datum::from(d.value),
    }
}

fn dt_from(value: pg_sys::Datum, typid: pg_sys::Oid, typmod: i32, tz: i32) -> Datetime {
    let (typ, v, zone) = match typid {
        pg_sys::DATEOID => (DtType::Date, i64::from(value.value() as i32), 0),
        pg_sys::TIMETZOID => {
            // SAFETY: a timetz datum points to a TimeTzADT.
            let t = unsafe { value.cast_mut_ptr::<pg_sys::TimeTzADT>().read() };
            (DtType::TimeTz, t.time, t.zone)
        }
        pg_sys::TIMEOID => (DtType::Time, value.value() as i64, 0),
        pg_sys::TIMESTAMPOID => (DtType::Timestamp, value.value() as i64, 0),
        _ => (DtType::TimestampTz, value.value() as i64, 0),
    };
    Datetime { typ, value: v, zone, typmod, tz }
}

/// `checkTimezoneIsUsedForCast`: raised whatever the mode.
fn tz_check(use_tz: bool, from: &str, to: &str) {
    if !use_tz {
        ereport!(
            ERROR,
            PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED,
            format!("cannot convert value from {from} to {to} without time zone usage"),
            "Use *_tz() function for time zone support."
        );
    }
}

/// `parse_datetime` with one template; None after a soft error (only when `soft`).
fn parse_one(input: *mut pg_sys::text, fmt: &[u8], soft: bool) -> Option<(pg_sys::Datum, pg_sys::Oid, i32, i32)> {
    let (mut typid, mut typmod, mut tz) = (pg_sys::InvalidOid, -1i32, 0 as c_int);
    let fmt = text(fmt);
    #[cfg(feature = "pg15")]
    {
        let mut error = false;
        let e: *mut bool = if soft { &mut error } else { core::ptr::null_mut() };
        // SAFETY: both arguments are text values; a null error pointer makes failures raise, which the guard handles.
        let v = unsafe { pg_sys::ffi::pg_guard_ffi_boundary(|| parse_datetime(input, fmt, pg_sys::DEFAULT_COLLATION_OID, true, &mut typid, &mut typmod, &mut tz, e)) };
        (!error).then_some((v, typid, typmod, tz))
    }
    #[cfg(not(feature = "pg15"))]
    {
        let mut es = soft_context();
        let ctx: *mut pg_sys::Node = if soft { (&raw mut es).cast() } else { core::ptr::null_mut() };
        // SAFETY: as above, with an error context that outlives the call.
        let v = unsafe { pg_sys::ffi::pg_guard_ffi_boundary(|| parse_datetime(input, fmt, pg_sys::DEFAULT_COLLATION_OID, true, &mut typid, &mut typmod, &mut tz, ctx)) };
        (!es.error_occurred).then_some((v, typid, typmod, tz))
    }
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

    fn datetime(&mut self, m: DatetimeMethod, arg: DtArg, s: &[u8], throw: bool, use_tz: bool) -> DtOut {
        let input_text = String::from_utf8_lossy(s).into_owned();
        let not_recognized = |name: &str| DtOut::Fail(Code::DatetimeArgument, format!("{name} format is not recognized: \"{input_text}\""));
        let input = text(s);
        let parsed = match (m, arg) {
            (DatetimeMethod::Datetime, DtArg::Template(t)) => match parse_one(input, t, !throw) {
                Some(p) => p,
                None => return DtOut::Suppressed,
            },
            _ => match ISO_FORMATS.iter().find_map(|f| parse_one(input, f.as_bytes(), true)) {
                Some(p) => p,
                None => {
                    let name = match m {
                        DatetimeMethod::Datetime => "datetime",
                        DatetimeMethod::Date => "date",
                        DatetimeMethod::Time => "time",
                        DatetimeMethod::TimeTz => "time_tz",
                        DatetimeMethod::Timestamp => "timestamp",
                        DatetimeMethod::TimestampTz => "timestamp_tz",
                    };
                    return not_recognized(name);
                }
            },
        };
        #[allow(unused_mut)]
        let (mut value, mut typid, mut typmod, mut tz) = parsed;
        #[cfg(any(feature = "pg15", feature = "pg16"))]
        let _ = (&mut value, &mut typid, &mut typmod, &mut tz, use_tz, &not_recognized);
        // The typed methods exist from PostgreSQL 17.
        #[cfg(not(any(feature = "pg15", feature = "pg16")))]
        {
            use pg_sys::{DATEOID, TIMEOID, TIMESTAMPOID, TIMESTAMPTZOID, TIMETZOID};
            let precision = match arg {
                DtArg::Precision(p) => Some(p),
                _ => None,
            };
            match m {
                DatetimeMethod::Datetime => {}
                DatetimeMethod::Date => {
                    match typid {
                        DATEOID => {}
                        TIMEOID | TIMETZOID => return not_recognized("date"),
                        TIMESTAMPOID => value = call1(pg_sys::timestamp_date, value),
                        _ => {
                            tz_check(use_tz, "timestamptz", "date");
                            value = call1(pg_sys::timestamptz_date, value);
                        }
                    }
                    typid = DATEOID;
                }
                DatetimeMethod::Time => {
                    match typid {
                        DATEOID => return not_recognized("time"),
                        TIMEOID => {}
                        TIMETZOID => {
                            tz_check(use_tz, "timetz", "time");
                            value = call1(pg_sys::timetz_time, value);
                        }
                        TIMESTAMPOID => value = call1(pg_sys::timestamp_time, value),
                        _ => {
                            tz_check(use_tz, "timestamptz", "time");
                            value = call1(pg_sys::timestamptz_time, value);
                        }
                    }
                    if let Some(p) = precision {
                        // SAFETY: plain value computations; out-of-range precisions raise a warning or an error.
                        let p = unsafe { pg_sys::anytime_typmod_check(false, p) };
                        let mut t = value.value() as i64;
                        // SAFETY: `t` is a valid TimeADT.
                        unsafe { pg_sys::AdjustTimeForTypmod(&mut t, p) };
                        value = pg_sys::Datum::from(t);
                        typmod = p;
                    }
                    typid = TIMEOID;
                }
                DatetimeMethod::TimeTz => {
                    match typid {
                        DATEOID | TIMESTAMPOID => return not_recognized("time_tz"),
                        TIMEOID => {
                            tz_check(use_tz, "time", "timetz");
                            value = call1(pg_sys::time_timetz, value);
                        }
                        TIMETZOID => {}
                        _ => value = call1(pg_sys::timestamptz_timetz, value),
                    }
                    if let Some(p) = precision {
                        // SAFETY: as above; the timetz datum points to a TimeTzADT this call owns.
                        unsafe {
                            let p = pg_sys::anytime_typmod_check(true, p);
                            pg_sys::AdjustTimeForTypmod(&raw mut (*value.cast_mut_ptr::<pg_sys::TimeTzADT>()).time, p);
                            typmod = p;
                        }
                    }
                    typid = TIMETZOID;
                }
                DatetimeMethod::Timestamp | DatetimeMethod::TimestampTz => {
                    let with_tz = m == DatetimeMethod::TimestampTz;
                    match (typid, with_tz) {
                        (TIMEOID | TIMETZOID, _) => return not_recognized(if with_tz { "timestamp_tz" } else { "timestamp" }),
                        (DATEOID, false) => value = call1(pg_sys::date_timestamp, value),
                        (TIMESTAMPOID, false) | (TIMESTAMPTZOID, true) => {}
                        (_, false) => {
                            tz_check(use_tz, "timestamptz", "timestamp");
                            value = call1(pg_sys::timestamptz_timestamp, value);
                        }
                        (DATEOID, true) => {
                            tz_check(use_tz, "date", "timestamptz");
                            // SAFETY: plain calendar computations on a valid date; session_timezone is set in a backend.
                            unsafe {
                                let mut tm: pg_sys::pg_tm = core::mem::zeroed();
                                pg_sys::j2date(value.value() as i32 + pg_sys::POSTGRES_EPOCH_JDATE as i32, &mut tm.tm_year, &mut tm.tm_mon, &mut tm.tm_mday);
                                tz = pg_sys::DetermineTimeZoneOffset(&mut tm, pg_sys::session_timezone);
                            }
                            value = call1(pg_sys::date_timestamptz, value);
                        }
                        (_, true) => {
                            tz_check(use_tz, "timestamp", "timestamptz");
                            // SAFETY: as above, on a valid timestamp.
                            unsafe {
                                let mut tm: pg_sys::pg_tm = core::mem::zeroed();
                                let mut fsec: pg_sys::fsec_t = 0;
                                if pg_sys::timestamp2tm(value.value() as i64, core::ptr::null_mut(), &mut tm, &mut fsec, core::ptr::null_mut(), core::ptr::null_mut()) == 0 {
                                    tz = pg_sys::DetermineTimeZoneOffset(&mut tm, pg_sys::session_timezone);
                                }
                            }
                            value = call1(pg_sys::timestamp_timestamptz, value);
                        }
                    }
                    if let Some(p) = precision {
                        let mut es = soft_context();
                        // SAFETY: plain value computations; the error context outlives the call.
                        let (p, t, failed) = unsafe {
                            let p = pg_sys::anytimestamp_typmod_check(with_tz, p);
                            let mut t = value.value() as i64;
                            pg_sys::AdjustTimestampForTypmod(&mut t, p, (&raw mut es).cast());
                            (p, t, es.error_occurred)
                        };
                        if failed {
                            let name = if with_tz { "timestamp_tz" } else { "timestamp" };
                            return DtOut::Fail(Code::DatetimeArgument, format!("time precision of jsonpath item method .{name}() is invalid"));
                        }
                        value = pg_sys::Datum::from(t);
                        typmod = p;
                    }
                    typid = if with_tz { TIMESTAMPTZOID } else { TIMESTAMPOID };
                }
            }
        }
        DtOut::Ok(dt_from(value, typid, typmod, tz))
    }

    fn datetime_cmp(&mut self, a: &Datetime, b: &Datetime, use_tz: bool) -> Option<Ordering> {
        use DtType::*;
        let (va, vb) = (dt_datum(a), dt_datum(b));
        // SAFETY (the *_internal calls): plain comparisons of valid values.
        let c = match (a.typ, b.typ) {
            (Date, Date) => call2(pg_sys::date_cmp, va, vb),
            (Date, Timestamp) => unsafe { pg_sys::date_cmp_timestamp_internal(a.value as i32, b.value) },
            (Date, TimestampTz) => {
                tz_check(use_tz, "date", "timestamptz");
                unsafe { pg_sys::date_cmp_timestamptz_internal(a.value as i32, b.value) }
            }
            (Time, Time) => call2(pg_sys::time_cmp, va, vb),
            (Time, TimeTz) => {
                tz_check(use_tz, "time", "timetz");
                call2(pg_sys::timetz_cmp, call1(pg_sys::time_timetz, va), vb)
            }
            (TimeTz, Time) => {
                tz_check(use_tz, "time", "timetz");
                call2(pg_sys::timetz_cmp, va, call1(pg_sys::time_timetz, vb))
            }
            (TimeTz, TimeTz) => call2(pg_sys::timetz_cmp, va, vb),
            (Timestamp, Date) => -unsafe { pg_sys::date_cmp_timestamp_internal(b.value as i32, a.value) },
            (Timestamp, Timestamp) | (TimestampTz, TimestampTz) => call2(pg_sys::timestamp_cmp, va, vb),
            (Timestamp, TimestampTz) => {
                tz_check(use_tz, "timestamp", "timestamptz");
                unsafe { pg_sys::timestamp_cmp_timestamptz_internal(a.value, b.value) }
            }
            (TimestampTz, Date) => {
                tz_check(use_tz, "date", "timestamptz");
                -unsafe { pg_sys::date_cmp_timestamptz_internal(b.value as i32, a.value) }
            }
            (TimestampTz, Timestamp) => {
                tz_check(use_tz, "timestamp", "timestamptz");
                -unsafe { pg_sys::timestamp_cmp_timestamptz_internal(b.value, a.value) }
            }
            _ => return None,
        };
        Some(c.cmp(&0))
    }

    fn datetime_text(&mut self, d: &Datetime) -> String {
        let tz: c_int = d.tz;
        // SAFETY: a valid datetime datum of its type; a null buffer makes the function allocate one.
        let p = unsafe { pg_sys::JsonEncodeDateTime(core::ptr::null_mut(), dt_datum(d), type_oid(d.typ), &tz) };
        // SAFETY: the result is a NUL-terminated string in palloc memory.
        unsafe { core::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }

    fn timestamptz_in(&mut self, s: &str) -> Option<i64> {
        #[cfg(feature = "pg15")]
        {
            // The core checked the RFC 3339 syntax; out-of-range dates raise.
            let v = unsafe { pgrx::direct_function_call::<pg_sys::Datum>(pg_sys::timestamptz_in, &[Some(pg_sys::Datum::from(cstring(s))), Some(pg_sys::Datum::from(pg_sys::InvalidOid)), Some(pg_sys::Datum::from(-1i32))]) };
            v.map(|d| d.value() as i64)
        }
        #[cfg(not(feature = "pg15"))]
        {
            input_safe(timestamptz_in, s, -1).map(|d| d.value() as i64)
        }
    }

    fn epoch_timestamptz(&mut self, secs: f64) -> i64 {
        call1(pg_sys::float8_timestamptz, secs.into_datum().unwrap_or(pg_sys::Datum::from(0usize))).value() as i64
    }

    fn check_stack(&mut self) {
        // SAFETY: raises an error when the stack is too deep.
        unsafe { pg_sys::check_stack_depth() };
    }
}

fn raise_path(e: PathError) -> ! {
    use PgSqlErrorCode::*;
    let (code, msg) = match *e.0 {
        PathFail::Core(e) => match e.kind {
            postbor_core::ErrorKind::InvalidArgument(m) => (ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE, m.to_string()),
            _ => raise(e, Source::Stored),
        },
        PathFail::Sql(code, msg) => (
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
                Code::DatetimeArgument => ERRCODE_INVALID_ARGUMENT_FOR_SQL_JSON_DATETIME_FUNCTION,
                Code::Internal => ERRCODE_INTERNAL_ERROR,
            },
            msg,
        ),
    };
    fail(code, msg)
}

/// The decoded path of a call site, kept in `fn_extra` while the query runs. Set-returning functions cannot use it
/// (`fn_extra` holds their call context) and decode once per call.
struct PathCache {
    // Declared first, so dropped before the bytes it borrows.
    path: Option<Path<'static>>,
    bytes: Box<[u8]>,
}

/// The decoded path, from the call site's cache when the same path was decoded before.
fn cached_path<'c>(fcinfo: pg_sys::FunctionCallInfo, arg: JsonPathArg<'_>) -> &'c Path<'c> {
    // SAFETY: a function call's flinfo is valid for the call; fn_extra is ours for non-set-returning functions and
    // lives in fn_mcxt, which outlives every call of the query.
    unsafe {
        let flinfo = (*fcinfo).flinfo;
        let mut cache = (*flinfo).fn_extra.cast::<PathCache>();
        if cache.is_null() {
            cache = pgrx::PgMemoryContexts::For((*flinfo).fn_mcxt).leak_and_drop_on_delete(PathCache { path: None, bytes: Box::new([]) });
            (*flinfo).fn_extra = cache.cast();
        }
        let cache = &mut *cache;
        if cache.path.is_none() || *cache.bytes != *arg.0 {
            cache.path = None;
            cache.bytes = arg.0.into();
            // The path borrows `bytes`, which stays unchanged until the path is dropped.
            let bytes: &'static [u8] = &*core::ptr::slice_from_raw_parts(cache.bytes.as_ptr(), cache.bytes.len());
            cache.path = Some(Path::decode(bytes).or_raise(Source::Stored));
        }
        cache.path.as_ref().unwrap_or_else(|| unreachable!())
    }
}

/// Runs a path; `vars` None is the operators' form (no variables, `$x` raises).
fn run<'a>(path: &'a Path<'a>, doc: &'a Datum<'a>, vars: Option<&'a Datum<'a>>, silent: bool, tz: bool, found: Option<&mut Seq<'a>>) -> Res {
    let res = match found {
        Some(f) => {
            let r = eval::execute(path, Value::root(doc), vars.map(Value::root), !silent, tz, &mut PgHost, Some(&mut *f));
            eval::datetimes_to_text(f, &mut PgHost);
            r
        }
        None => eval::execute(path, Value::root(doc), vars.map(Value::root), !silent, tz, &mut PgHost, None),
    };
    res.unwrap_or_else(|e| raise_path(e))
}

fn out(it: &JItem) -> CborOut {
    let mut buf = PallocBuf::varlena(32);
    eval::write_datum(it, &mut buf, PgCtx::new().limits().max_depth).or_raise(Source::Stored);
    CborOut(buf.into_varlena())
}

fn exists(target: Cbor<'_>, p: &Path<'_>, vars: Option<Cbor<'_>>, silent: bool, tz: bool) -> Option<bool> {
    let d = parse_datum(target);
    let v = vars.map(parse_datum);
    match run(p, &d, v.as_ref(), silent, tz, None) {
        Res::Error => None,
        r => Some(r == Res::Ok),
    }
}

fn matches(target: Cbor<'_>, p: &Path<'_>, vars: Option<Cbor<'_>>, silent: bool, tz: bool) -> Option<bool> {
    let d = parse_datum(target);
    let v = vars.map(parse_datum);
    let mut found = Seq::default();
    run(p, &d, v.as_ref(), silent, tz, Some(&mut found));
    if let Some(one) = found.single()
        && let Some(b) = eval::bool_or_null(one).unwrap_or_else(|e| raise_path(e))
    {
        return b;
    }
    if !silent {
        fail(PgSqlErrorCode::ERRCODE_SINGLETON_SQL_JSON_ITEM_REQUIRED, "single boolean result is expected");
    }
    None
}

fn query(target: Cbor<'_>, path: JsonPathArg<'_>, vars: Cbor<'_>, silent: bool, tz: bool) -> Vec<CborOut> {
    let p = Path::decode(path.0).or_raise(Source::Stored);
    let d = parse_datum(target);
    let v = parse_datum(vars);
    let mut found = Seq::default();
    run(&p, &d, Some(&v), silent, tz, Some(&mut found));
    found.iter().map(out).collect()
}

fn query_array(target: Cbor<'_>, p: &Path<'_>, vars: Cbor<'_>, silent: bool, tz: bool) -> CborOut {
    let d = parse_datum(target);
    let v = parse_datum(vars);
    let mut found = Seq::default();
    run(p, &d, Some(&v), silent, tz, Some(&mut found));
    let mut buf = PallocBuf::varlena(32);
    eval::write_array(&found, &mut buf, PgCtx::new().limits().max_depth).or_raise(Source::Stored);
    CborOut(buf.into_varlena())
}

fn query_first(target: Cbor<'_>, p: &Path<'_>, vars: Cbor<'_>, silent: bool, tz: bool) -> Option<CborOut> {
    let d = parse_datum(target);
    let v = parse_datum(vars);
    let mut found = Seq::default();
    run(p, &d, Some(&v), silent, tz, Some(&mut found));
    found.first().map(out)
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_path_exists(target: Cbor<'_>, path: JsonPathArg<'_>, vars: default!(Cbor<'_>, "'{}'"), silent: default!(bool, false), fcinfo: pg_sys::FunctionCallInfo) -> Option<bool> {
    exists(target, cached_path(fcinfo, path), Some(vars), silent, false)
}

#[pg_extern(stable, strict, parallel_safe)]
fn cbor_path_exists_tz(target: Cbor<'_>, path: JsonPathArg<'_>, vars: default!(Cbor<'_>, "'{}'"), silent: default!(bool, false), fcinfo: pg_sys::FunctionCallInfo) -> Option<bool> {
    exists(target, cached_path(fcinfo, path), Some(vars), silent, true)
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_path_match(target: Cbor<'_>, path: JsonPathArg<'_>, vars: default!(Cbor<'_>, "'{}'"), silent: default!(bool, false), fcinfo: pg_sys::FunctionCallInfo) -> Option<bool> {
    matches(target, cached_path(fcinfo, path), Some(vars), silent, false)
}

#[pg_extern(stable, strict, parallel_safe)]
fn cbor_path_match_tz(target: Cbor<'_>, path: JsonPathArg<'_>, vars: default!(Cbor<'_>, "'{}'"), silent: default!(bool, false), fcinfo: pg_sys::FunctionCallInfo) -> Option<bool> {
    matches(target, cached_path(fcinfo, path), Some(vars), silent, true)
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_path_query<'a>(target: Cbor<'a>, path: JsonPathArg<'a>, vars: default!(Cbor<'a>, "'{}'"), silent: default!(bool, false)) -> SetOfIterator<'a, CborOut> {
    SetOfIterator::new(query(target, path, vars, silent, false))
}

#[pg_extern(stable, strict, parallel_safe)]
fn cbor_path_query_tz<'a>(target: Cbor<'a>, path: JsonPathArg<'a>, vars: default!(Cbor<'a>, "'{}'"), silent: default!(bool, false)) -> SetOfIterator<'a, CborOut> {
    SetOfIterator::new(query(target, path, vars, silent, true))
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_path_query_array(target: Cbor<'_>, path: JsonPathArg<'_>, vars: default!(Cbor<'_>, "'{}'"), silent: default!(bool, false), fcinfo: pg_sys::FunctionCallInfo) -> CborOut {
    query_array(target, cached_path(fcinfo, path), vars, silent, false)
}

#[pg_extern(stable, strict, parallel_safe)]
fn cbor_path_query_array_tz(target: Cbor<'_>, path: JsonPathArg<'_>, vars: default!(Cbor<'_>, "'{}'"), silent: default!(bool, false), fcinfo: pg_sys::FunctionCallInfo) -> CborOut {
    query_array(target, cached_path(fcinfo, path), vars, silent, true)
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_path_query_first(target: Cbor<'_>, path: JsonPathArg<'_>, vars: default!(Cbor<'_>, "'{}'"), silent: default!(bool, false), fcinfo: pg_sys::FunctionCallInfo) -> Option<CborOut> {
    query_first(target, cached_path(fcinfo, path), vars, silent, false)
}

#[pg_extern(stable, strict, parallel_safe)]
fn cbor_path_query_first_tz(target: Cbor<'_>, path: JsonPathArg<'_>, vars: default!(Cbor<'_>, "'{}'"), silent: default!(bool, false), fcinfo: pg_sys::FunctionCallInfo) -> Option<CborOut> {
    query_first(target, cached_path(fcinfo, path), vars, silent, true)
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(@?)]
#[restrict(contsel)]
#[join(contjoinsel)]
fn cbor_path_exists_opr(target: Cbor<'_>, path: JsonPathArg<'_>, fcinfo: pg_sys::FunctionCallInfo) -> Option<bool> {
    exists(target, cached_path(fcinfo, path), None, true, false)
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(@@)]
#[restrict(contsel)]
#[join(contjoinsel)]
fn cbor_path_match_opr(target: Cbor<'_>, path: JsonPathArg<'_>, fcinfo: pg_sys::FunctionCallInfo) -> Option<bool> {
    matches(target, cached_path(fcinfo, path), None, true, false)
}
