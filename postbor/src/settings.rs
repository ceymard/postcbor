//! `postbor.*` settings (design §2) and the core context built from them.

use pgrx::guc::{GucContext, GucFlags, GucRegistry, GucSetting};
use postbor_core::{Ctx, Limits};

static MAX_DEPTH: GucSetting<i32> = GucSetting::<i32>::new(512);
static MAX_EXPANSION_RATIO: GucSetting<i32> = GucSetting::<i32>::new(16);
static MAX_EXPANDED_SIZE_KB: GucSetting<i32> = GucSetting::<i32>::new(256 * 1024);

pub fn register() {
    // Superuser-only: callers reaching SQL through an application must not be able to raise them.
    GucRegistry::define_int_guc(
        c"postbor.max_depth",
        c"Maximum nesting depth of a cbor value.",
        c"Each array, map and tag counts as one level.",
        &MAX_DEPTH,
        1,
        100_000,
        GucContext::Suset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"postbor.max_expansion_ratio",
        c"Maximum size of a normalized cbor value relative to its input.",
        c"Bounds expansion of packed input.",
        &MAX_EXPANSION_RATIO,
        1,
        1_000_000,
        GucContext::Suset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"postbor.max_expanded_size",
        c"Maximum size of a normalized cbor value.",
        c"Bounds expansion of packed input.",
        &MAX_EXPANDED_SIZE_KB,
        1,
        1024 * 1024,
        GucContext::Suset,
        GucFlags::UNIT_KB,
    );
}

/// Core context: current settings, and query cancellation through PostgreSQL's interrupt check.
pub struct PgCtx(Limits);

impl PgCtx {
    pub fn new() -> Self {
        PgCtx(Limits {
            max_depth: MAX_DEPTH.get() as usize,
            max_expansion_ratio: MAX_EXPANSION_RATIO.get() as usize,
            max_expanded_size: (MAX_EXPANDED_SIZE_KB.get() as usize).saturating_mul(1024),
        })
    }
}

impl Ctx for PgCtx {
    fn limits(&self) -> &Limits {
        &self.0
    }
    fn check_interrupts(&mut self) {
        pgrx::check_for_interrupts();
    }
}
