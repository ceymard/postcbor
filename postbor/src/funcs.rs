//! Inspection functions, CBOR-specific functions and set-returning functions (design §8).

use pgrx::iter::{SetOfIterator, TableIterator};
use pgrx::prelude::*;
use postbor_core::cursor::MAJOR_TAG;
use postbor_core::encode::write_head;
use postbor_core::value::{Kind, Value};

use crate::access::{value_out, value_text};
use crate::convert::finish_raw;
use crate::datum::{Cbor, CborOut, PallocBuf, TextOut, VarlenaOut};
use crate::error::{OrRaise, Source};
use crate::io::root;
use crate::io::{parse_datum, parse_datum_in_context, print_value};


#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_typeof(c: Cbor<'_>) -> &'static str {
    match root!(c).kind().or_raise(Source::Stored) {
        Kind::Unsigned(_) | Kind::Negative(_) => "integer",
        Kind::Float(_) => "float",
        Kind::Bytes(_) => "bytes",
        Kind::Text(_) => "text",
        Kind::Array(_) => "array",
        Kind::Map(_) => "map",
        Kind::Tag(_) => "tag",
        Kind::Simple(_) => "simple",
        Kind::True | Kind::False => "boolean",
        Kind::Null => "null",
        Kind::Undefined => "undefined",
    }
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_array_length(c: Cbor<'_>) -> i32 {
    match root!(c).kind().or_raise(Source::Stored) {
        Kind::Array(n) => n as i32,
        _ => crate::error::fail(PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE, "cannot get array length of a non-array"),
    }
}

fn edn_text(c: Cbor<'_>, pretty: bool) -> TextOut {
    let d = parse_datum(c);
    let mut out = PallocBuf::varlena(d.body.len() * 2);
    print_value(&Value::root(&d), &mut out, pretty);
    TextOut(VarlenaOut(out.into_varlena()))
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_pretty(c: Cbor<'_>) -> TextOut {
    edn_text(c, true)
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_diag(c: Cbor<'_>, pretty: default!(bool, false)) -> TextOut {
    edn_text(c, pretty)
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_byte_length(c: Cbor<'_>) -> i64 {
    parse_datum(c).body.len() as i64
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_tag(c: Cbor<'_>) -> Option<i64> {
    match root!(c).kind().or_raise(Source::Stored) {
        Kind::Tag(t) => Some(i64::try_from(t).unwrap_or_else(|_| {
            crate::error::fail(PgSqlErrorCode::ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE, "tag number out of range for bigint")
        })),
        _ => None,
    }
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_untag(c: Cbor<'_>) -> CborOut {
    let d = parse_datum(c);
    let v = Value::root(&d);
    match v.kind().or_raise(Source::Stored) {
        Kind::Tag(_) => value_out(&v.tag_content().or_raise(Source::Stored)),
        _ => value_out(&v),
    }
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_tagged(tag: i64, value: Cbor<'_>) -> CborOut {
    if tag < 0 {
        crate::error::fail(PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE, "tag number must not be negative");
    }
    let mut raw = Vec::new();
    write_head(&mut raw, MAJOR_TAG, tag as u64).or_raise(Source::Stored);
    raw.extend_from_slice(parse_datum(value).body);
    // Normalized again: `cbor_tagged(2, h'01')` is the integer 1.
    finish_raw(&raw)
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_simple(n: i32) -> CborOut {
    let raw: Vec<u8> = match n {
        0..=23 => vec![0xe0 | n as u8],
        32..=255 => vec![0xf8, n as u8],
        _ => crate::error::fail(PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE, "simple value must be 0–23 or 32–255"),
    };
    finish_raw(&raw)
}

#[pg_extern(immutable, parallel_safe)]
fn cbor_undefined() -> CborOut {
    finish_raw(&[0xf7])
}

// ---- set-returning functions ----

fn not_a(what: &str) -> ! {
    ereport!(ERROR, PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE, format!("cannot call this function on a non-{what}"));
}

fn entries<'a>(c: Cbor<'a>) -> impl Iterator<Item = (Value<'a>, Value<'a>)> + 'a {
    let v = Value::root(parse_datum_in_context(c));
    if !matches!(v.kind().or_raise(Source::Stored), Kind::Map(_)) {
        not_a("map");
    }
    v.map_iter().or_raise(Source::Stored).map(|e| e.or_raise(Source::Stored))
}

fn elements<'a>(c: Cbor<'a>) -> impl Iterator<Item = Value<'a>> + 'a {
    let v = Value::root(parse_datum_in_context(c));
    if !matches!(v.kind().or_raise(Source::Stored), Kind::Array(_)) {
        not_a("array");
    }
    v.array_iter().or_raise(Source::Stored).map(|e| e.or_raise(Source::Stored))
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_each<'a>(c: Cbor<'a>) -> TableIterator<'a, (name!(key, CborOut), name!(value, CborOut))> {
    TableIterator::new(entries(c).map(|(k, v)| (value_out(&k), value_out(&v))))
}

/// Keys and values as text, with the `->>` rules (a null value gives SQL NULL).
#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_each_text<'a>(c: Cbor<'a>) -> TableIterator<'a, (name!(key, Option<TextOut>), name!(value, Option<TextOut>))> {
    TableIterator::new(entries(c).map(|(k, v)| (value_text(&k), value_text(&v))))
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_object_keys<'a>(c: Cbor<'a>) -> SetOfIterator<'a, CborOut> {
    SetOfIterator::new(entries(c).map(|(k, _)| value_out(&k)))
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_array_elements<'a>(c: Cbor<'a>) -> SetOfIterator<'a, CborOut> {
    SetOfIterator::new(elements(c).map(|e| value_out(&e)))
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_array_elements_text<'a>(c: Cbor<'a>) -> SetOfIterator<'a, Option<TextOut>> {
    SetOfIterator::new(elements(c).map(|e| value_text(&e)))
}

#[pg_extern(immutable, strict, parallel_safe)]
fn cbor_pack(c: Cbor<'_>, format: default!(&str, "'cbor-x'")) -> crate::datum::ByteaOut {
    use postbor_core::pack::{PackFormat, pack};
    let fmt = match format {
        "cbor-x" => PackFormat::CborX,
        "ietf" => PackFormat::Ietf,
        _ => crate::error::fail(PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE, "format must be 'cbor-x' or 'ietf'"),
    };
    let d = parse_datum(c);
    let mut out = PallocBuf::varlena(d.body.len());
    pack(&Value::root(&d), fmt, &mut out).or_raise(Source::Stored);
    crate::datum::ByteaOut(VarlenaOut(out.into_varlena()))
}
