//! The `cbor` varlena type: borrowed arguments, results built in place in `palloc` memory.

use core::ffi::{CStr, c_char};
use core::ptr;
use std::sync::atomic::{AtomicU32, Ordering};

use pgrx::callconv::{Arg, ArgAbi, BoxRet, FcInfo};
use pgrx::datum::Datum as PgDatum;
use pgrx::pg_sys;
use pgrx::pgrx_sql_entity_graph::metadata::{
    ArgumentError, ReturnsError, ReturnsRef, SqlMappingRef, SqlTranslatable, TypeOrigin,
};
use pgrx::prelude::*;
use postbor_core::{Error, ErrorKind, OutBuf, Result};

const VARHDRSZ: usize = 4;

/// A `cbor` argument: the detoasted datum bytes (header, body, index section).
#[derive(Clone, Copy)]
pub struct Cbor<'a>(pub &'a [u8], *mut pg_sys::varlena);

impl Cbor<'_> {
    /// Frees the detoasted copy, if detoasting made one. For functions a sort or hash calls many times in one
    /// memory context (comparison, hashing); the bytes must not be used afterwards.
    pub fn free_copy(self) {
        if !self.1.is_null() {
            // SAFETY: `self.1` was palloc'd by detoasting and nothing else refers to it.
            unsafe { pg_sys::pfree(self.1.cast()) };
        }
    }
}

impl FromDatum for Cbor<'_> {
    unsafe fn from_polymorphic_datum(datum: pg_sys::Datum, is_null: bool, _: pg_sys::Oid) -> Option<Self> {
        if is_null || datum.is_null() {
            return None;
        }
        // SAFETY: a non-null `cbor` datum is a varlena.
        let (bytes, copy) = unsafe { detoast(datum) };
        Some(Cbor(bytes, copy))
    }
}

/// Payload of a varlena datum, and the detoasted copy when one was made (else null). Inline values are read in
/// place without calling into PostgreSQL: the guarded call costs more than most operators' own work.
///
/// # Safety
/// `datum` must point to a varlena that lives for `'a`.
unsafe fn detoast<'a>(datum: pg_sys::Datum) -> (&'a [u8], *mut pg_sys::varlena) {
    let orig = datum.cast_mut_ptr::<pg_sys::varlena>();
    // SAFETY: every varlena starts with at least one header byte.
    let first = unsafe { *orig.cast::<u8>() };
    // Compressed (4-byte header, VARATT_IS_4B_C) or out of line / expanded (1-byte header 0x01, VARATT_IS_1B_E).
    let extended = if cfg!(target_endian = "little") {
        first == 0x01 || first & 0x03 == 0x02
    } else {
        first == 0x80 || first & 0xc0 == 0x40
    };
    // SAFETY: the varlena is valid; detoasting returns a varlena in the current memory context.
    unsafe {
        let v = if extended { pg_sys::pg_detoast_datum_packed(orig) } else { orig };
        let copy = if v == orig { core::ptr::null_mut() } else { v };
        (pgrx::varlena_to_byte_slice(v), copy)
    }
}

/// A `text[]` argument, its elements borrowed in place (NULL elements are None; dimensions are flattened). Text's
/// storage layout is fixed, so the array is split without the catalog lookup pgrx's `Array` makes on every call.
pub struct TextArray<'a>(Vec<Option<&'a str>>);

impl<'a> TextArray<'a> {
    pub fn iter(&self) -> impl Iterator<Item = Option<&'a str>> + '_ {
        self.0.iter().copied()
    }
}

impl FromDatum for TextArray<'_> {
    unsafe fn from_polymorphic_datum(datum: pg_sys::Datum, is_null: bool, _: pg_sys::Oid) -> Option<Self> {
        if is_null || datum.is_null() {
            return None;
        }
        let orig = datum.cast_mut_ptr::<pg_sys::varlena>();
        // SAFETY: a non-null `text[]` datum is a varlena; ArrayType needs a plain 4-byte header (VARATT_IS_4B_U),
        // anything else (short, compressed, external, expanded) is flattened by PostgreSQL.
        let arr = unsafe {
            let first = *orig.cast::<u8>();
            let plain = if cfg!(target_endian = "little") { first & 0x03 == 0 } else { first & 0xc0 == 0 };
            if plain { orig } else { pg_sys::pg_detoast_datum(orig) }
        };
        let (mut elems, mut nulls, mut n) = (ptr::null_mut(), ptr::null_mut(), 0);
        // SAFETY: `arr` is a flat array of text; the element pointers point into it and live as long.
        unsafe {
            pg_sys::deconstruct_array(arr.cast(), pg_sys::TEXTOID, -1, false, pg_sys::TYPALIGN_INT as c_char, &mut elems, &mut nulls, &mut n);
        }
        let n = n.max(0) as usize;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            // SAFETY: deconstruct_array returned `n` elements and `n` null flags.
            let (d, null) = unsafe { (*elems.add(i), *nulls.add(i)) };
            out.push(if null {
                None
            } else {
                // SAFETY: a non-null element is a text varlena inside the array.
                let (b, _) = unsafe { detoast(d) };
                // Text in a UTF-8 database is valid UTF-8 (ADR-0010).
                Some(core::str::from_utf8(b).unwrap_or_else(|_| crate::error::fail(PgSqlErrorCode::ERRCODE_CHARACTER_NOT_IN_REPERTOIRE, "invalid UTF-8 in text array")))
            });
        }
        Some(TextArray(out))
    }
}

unsafe impl<'fcx> ArgAbi<'fcx> for TextArray<'fcx> {
    unsafe fn unbox_arg_unchecked(arg: Arg<'_, 'fcx>) -> Self {
        let index = arg.index();
        // SAFETY: pgrx calls this only for arguments declared as `text[]`.
        unsafe { arg.unbox_arg_using_from_datum().unwrap_or_else(|| panic!("argument {index} must not be null")) }
    }
}

/// A `text` or `bytea` argument borrowed as bytes. Text is UTF-8 (ADR-0010), so it is not re-checked.
macro_rules! bytes_arg {
    ($arg:ident, $sql:literal) => {
        #[derive(Clone, Copy)]
        pub struct $arg<'a>(pub &'a [u8]);

        impl FromDatum for $arg<'_> {
            unsafe fn from_polymorphic_datum(datum: pg_sys::Datum, is_null: bool, _: pg_sys::Oid) -> Option<Self> {
                // SAFETY: a non-null argument of this type is a varlena that lives for the call.
                (!is_null && !datum.is_null()).then(|| $arg(unsafe { detoast(datum) }.0))
            }
        }

        unsafe impl<'fcx> ArgAbi<'fcx> for $arg<'fcx> {
            unsafe fn unbox_arg_unchecked(arg: Arg<'_, 'fcx>) -> Self {
                let index = arg.index();
                // SAFETY: pgrx calls this only for arguments declared with this SQL type.
                unsafe { arg.unbox_arg_using_from_datum().unwrap_or_else(|| panic!("argument {index} must not be null")) }
            }
        }

        sql_type!($arg<'_>, $sql, TypeOrigin::External);
    };
}

/// A `cbor` view over bytes that are not a datum of their own (constants, subscript values).
pub fn cbor_bytes(b: &[u8]) -> Cbor<'_> {
    Cbor(b, core::ptr::null_mut())
}

unsafe impl<'fcx> ArgAbi<'fcx> for Cbor<'fcx> {
    unsafe fn unbox_arg_unchecked(arg: Arg<'_, 'fcx>) -> Self {
        let index = arg.index();
        // SAFETY: pgrx calls this only for arguments declared as `cbor`.
        unsafe { arg.unbox_arg_using_from_datum().unwrap_or_else(|| panic!("argument {index} must not be null")) }
    }
}

/// A `cbor` result: a varlena already in PostgreSQL memory.
pub struct CborOut(pub *mut pg_sys::varlena);

impl IntoDatum for CborOut {
    fn into_datum(self) -> Option<pg_sys::Datum> {
        Some(pg_sys::Datum::from(self.0))
    }
    fn type_oid() -> pg_sys::Oid {
        cbor_oid()
    }
}

unsafe impl BoxRet for CborOut {
    unsafe fn box_into<'fcx>(self, fcinfo: &mut FcInfo<'fcx>) -> PgDatum<'fcx> {
        // SAFETY: the pointer is a complete varlena allocated in the current memory context.
        unsafe { fcinfo.return_raw_datum(pg_sys::Datum::from(self.0)) }
    }
}

/// A `text` or `bytea` result built in place (same varlena layout as `CborOut`).
pub struct VarlenaOut(pub *mut pg_sys::varlena);

unsafe impl BoxRet for VarlenaOut {
    unsafe fn box_into<'fcx>(self, fcinfo: &mut FcInfo<'fcx>) -> PgDatum<'fcx> {
        // SAFETY: as for `CborOut`.
        unsafe { fcinfo.return_raw_datum(pg_sys::Datum::from(self.0)) }
    }
}

macro_rules! sql_type {
    ($ty:ty, $sql:literal, $origin:expr) => {
        unsafe impl SqlTranslatable for $ty {
            const TYPE_IDENT: &'static str = pgrx::pgrx_resolved_type!($ty);
            const TYPE_ORIGIN: TypeOrigin = $origin;
            const ARGUMENT_SQL: core::result::Result<SqlMappingRef, ArgumentError> = Ok(SqlMappingRef::literal($sql));
            const RETURN_SQL: core::result::Result<ReturnsRef, ReturnsError> =
                Ok(ReturnsRef::One(SqlMappingRef::literal($sql)));
        }
    };
}

sql_type!(Cbor<'_>, "cbor", TypeOrigin::ThisExtension);
sql_type!(CborOut, "cbor", TypeOrigin::ThisExtension);
pub struct TextOut(pub VarlenaOut);
pub struct ByteaOut(pub VarlenaOut);
sql_type!(TextOut, "text", TypeOrigin::External);
bytes_arg!(TextArg, "text");
bytes_arg!(ByteaArg, "bytea");
bytes_arg!(JsonPathArg, "jsonpath");
sql_type!(TextArray<'_>, "text[]", TypeOrigin::External);
sql_type!(ByteaOut, "bytea", TypeOrigin::External);

impl IntoDatum for TextOut {
    fn into_datum(self) -> Option<pg_sys::Datum> {
        Some(pg_sys::Datum::from((self.0).0))
    }
    fn type_oid() -> pg_sys::Oid {
        pg_sys::TEXTOID
    }
}

unsafe impl BoxRet for TextOut {
    unsafe fn box_into<'fcx>(self, fcinfo: &mut FcInfo<'fcx>) -> PgDatum<'fcx> {
        unsafe { self.0.box_into(fcinfo) }
    }
}

unsafe impl BoxRet for ByteaOut {
    unsafe fn box_into<'fcx>(self, fcinfo: &mut FcInfo<'fcx>) -> PgDatum<'fcx> {
        unsafe { self.0.box_into(fcinfo) }
    }
}

static CBOR_OID: AtomicU32 = AtomicU32::new(0);

/// OID of the `cbor` type, found through the extension (whatever its schema and the search_path).
/// Cached; any change to `pg_type` resets the cache, so dropping and recreating the extension is safe.
pub fn cbor_oid() -> pg_sys::Oid {
    let cached = CBOR_OID.load(Ordering::Relaxed);
    if cached != 0 {
        return pg_sys::Oid::from(cached);
    }
    // SAFETY: read-only catalog access (allowed in parallel workers): pg_extension by OID, then the type by name
    // in the extension's schema.
    let oid = unsafe {
        let ext = pg_sys::get_extension_oid(c"postbor".as_ptr(), true);
        if ext == pg_sys::InvalidOid {
            return pg_sys::InvalidOid;
        }
        let rel = pg_sys::table_open(pg_sys::ExtensionRelationId, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
        let mut key = pg_sys::ScanKeyData::default();
        pg_sys::ScanKeyInit(
            &mut key,
            pg_sys::Anum_pg_extension_oid as pg_sys::AttrNumber,
            pg_sys::BTEqualStrategyNumber as pg_sys::StrategyNumber,
            pg_sys::RegProcedure::from(pg_sys::F_OIDEQ),
            pg_sys::Datum::from(ext),
        );
        let scan = pg_sys::systable_beginscan(rel, pg_sys::Oid::from(pg_sys::ExtensionOidIndexId), true, core::ptr::null_mut(), 1, &mut key);
        let tup = pg_sys::systable_getnext(scan);
        let nsp = if tup.is_null() {
            pg_sys::InvalidOid
        } else {
            let header = (*tup).t_data;
            let form = (header as *const u8).add((*header).t_hoff as usize) as *const pg_sys::FormData_pg_extension;
            (*form).extnamespace
        };
        pg_sys::systable_endscan(scan);
        pg_sys::table_close(rel, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
        pg_sys::GetSysCacheOid(
            pg_sys::SysCacheIdentifier::TYPENAMENSP as core::ffi::c_int,
            pg_sys::Anum_pg_type_oid as pg_sys::AttrNumber,
            pg_sys::Datum::from(c"cbor".as_ptr()),
            pg_sys::Datum::from(nsp),
            pg_sys::Datum::from(0usize),
            pg_sys::Datum::from(0usize),
        )
    };
    CBOR_OID.store(oid.to_u32(), Ordering::Relaxed);
    oid
}

#[pgrx::pg_guard]
unsafe extern "C-unwind" fn reset_cbor_oid(_arg: pg_sys::Datum, _cacheid: core::ffi::c_int, _hash: u32) {
    CBOR_OID.store(0, Ordering::Relaxed);
}

/// Registers the `pg_type` invalidation callback that resets the cached `cbor` OID.
pub fn register_invalidation() {
    // SAFETY: registered once per backend from _PG_init; the callback only resets an atomic.
    unsafe {
        pg_sys::CacheRegisterSyscacheCallback(
            pg_sys::SysCacheIdentifier::TYPEOID as core::ffi::c_int,
            Some(reset_cbor_oid),
            pg_sys::Datum::from(0usize),
        )
    };
}

/// Releases a tuple descriptor obtained from the type cache (`ReleaseTupleDesc`).
///
/// # Safety
/// `desc` must come from `lookup_rowtype_tupdesc*` and not be released twice.
pub unsafe fn release_tupdesc(desc: pg_sys::TupleDesc) {
    // SAFETY: only reference-counted descriptors are released, as ReleaseTupleDesc does.
    unsafe {
        if (*desc).tdrefcount >= 0 {
            pg_sys::DecrTupleDescRefCount(desc);
        }
    }
}

/// Total size of a detoasted (4-byte header) varlena.
///
/// # Safety
/// `v` must point to an uncompressed varlena with a 4-byte header.
pub unsafe fn varsize_4b(v: *const u8) -> u32 {
    // SAFETY: the caller guarantees a 4-byte header.
    let h = unsafe { ptr::read_unaligned(v as *const u32) };
    if cfg!(target_endian = "little") { (h >> 2) & 0x3fff_ffff } else { h & 0x3fff_ffff }
}

/// A 4-byte varlena header for a value of `total` bytes (header included), as SET_VARSIZE writes it.
pub fn varlena_header(total: usize) -> [u8; 4] {
    let total = total as u32;
    let h = if cfg!(target_endian = "little") { total << 2 } else { total & 0x3fff_ffff };
    h.to_ne_bytes()
}

/// Output buffer in `palloc` memory of the current context, with room for a varlena header when `hdr` is 4.
pub struct PallocBuf {
    ptr: *mut u8,
    hdr: usize,
    len: usize,
    cap: usize,
}

impl PallocBuf {
    fn with_header(hdr: usize, capacity: usize) -> Self {
        let cap = capacity.clamp(16, pg_sys::MaxAllocSize - hdr - 1);
        // One spare byte past `cap` keeps room for a C string terminator.
        // SAFETY: palloc returns a block of the requested size or raises an error.
        let ptr = unsafe { pg_sys::palloc(hdr + cap + 1) as *mut u8 };
        PallocBuf { ptr, hdr, len: 0, cap }
    }

    /// For results returned as varlena datums.
    pub fn varlena(capacity: usize) -> Self {
        Self::with_header(VARHDRSZ, capacity)
    }

    /// For NUL-terminated C strings.
    pub fn cstring(capacity: usize) -> Self {
        Self::with_header(0, capacity)
    }

    /// For raw blocks returned through `internal` (GIN entries).
    pub fn raw(capacity: usize) -> Self {
        Self::with_header(0, capacity)
    }

    /// The block, valid until its memory context is reset.
    pub fn into_ptr(self) -> *mut u8 {
        self.ptr
    }

    fn reserve(&mut self, extra: usize) -> Result<()> {
        let need = self.len.checked_add(extra).ok_or(Error::new(ErrorKind::TooLarge, self.len))?;
        if need <= self.cap {
            return Ok(());
        }
        let limit = pg_sys::MaxAllocSize - self.hdr - 1;
        if need > limit {
            return Err(Error::new(ErrorKind::TooLarge, self.len));
        }
        let cap = need.max(self.cap.saturating_mul(2)).min(limit);
        // SAFETY: `ptr` came from palloc/repalloc; repalloc keeps its memory context.
        self.ptr = unsafe { pg_sys::repalloc(self.ptr.cast(), self.hdr + cap + 1) as *mut u8 };
        self.cap = cap;
        Ok(())
    }

    pub fn into_varlena(self) -> *mut pg_sys::varlena {
        debug_assert_eq!(self.hdr, VARHDRSZ);
        let v = self.ptr as *mut pg_sys::varlena;
        let header = varlena_header(self.hdr + self.len);
        // SAFETY: the block starts with VARHDRSZ reserved bytes followed by `len` payload bytes.
        unsafe { ptr::copy_nonoverlapping(header.as_ptr(), self.ptr, VARHDRSZ) };
        v
    }

    pub fn into_cstr(self) -> &'static CStr {
        debug_assert_eq!(self.hdr, 0);
        // SAFETY: capacity always keeps one spare byte; the C string lives until its memory context is reset.
        unsafe {
            *self.ptr.add(self.len) = 0;
            CStr::from_ptr(self.ptr as *const c_char)
        }
    }
}

impl OutBuf for PallocBuf {
    fn len(&self) -> usize {
        self.len
    }
    fn as_slice(&self) -> &[u8] {
        // SAFETY: `len` bytes after the header are initialized.
        unsafe { core::slice::from_raw_parts(self.ptr.add(self.hdr), self.len) }
    }
    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as above, and we hold the only reference.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.add(self.hdr), self.len) }
    }
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.reserve(bytes.len())?;
        // SAFETY: `reserve` guaranteed room for `bytes.len()` more bytes; the regions cannot overlap.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.add(self.hdr + self.len), bytes.len()) };
        self.len += bytes.len();
        Ok(())
    }
    fn truncate(&mut self, len: usize) {
        self.len = self.len.min(len);
    }
}

/// An `internal` result: a pointer the caller knows how to read (aggregate states, GIN entry arrays).
pub struct InternalOut(pub pg_sys::Datum);

unsafe impl BoxRet for InternalOut {
    unsafe fn box_into<'fcx>(self, fcinfo: &mut FcInfo<'fcx>) -> PgDatum<'fcx> {
        // SAFETY: the pointer lives in a memory context that outlives the call (aggregate or index build/scan).
        unsafe { fcinfo.return_raw_datum(self.0) }
    }
}

sql_type!(InternalOut, "internal", TypeOrigin::External);

/// A datum passed through untouched, typed for SQL as `$sql`.
macro_rules! raw_datum {
    ($arg:ident, $out:ident, $sql:literal) => {
        #[derive(Clone, Copy)]
        pub struct $arg(pub pg_sys::Datum);

        impl FromDatum for $arg {
            unsafe fn from_polymorphic_datum(datum: pg_sys::Datum, is_null: bool, _: pg_sys::Oid) -> Option<Self> {
                (!is_null).then_some($arg(datum))
            }
        }

        unsafe impl<'fcx> ArgAbi<'fcx> for $arg {
            unsafe fn unbox_arg_unchecked(arg: Arg<'_, 'fcx>) -> Self {
                let index = arg.index();
                // SAFETY: pgrx calls this only for arguments declared with this SQL type.
                unsafe { arg.unbox_arg_using_from_datum().unwrap_or_else(|| panic!("argument {index} must not be null")) }
            }
        }

        pub struct $out(pub pg_sys::Datum);

        unsafe impl BoxRet for $out {
            unsafe fn box_into<'fcx>(self, fcinfo: &mut FcInfo<'fcx>) -> PgDatum<'fcx> {
                // SAFETY: the datum was produced by this type's input function in the current context.
                unsafe { fcinfo.return_raw_datum(self.0) }
            }
        }

        sql_type!($arg, $sql, TypeOrigin::External);
        sql_type!($out, $sql, TypeOrigin::External);
    };
}

raw_datum!(JsonbArg, JsonbOut, "jsonb");
raw_datum!(JsonArg, JsonOut, "json");
