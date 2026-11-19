//! Access, containment and existence operators, and the typed-key functions (design §8).

use pgrx::prelude::*;
use postcbor_core::OutBuf;
use postcbor_core::datum::write_subvalue;
use postcbor_core::ops;
use postcbor_core::value::{Kind, Value};

use crate::datum::{ByteaArg, Cbor, CborOut, PallocBuf, TextArg, TextArray, TextOut, VarlenaOut};
use crate::error::{OrRaise, Source};
use crate::io::root;
use crate::io::{parse_datum, print_value};

/// A sub-value as a datum of its own.
pub fn value_out(v: &Value) -> CborOut {
    let end = v.end().or_raise(Source::Stored);
    let mut out = PallocBuf::varlena(end - v.pos + 1);
    write_subvalue(v.d, v.pos, end, &mut out).or_raise(Source::Stored);
    CborOut(out.into_varlena())
}

/// `->>` rendering: text strings as themselves, null as SQL NULL, anything else as EDN.
pub fn value_text(v: &Value) -> Option<TextOut> {
    // Stored text is valid UTF-8 and the database is UTF-8 (ADR-0010): copied without re-checking.
    let out = if let Some(s) = v.text_bytes().or_raise(Source::Stored) {
        let mut out = PallocBuf::varlena(s.len());
        out.write_bytes(s).or_raise(Source::Stored);
        out
    } else if v.kind().or_raise(Source::Stored) == Kind::Null {
        return None;
    } else {
        let mut out = PallocBuf::varlena(16);
        print_value(v, &mut out, false);
        out
    };
    Some(TextOut(VarlenaOut(out.into_varlena())))
}


/// Text path steps as owned strings.
fn get_text_path<'a, 'x>(v: &Value<'a>, steps: impl Iterator<Item = Option<&'x str>>) -> Option<Value<'a>> {
    let steps: Vec<Option<&str>> = steps.collect();
    ops::get_text_path(v, &steps).or_raise(Source::Stored)
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(->)]
fn cbor_array_element(doc: Cbor<'_>, idx: i32) -> Option<CborOut> {
    ops::get_int(&root!(doc), i64::from(idx)).or_raise(Source::Stored).map(|v| value_out(&v))
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(->>)]
fn cbor_array_element_text(doc: Cbor<'_>, idx: i32) -> Option<TextOut> {
    ops::get_int(&root!(doc), i64::from(idx)).or_raise(Source::Stored).and_then(|v| value_text(&v))
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(->)]
fn cbor_object_field(doc: Cbor<'_>, key: TextArg<'_>) -> Option<CborOut> {
    root!(doc).map_get(&ops::key_text(key.0)).or_raise(Source::Stored).map(|v| value_out(&v))
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(->>)]
fn cbor_object_field_text(doc: Cbor<'_>, key: TextArg<'_>) -> Option<TextOut> {
    root!(doc).map_get(&ops::key_text(key.0)).or_raise(Source::Stored).and_then(|v| value_text(&v))
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(->)]
fn cbor_object_field_bytes(doc: Cbor<'_>, key: ByteaArg<'_>) -> Option<CborOut> {
    root!(doc).map_get(&ops::key_bytes(key.0)).or_raise(Source::Stored).map(|v| value_out(&v))
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(->>)]
fn cbor_object_field_bytes_text(doc: Cbor<'_>, key: ByteaArg<'_>) -> Option<TextOut> {
    root!(doc).map_get(&ops::key_bytes(key.0)).or_raise(Source::Stored).and_then(|v| value_text(&v))
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(#>)]
fn cbor_extract_path_op(doc: Cbor<'_>, path: TextArray<'_>) -> Option<CborOut> {
    get_text_path(&root!(doc), path.iter()).map(|v| value_out(&v))
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(#>>)]
fn cbor_extract_path_text_op(doc: Cbor<'_>, path: TextArray<'_>) -> Option<TextOut> {
    get_text_path(&root!(doc), path.iter()).and_then(|v| value_text(&v))
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_extract_path(doc: Cbor<'_>, path: VariadicArray<'_, &str>) -> Option<CborOut> {
    get_text_path(&root!(doc), path.iter()).map(|v| value_out(&v))
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_extract_path_text(doc: Cbor<'_>, path: VariadicArray<'_, &str>) -> Option<TextOut> {
    get_text_path(&root!(doc), path.iter()).and_then(|v| value_text(&v))
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_get(doc: Cbor<'_>, key: Cbor<'_>) -> Option<CborOut> {
    ops::get_key(&root!(doc), &root!(key)).or_raise(Source::Stored).map(|v| value_out(&v))
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_get_text(doc: Cbor<'_>, key: Cbor<'_>) -> Option<TextOut> {
    ops::get_key(&root!(doc), &root!(key)).or_raise(Source::Stored).and_then(|v| value_text(&v))
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_get_path(doc: Cbor<'_>, path: Cbor<'_>) -> Option<CborOut> {
    ops::get_typed_path(&root!(doc), &root!(path)).or_raise(Source::Stored).map(|v| value_out(&v))
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_get_path_text(doc: Cbor<'_>, path: Cbor<'_>) -> Option<TextOut> {
    ops::get_typed_path(&root!(doc), &root!(path)).or_raise(Source::Stored).and_then(|v| value_text(&v))
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(@>)]
#[commutator(<@)]
#[restrict(matchingsel)]
#[join(matchingjoinsel)]
fn cbor_contains(a: Cbor<'_>, b: Cbor<'_>) -> bool {
    ops::contains(&root!(a), &root!(b), &mut crate::settings::PgCtx::new()).or_raise(Source::Stored)
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(<@)]
#[commutator(@>)]
#[restrict(matchingsel)]
#[join(matchingjoinsel)]
fn cbor_contained(a: Cbor<'_>, b: Cbor<'_>) -> bool {
    ops::contains(&root!(b), &root!(a), &mut crate::settings::PgCtx::new()).or_raise(Source::Stored)
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(?)]
#[restrict(matchingsel)]
#[join(matchingjoinsel)]
fn cbor_exists(doc: Cbor<'_>, key: TextArg<'_>) -> bool {
    ops::exists_text(&root!(doc), key.0).or_raise(Source::Stored)
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(?|)]
#[restrict(matchingsel)]
#[join(matchingjoinsel)]
fn cbor_exists_any(doc: Cbor<'_>, keys: TextArray<'_>) -> bool {
    let d = parse_datum(doc);
    let v = Value::root(&d);
    keys.iter().flatten().any(|k| ops::exists_text(&v, k).or_raise(Source::Stored))
}

#[pg_operator(immutable, strict, parallel_safe)]
#[opname(?&)]
#[restrict(matchingsel)]
#[join(matchingjoinsel)]
fn cbor_exists_all(doc: Cbor<'_>, keys: TextArray<'_>) -> bool {
    let d = parse_datum(doc);
    let v = Value::root(&d);
    keys.iter().flatten().all(|k| ops::exists_text(&v, k).or_raise(Source::Stored))
}

/// `?#`: `?` with a key of any type (ADR-0006: a distinct symbol, so untyped literals are not captured).
#[pg_operator(immutable, strict, parallel_safe, name = "cbor_has")]
#[opname(?#)]
#[restrict(matchingsel)]
#[join(matchingjoinsel)]
fn cbor_has(doc: Cbor<'_>, key: Cbor<'_>) -> bool {
    ops::exists_encoded(&root!(doc), root!(key).bytes().or_raise(Source::Stored)).or_raise(Source::Stored)
}

/// Each element of the `keys` array checked with `?#`; a non-array `keys` has no elements.
fn has_keys(doc: Cbor<'_>, keys: Cbor<'_>, all: bool) -> bool {
    let (d, kd) = (parse_datum(doc), parse_datum(keys));
    let v = Value::root(&d);
    let mut it = Value::root(&kd)
        .array_iter()
        .or_raise(Source::Stored)
        .map(|k| ops::exists_encoded(&v, k.or_raise(Source::Stored).bytes().or_raise(Source::Stored)).or_raise(Source::Stored));
    if all { it.all(|x| x) } else { it.any(|x| x) }
}

/// `?#|`: any of the keys in a CBOR array, of any type.
#[pg_operator(immutable, strict, parallel_safe, name = "cbor_has_any")]
#[opname(?#|)]
#[restrict(matchingsel)]
#[join(matchingjoinsel)]
fn cbor_has_any(doc: Cbor<'_>, keys: Cbor<'_>) -> bool {
    has_keys(doc, keys, false)
}

/// `?#&`: all of the keys in a CBOR array, of any type.
#[pg_operator(immutable, strict, parallel_safe, name = "cbor_has_all")]
#[opname(?#&)]
#[restrict(matchingsel)]
#[join(matchingjoinsel)]
fn cbor_has_all(doc: Cbor<'_>, keys: Cbor<'_>) -> bool {
    has_keys(doc, keys, true)
}
