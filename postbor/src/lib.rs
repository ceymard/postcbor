//! postbor: a `cbor` type with jsonb-like operators and functions. Byte-level work lives in `postbor-core`.

::pgrx::pg_module_magic!(name, version);

mod datum;
mod error;
mod funcs;
mod access;
mod build;
mod compare;
mod convert;
mod io;
mod modify;
mod record;
mod settings;
mod subscript;

#[pgrx::pg_guard]
pub extern "C-unwind" fn _PG_init() {
    settings::register();
    datum::register_invalidation();
    postbor_core::ctx::set_interrupt_hook(pgrx::check_for_interrupts);
}
