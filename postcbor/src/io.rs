//! Type I/O: EDN text in and out, deterministic CBOR for the binary protocol.

use core::ffi::CStr;

use pgrx::prelude::*;
use pgrx::{Internal, PgMemoryContexts, pg_sys};
use postcbor_core::datum::{self, Datum};
use postcbor_core::edn::{self, PrintOpts};
use postcbor_core::value::Value;

use crate::datum::{ByteaOut, Cbor, CborOut, PallocBuf, VarlenaOut};
use crate::error::{OrRaise, Source};
use crate::settings::PgCtx;

// The type and its I/O functions are declared together, first, so every later object sees a complete type.
// Text arguments and conversions assume UTF-8: other database encodings are refused until they are supported.
pgrx::extension_sql!(
    r#"
DO $$
BEGIN
    IF pg_catalog.current_setting('server_encoding') <> 'UTF8' THEN
        RAISE EXCEPTION 'postcbor requires a UTF-8 database (this one is %)', pg_catalog.current_setting('server_encoding');
    END IF;
END
$$;
CREATE TYPE cbor;
CREATE FUNCTION cbor_in(cstring) RETURNS cbor
    IMMUTABLE STRICT PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_in_wrapper';
CREATE FUNCTION cbor_out(cbor) RETURNS cstring
    IMMUTABLE STRICT PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_out_wrapper';
CREATE FUNCTION cbor_recv(internal) RETURNS cbor
    IMMUTABLE STRICT PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_recv_wrapper';
CREATE FUNCTION cbor_send(cbor) RETURNS bytea
    IMMUTABLE STRICT PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 'cbor_send_wrapper';
CREATE TYPE cbor (
    INPUT = cbor_in,
    OUTPUT = cbor_out,
    RECEIVE = cbor_recv,
    SEND = cbor_send,
    INTERNALLENGTH = VARIABLE,
    STORAGE = extended
);
"#,
    name = "cbor_type",
    bootstrap,
    creates = [Type(Cbor), Type(CborOut)],
);

/// Parses EDN text into a new datum.
pub fn parse_text(text: &[u8]) -> CborOut {
    let mut out = PallocBuf::varlena(text.len());
    edn::to_datum(text, &mut out, &mut PgCtx::new()).or_raise(Source::Text);
    CborOut(out.into_varlena())
}

/// Parsed view of a stored argument.
pub fn parse_datum(c: Cbor<'_>) -> Datum<'_> {
    let d = Datum::parse(c.0).or_raise(Source::Stored);
    #[cfg(debug_assertions)]
    revalidate(c.0, &d);
    d
}

/// Root value of a `cbor` argument. Its parsed datum is a temporary that lives until the end of the enclosing
/// statement; bind `parse_datum` to a local instead when the value must outlive it.
macro_rules! root {
    ($c:expr) => {
        postcbor_core::value::Value::root(&$crate::io::parse_datum($c))
    };
}
pub(crate) use root;

/// [`parse_datum`] for values that must outlive the call's locals (set-returning iterators): the parsed datum is
/// stored in the current memory context, like the detoasted bytes it points into.
pub fn parse_datum_in_context(c: Cbor<'_>) -> &Datum<'_> {
    let p = PgMemoryContexts::CurrentMemoryContext.leak_and_drop_on_delete(parse_datum(c));
    // SAFETY: the allocation lives until the current memory context is reset, which outlives this call's result.
    unsafe { &*p }
}

/// Debug builds: every stored datum must be exactly what normalizing its body gives (header and index included).
#[cfg(debug_assertions)]
fn revalidate(bytes: &[u8], d: &Datum) {
    let mut again = Vec::new();
    let ok = datum::from_cbor(d.body, &mut again, &mut PgCtx::new()).is_ok() && again == bytes;
    if !ok {
        crate::error::fail(PgSqlErrorCode::ERRCODE_DATA_CORRUPTED, "stored cbor datum is not canonical (debug re-validation)");
    }
}

/// Appends the EDN of `v`.
pub fn print_value(v: &Value, out: &mut PallocBuf, pretty: bool) {
    let opts = PrintOpts { pretty, ascii_only: false };
    edn::print(v, out, opts).or_raise(Source::Stored);
}

#[pg_extern(sql = false)]
fn cbor_in(input: &CStr) -> CborOut {
    parse_text(input.to_bytes())
}

#[pg_extern(sql = false)]
fn cbor_out(c: Cbor<'_>) -> &'static CStr {
    let d = parse_datum(c);
    let mut out = PallocBuf::cstring(d.body.len() * 2);
    print_value(&Value::root(&d), &mut out, false);
    out.into_cstr()
}

#[pg_extern(sql = false)]
fn cbor_recv(buf: Internal) -> CborOut {
    // SAFETY: the receive function's argument is the message StringInfo.
    let bytes = unsafe {
        let si = buf.unwrap().unwrap().cast_mut_ptr::<pg_sys::StringInfoData>();
        let start = (*si).cursor as usize;
        let len = (*si).len as usize;
        (*si).cursor = (*si).len;
        core::slice::from_raw_parts(((*si).data as *const u8).add(start), len - start)
    };
    from_cbor_bytes(bytes, Source::Binary)
}

/// Normalizes untrusted CBOR bytes into a new datum.
pub fn from_cbor_bytes(bytes: &[u8], src: Source) -> CborOut {
    let mut out = PallocBuf::varlena(bytes.len() + 1);
    datum::from_cbor(bytes, &mut out, &mut PgCtx::new()).or_raise(src);
    CborOut(out.into_varlena())
}

#[pg_extern(sql = false)]
fn cbor_send(c: Cbor<'_>) -> ByteaOut {
    let d = parse_datum(c);
    ByteaOut(body_bytea(&d))
}

/// The body as a `bytea` (deterministic CBOR).
pub fn body_bytea(d: &Datum) -> VarlenaOut {
    let mut out = PallocBuf::varlena(d.body.len());
    postcbor_core::OutBuf::write_bytes(&mut out, d.body).or_raise(Source::Stored);
    VarlenaOut(out.into_varlena())
}
