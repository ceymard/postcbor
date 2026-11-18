# Rust and pgrx, not C

Untrusted bytes reach the decoder over the internet (web apps pass request bodies to `::cbor` and the binary protocol). In C, a parsing bug that fuzzing has not found yet is memory corruption: a crash restarts every backend, and an out-of-bounds read can leak other rows. We write everything in Rust using pgrx, so the parsing and format code is memory-safe by construction, and pgrx owns the boundary with PostgreSQL (error jumps turned into panics and back, memory contexts, set-returning functions).

## Considered options

- **C with PGXS**, like jsonb. It has the simplest build and no dependencies, but its safety depends only on discipline and testing.
- **A Rust core as a static library, with C glue.** Rejected: the hand-written boundary (opaque handles, allocator wiring, interrupt callbacks) was judged too hacky to maintain.

## Consequences

- Building from source needs Rust, `cargo-pgrx` at the version matching the `pgrx` crate, and clang, libclang and llvm-ar from one LLVM release. `make install` runs `cargo pgrx install`. Crates are fetched from crates.io.
- Supported PostgreSQL majors follow pgrx (0.20 covers 15–18).
- `unsafe` is allowed only in modules that talk to raw `pg_sys` APIs (varlena type, subscripting, GIN).
- A panic becomes a SQL `ERROR`, never a backend crash.
