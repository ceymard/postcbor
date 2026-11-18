//! Operators and functions that build new values: `||`, `-`, `#-`, set/insert, strip_nulls (design §8).

use pgrx::prelude::*;
use postbor_core::edit::{self, SetMode, Step};
use postbor_core::ops::{key_bytes, key_text};
use postbor_core::value::Value;

use crate::access::text_steps;
use crate::datum::{Cbor, CborOut, PallocBuf};
use crate::error::{OrRaise, Source};
use crate::settings::PgCtx;
use postbor_core::{Ctx, Limits};
use crate::io::parse_datum;

fn root(c: Cbor<'_>) -> Value<'_> {
    Value::root(parse_datum(c))
}

/// Runs a core edit into a fresh datum sized after `hint`, with the session's limits (results respect
/// `postbor.max_depth`, checked while the index section is built).
pub fn build(hint: usize, f: impl FnOnce(&mut PallocBuf, &Limits) -> postbor_core::Result<()>) -> CborOut {
    let mut out = PallocBuf::varlena(hint + 16);
    let limits = *PgCtx::new().limits();
    f(&mut out, &limits)
        .map_err(|e| match e.kind {
            // Offsets into the new value mean nothing to the caller.
            postbor_core::ErrorKind::DepthLimit => postbor_core::Error::new(e.kind, postbor_core::error::NO_OFFSET),
            _ => e,
        })
        .or_raise(Source::Stored);
    CborOut(out.into_varlena())
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(||)]
fn cbor_concat(a: Cbor<'_>, b: Cbor<'_>) -> CborOut {
    build(a.0.len() + b.0.len(), |o, l| edit::concat(&root(a), &root(b), o, l))
}

#[pg_operator(immutable, strict, parallel_safe, name = "cbor_delete")]
#[opname(-)]
fn cbor_delete_text(doc: Cbor<'_>, key: &str) -> CborOut {
    build(doc.0.len(), |o, l| edit::delete_matching(&root(doc), &[key_text(key).to_vec()], o, l))
}

#[pg_operator(immutable, strict, parallel_safe, name = "cbor_delete")]
#[opname(-)]
fn cbor_delete_texts(doc: Cbor<'_>, keys: Array<'_, &str>) -> CborOut {
    let keys: Vec<Vec<u8>> = keys.iter().flatten().map(|k| key_text(k).to_vec()).collect();
    build(doc.0.len(), |o, l| edit::delete_matching(&root(doc), &keys, o, l))
}

#[pg_operator(immutable, strict, parallel_safe, name = "cbor_delete")]
#[opname(-)]
fn cbor_delete_index(doc: Cbor<'_>, idx: i32) -> CborOut {
    build(doc.0.len(), |o, l| edit::delete_index(&root(doc), i64::from(idx), o, l))
}

#[pg_operator(immutable, strict, parallel_safe, name = "cbor_delete")]
#[opname(-)]
fn cbor_delete_bytes(doc: Cbor<'_>, key: &[u8]) -> CborOut {
    build(doc.0.len(), |o, l| edit::delete_matching(&root(doc), &[key_bytes(key).to_vec()], o, l))
}

/// Typed key: removes a map key of any type, or the array elements equal to it.
#[pg_extern(immutable, strict, parallel_safe, name = "cbor_delete")]
fn cbor_delete_key(doc: Cbor<'_>, key: Cbor<'_>) -> CborOut {
    let k = root(key).bytes().or_raise(Source::Stored).to_vec();
    build(doc.0.len(), |o, l| edit::delete_matching(&root(doc), &[k], o, l))
}

fn typed_steps<'a>(path: &Value<'a>) -> Vec<Step<'a>> {
    if !matches!(path.kind().or_raise(Source::Stored), postbor_core::value::Kind::Array(_)) {
        crate::error::raise(
            postbor_core::Error::new(
                postbor_core::ErrorKind::InvalidArgument("path must be a cbor array"),
                postbor_core::error::NO_OFFSET,
            ),
            Source::Stored,
        );
    }
    path.array_iter().or_raise(Source::Stored).map(|s| Step::Typed(s.or_raise(Source::Stored))).collect()
}

/// Text path steps; a NULL step makes the whole operation a no-op, as in jsonb.
fn with_text_path<R>(path: Array<'_, &str>, f: impl FnOnce(Option<&[Step]>) -> R) -> R {
    let owned = text_steps(path.iter());
    if owned.iter().any(Option::is_none) {
        return f(None);
    }
    let steps: Vec<Step> = owned.iter().map(|s| Step::Text(s.as_deref().unwrap_or_default())).collect();
    f(Some(&steps))
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(#-)]
fn cbor_delete_text_path(doc: Cbor<'_>, path: Array<'_, &str>) -> CborOut {
    with_text_path(path, |steps| match steps {
        Some(s) => build(doc.0.len(), |o, l| edit::delete_path(&root(doc), s, o, l)),
        None => build(doc.0.len(), |o, _| edit::copy_value(&root(doc), o)),
    })
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_delete_path(doc: Cbor<'_>, path: Cbor<'_>) -> CborOut {
    let p = root(path);
    let steps = typed_steps(&p);
    build(doc.0.len(), |o, l| edit::delete_path(&root(doc), &steps, o, l))
}

fn set_text(target: Cbor<'_>, path: Array<'_, &str>, new: Cbor<'_>, mode: SetMode) -> CborOut {
    let newbody = parse_datum(new).body;
    with_text_path(path, |steps| match steps {
        Some(s) => build(target.0.len() + newbody.len(), |o, l| edit::set_path(&root(target), s, newbody, mode, o, l)),
        None => crate::error::raise(
            postbor_core::Error::new(
                postbor_core::ErrorKind::InvalidArgument("path element must not be null"),
                postbor_core::error::NO_OFFSET,
            ),
            Source::Stored,
        ),
    })
}

fn set_typed(target: Cbor<'_>, path: Cbor<'_>, new: Cbor<'_>, mode: SetMode) -> CborOut {
    let newbody = parse_datum(new).body;
    let p = root(path);
    let steps = typed_steps(&p);
    build(target.0.len() + newbody.len(), |o, l| edit::set_path(&root(target), &steps, newbody, mode, o, l))
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_set(target: Cbor<'_>, path: Array<'_, &str>, new_value: Cbor<'_>, create_if_missing: default!(bool, true)) -> CborOut {
    set_text(target, path, new_value, SetMode::Set { create: create_if_missing })
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_set_path(target: Cbor<'_>, path: Cbor<'_>, new_value: Cbor<'_>, create_if_missing: default!(bool, true)) -> CborOut {
    set_typed(target, path, new_value, SetMode::Set { create: create_if_missing })
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_insert(target: Cbor<'_>, path: Array<'_, &str>, new_value: Cbor<'_>, insert_after: default!(bool, false)) -> CborOut {
    set_text(target, path, new_value, SetMode::Insert { after: insert_after })
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_insert_path(target: Cbor<'_>, path: Cbor<'_>, new_value: Cbor<'_>, insert_after: default!(bool, false)) -> CborOut {
    set_typed(target, path, new_value, SetMode::Insert { after: insert_after })
}

/// What `cbor_set_lax` does when the new value is SQL NULL.
enum NullTreatment {
    Raise,
    UseNull,
    Delete,
    ReturnTarget,
}

fn null_treatment(t: &str) -> NullTreatment {
    match t {
        "raise_exception" => NullTreatment::Raise,
        "use_json_null" | "use_cbor_null" => NullTreatment::UseNull,
        "delete_key" => NullTreatment::Delete,
        "return_target" => NullTreatment::ReturnTarget,
        _ => {
            ereport!(
                ERROR,
                PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
                "null_value_treatment must be \"delete_key\", \"return_target\", \"use_json_null\", or \"raise_exception\""
            );
        }
    }
}

const CBOR_NULL: [u8; 2] = [0x00, 0xf6];

#[pg_extern(immutable, parallel_safe)]
fn cbor_set_lax(
    target: Option<Cbor<'_>>,
    path: Option<Array<'_, &str>>,
    new_value: Option<Cbor<'_>>,
    create_if_missing: default!(bool, true),
    null_value_treatment: default!(&str, "'use_json_null'"),
) -> Option<CborOut> {
    let (target, path) = (target?, path?);
    if let Some(v) = new_value {
        return Some(set_text(target, path, v, SetMode::Set { create: create_if_missing }));
    }
    Some(match null_treatment(null_value_treatment) {
        NullTreatment::Raise => {
            ereport!(ERROR, PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED, "cbor value must not be null");
        }
        NullTreatment::UseNull => set_text(target, path, crate::datum::cbor_bytes(&CBOR_NULL), SetMode::Set { create: create_if_missing }),
        NullTreatment::Delete => cbor_delete_text_path(target, path),
        NullTreatment::ReturnTarget => build(target.0.len(), |o, _| edit::copy_value(&root(target), o)),
    })
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_strip_nulls(
    target: Cbor<'_>,
    strip_in_arrays: default!(bool, false),
    strip_undefined: default!(bool, false),
) -> CborOut {
    build(target.0.len(), |o, l| edit::strip_nulls(&root(target), strip_in_arrays, strip_undefined, o, l))
}
