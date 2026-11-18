//! Subscripting (`doc['a'][0]`, `SET doc['a'] = …`), modelled on PostgreSQL's jsonbsubs.c.
//! Subscripts keep their SQL type: integers index arrays or address integer keys, `bytea` addresses
//! byte-string keys, `cbor` addresses any key, anything else is text (jsonb rules).

use pgrx::pg_sys;
use pgrx::prelude::*;
use postbor_core::datum::Datum;
use postbor_core::edit::{self, Step};
use postbor_core::ops::get_text_path;
use postbor_core::value::Value;

use crate::access::value_out;
use crate::datum::{Cbor, PallocBuf, cbor_oid};
use crate::error::{OrRaise, Source, fail};
use crate::io::parse_datum;

/// SQL type each subscript is coerced to.
fn target_type(t: pg_sys::Oid) -> pg_sys::Oid {
    match t {
        pg_sys::INT2OID | pg_sys::INT4OID | pg_sys::INT8OID => pg_sys::INT8OID,
        pg_sys::BYTEAOID => pg_sys::BYTEAOID,
        o if o == cbor_oid() => o,
        _ => pg_sys::TEXTOID,
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn transform(
    sbsref: *mut pg_sys::SubscriptingRef,
    indirection: *mut pg_sys::List,
    pstate: *mut pg_sys::ParseState,
    is_slice: bool,
    _is_assignment: bool,
) {
    // SAFETY: called by the parser with a list of A_Indices; each upper subscript is transformed and coerced.
    unsafe {
        if is_slice {
            fail(PgSqlErrorCode::ERRCODE_DATATYPE_MISMATCH, "cbor subscript does not support slices");
        }
        let mut upper: *mut pg_sys::List = core::ptr::null_mut();
        let n = if indirection.is_null() { 0 } else { (*indirection).length };
        for i in 0..n {
            let ai = (*(*indirection).elements.add(i as usize)).ptr_value as *mut pg_sys::A_Indices;
            if (*ai).uidx.is_null() {
                fail(PgSqlErrorCode::ERRCODE_DATATYPE_MISMATCH, "cbor subscript must have a value");
            }
            let expr = pg_sys::transformExpr(pstate, (*ai).uidx, (*pstate).p_expr_kind);
            let from = pg_sys::exprType(expr);
            let to = if from == pg_sys::UNKNOWNOID { pg_sys::TEXTOID } else { target_type(from) };
            let coerced = pg_sys::coerce_type(
                pstate,
                expr,
                from,
                to,
                -1,
                pg_sys::CoercionContext::COERCION_IMPLICIT,
                pg_sys::CoercionForm::COERCE_IMPLICIT_CAST,
                -1,
            );
            if coerced.is_null() {
                fail(PgSqlErrorCode::ERRCODE_DATATYPE_MISMATCH, "cbor subscript type is not supported");
            }
            upper = pg_sys::lappend(upper, coerced.cast());
        }
        (*sbsref).refupperindexpr = upper;
        (*sbsref).reflowerindexpr = core::ptr::null_mut();
        // The container's own type: correct whatever schema the extension lives in.
        (*sbsref).refrestype = (*sbsref).refcontainertype;
        (*sbsref).reftypmod = -1;
    }
}

/// Per-expression workspace: the SQL type of each subscript.
#[repr(C)]
struct Workspace {
    n: usize,
    types: [pg_sys::Oid; 0],
}

#[pg_guard]
unsafe extern "C-unwind" fn exec_setup(
    sbsref: *const pg_sys::SubscriptingRef,
    state: *mut pg_sys::SubscriptingRefState,
    methods: *mut pg_sys::SubscriptExecSteps,
) {
    // SAFETY: the workspace is allocated in the executor's current context and outlives the expression.
    unsafe {
        let list = (*sbsref).refupperindexpr;
        let n = if list.is_null() { 0 } else { (*list).length as usize };
        let ws = pg_sys::palloc0(core::mem::size_of::<Workspace>() + n * core::mem::size_of::<pg_sys::Oid>()) as *mut Workspace;
        (*ws).n = n;
        let types = (*ws).types.as_mut_ptr();
        for i in 0..n {
            let expr = (*(*list).elements.add(i)).ptr_value as *mut pg_sys::Node;
            *types.add(i) = pg_sys::exprType(expr);
        }
        (*state).workspace = ws.cast();
        (*methods).sbs_check_subscripts = Some(check_subscripts);
        (*methods).sbs_fetch = Some(fetch);
        (*methods).sbs_assign = Some(assign);
        (*methods).sbs_fetch_old = Some(fetch_old);
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn check_subscripts(
    _state: *mut pg_sys::ExprState,
    op: *mut pg_sys::ExprEvalStep,
    _econtext: *mut pg_sys::ExprContext,
) -> bool {
    // SAFETY: executor step of a subscripting expression set up by `exec_setup`.
    unsafe {
        let st = (*op).d.sbsref_subscript.state;
        for i in 0..(*st).numupper as usize {
            if *(*st).upperindexnull.add(i) {
                if (*st).isassignment {
                    fail(PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED, "cbor subscript in assignment must not be null");
                }
                *(*op).resnull = true;
                return false;
            }
        }
        true
    }
}

/// Owned forms of the subscripts, so path steps can borrow them.
enum Sub {
    Text(String),
    Int(i64),
    Bytes(Vec<u8>),
    Cbor(Vec<u8>),
}

/// Reads the subscript values of the current evaluation.
unsafe fn subscripts(st: *mut pg_sys::SubscriptingRefState) -> Vec<Sub> {
    // SAFETY: subscripts were checked non-null and have the types recorded in the workspace.
    unsafe {
        let ws = (*st).workspace as *mut Workspace;
        let types = (*ws).types.as_ptr();
        (0..(*ws).n)
            .map(|i| {
                let d = *(*st).upperindex.add(i);
                match *types.add(i) {
                    pg_sys::INT8OID => Sub::Int(i64::from_datum(d, false).unwrap_or(0)),
                    pg_sys::BYTEAOID => Sub::Bytes(<&[u8]>::from_datum(d, false).unwrap_or_default().to_vec()),
                    o if o == cbor_oid() => Sub::Cbor(Cbor::from_datum(d, false).map(|c| c.0.to_vec()).unwrap_or_default()),
                    _ => Sub::Text(<&str>::from_datum(d, false).unwrap_or_default().to_string()),
                }
            })
            .collect()
    }
}

fn steps<'a>(subs: &'a [Sub], cbors: &'a [Option<Datum<'a>>]) -> Vec<Step<'a>> {
    subs.iter()
        .zip(cbors)
        .map(|(s, c)| match (s, c) {
            (Sub::Text(t), _) => Step::Text(t),
            (Sub::Int(i), _) => Step::Int(*i),
            (Sub::Bytes(b), _) => Step::Bytes(b),
            (Sub::Cbor(_), Some(d)) => Step::Typed(Value::root(*d)),
            (Sub::Cbor(_), None) => Step::Int(0),
        })
        .collect()
}

fn parsed(subs: &[Sub]) -> Vec<Option<Datum<'_>>> {
    subs.iter()
        .map(|s| match s {
            Sub::Cbor(b) => Some(Datum::parse(b).or_raise(Source::Stored)),
            _ => None,
        })
        .collect()
}

/// Value at the subscripts, or None.
fn get(doc: pg_sys::Datum, subs: &[Sub]) -> Option<pg_sys::Datum> {
    // SAFETY: `doc` is a non-null cbor datum.
    let c = unsafe { Cbor::from_datum(doc, false) }?;
    let root = Value::root(parse_datum(c));
    let cb = parsed(subs);
    let st = steps(subs, &cb);
    // Same walk as `#>`, extended to integer, byte and typed steps.
    let found = match st.iter().all(|s| matches!(s, Step::Text(_))) {
        true => {
            let texts: Vec<Option<&str>> = subs.iter().map(|s| if let Sub::Text(t) = s { Some(t.as_str()) } else { None }).collect();
            get_text_path(&root, &texts).or_raise(Source::Stored)
        }
        false => edit::walk(&root, &st, false).or_raise(Source::Stored),
    };
    found.and_then(|v| value_out(&v).into_datum())
}

#[pg_guard]
unsafe extern "C-unwind" fn fetch(_state: *mut pg_sys::ExprState, op: *mut pg_sys::ExprEvalStep, _econtext: *mut pg_sys::ExprContext) {
    // SAFETY: the container is non-null here (fetch_strict).
    unsafe {
        let st = (*op).d.sbsref.state;
        let subs = subscripts(st);
        match get(*(*op).resvalue, &subs) {
            Some(d) => *(*op).resvalue = d,
            None => *(*op).resnull = true,
        }
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn fetch_old(_state: *mut pg_sys::ExprState, op: *mut pg_sys::ExprEvalStep, _econtext: *mut pg_sys::ExprContext) {
    // SAFETY: for nested assignments; the container may be NULL.
    unsafe {
        let st = (*op).d.sbsref.state;
        if *(*op).resnull {
            (*st).prevnull = true;
            (*st).prevvalue = pg_sys::Datum::from(0usize);
            return;
        }
        let subs = subscripts(st);
        match get(*(*op).resvalue, &subs) {
            Some(d) => {
                (*st).prevvalue = d;
                (*st).prevnull = false;
            }
            None => (*st).prevnull = true,
        }
    }
}

const CBOR_NULL_BODY: [u8; 1] = [0xf6];

#[pg_guard]
unsafe extern "C-unwind" fn assign(_state: *mut pg_sys::ExprState, op: *mut pg_sys::ExprEvalStep, _econtext: *mut pg_sys::ExprContext) {
    // SAFETY: executor assignment step; the replacement and container come from the step state.
    unsafe {
        let st = (*op).d.sbsref.state;
        let subs = subscripts(st);
        let new_owned;
        let new: &[u8] = if (*st).replacenull {
            &CBOR_NULL_BODY
        } else {
            let c = Cbor::from_datum((*st).replacevalue, false).unwrap_or(crate::datum::cbor_bytes(&[]));
            new_owned = parse_datum(c).body.to_vec();
            &new_owned
        };
        let container = if *(*op).resnull { None } else { Cbor::from_datum(*(*op).resvalue, false) };
        let root = container.map(|c| Value::root(parse_datum(c)));
        let cb = parsed(&subs);
        let path = steps(&subs, &cb);
        let mut out = PallocBuf::varlena(container.map_or(0, |c| c.0.len()) + new.len() + 16);
        edit::assign(root.as_ref(), &path, new, &mut out).or_raise(Source::Stored);
        crate::modify::check_result_depth(postbor_core::OutBuf::as_slice(&out));
        *(*op).resvalue = pg_sys::Datum::from(out.into_varlena());
        *(*op).resnull = false;
    }
}

static ROUTINES: pg_sys::SubscriptRoutines = pg_sys::SubscriptRoutines {
    transform: Some(transform),
    exec_setup: Some(exec_setup),
    fetch_strict: true,
    fetch_leakproof: true,
    store_leakproof: false,
};

/// Returns the subscripting routines; PostgreSQL calls it through the type's SUBSCRIPT handler.
#[pg_extern(sql = false)]
fn cbor_subscript_handler(_fcinfo: pg_sys::FunctionCallInfo) -> crate::record::RawOutPtr {
    crate::record::RawOutPtr(&ROUTINES as *const _ as *mut core::ffi::c_void)
}

pgrx::extension_sql!(
    r#"
CREATE FUNCTION cbor_subscript_handler(internal) RETURNS internal
    IMMUTABLE STRICT PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_subscript_handler_wrapper';
ALTER TYPE cbor SET (SUBSCRIPT = cbor_subscript_handler);
"#,
    name = "cbor_subscript",
    requires = ["cbor_type"],
);
