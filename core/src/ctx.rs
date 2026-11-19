/// Input limits, from the `postcbor.*` settings.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_depth: usize,
    pub max_expansion_ratio: usize,
    pub max_expanded_size: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_depth: 512,
            max_expansion_ratio: 16,
            max_expanded_size: 256 << 20,
        }
    }
}

impl Limits {
    /// Largest body allowed when normalizing `input_len` bytes.
    pub fn max_body(&self, input_len: usize) -> usize {
        input_len
            .saturating_mul(self.max_expansion_ratio)
            .min(self.max_expanded_size)
            // Normalization of tiny inputs can legitimately grow by a few bytes (heads of joined chunks).
            .max(input_len.saturating_add(16))
    }
}

/// Environment of a core operation.
pub trait Ctx {
    fn limits(&self) -> &Limits;
    /// Called every [`INTERRUPT_EVERY`] items; may unwind to cancel the operation.
    fn check_interrupts(&mut self);
}

pub const INTERRUPT_EVERY: u32 = 4096;

/// A context with fixed limits and no interrupt source, for tests and fuzzing.
#[derive(Default)]
pub struct PlainCtx(pub Limits);


impl Ctx for PlainCtx {
    fn limits(&self) -> &Limits {
        &self.0
    }
    fn check_interrupts(&mut self) {}
}

static INTERRUPT_HOOK: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// Sets the function long-running loops call to allow cancellation (the glue's interrupt check). Set once.
pub fn set_interrupt_hook(f: fn()) {
    let _ = INTERRUPT_HOOK.set(f);
}

/// Counts loop iterations and calls the interrupt hook every [`INTERRUPT_EVERY`] of them.
#[derive(Default)]
pub struct Ticker(u32);

impl Ticker {
    #[inline]
    pub fn tick(&mut self) {
        self.0 = self.0.wrapping_add(1);
        if self.0.is_multiple_of(INTERRUPT_EVERY)
            && let Some(f) = INTERRUPT_HOOK.get() {
                f();
            }
    }
}
