# postbor

A PostgreSQL extension that adds a `cbor` type, the CBOR counterpart of `jsonb`. Written in Rust with pgrx.

- `docs/content/design.md`: the specification. Read it before changing behaviour, and update it in the same change.
- `TODO.md`: the implementation plan. Keep it up to date as steps are done.
- `docs/adr/`: decisions and their reasons. Read the relevant ADR before going against one; ask the user before reversing it.
- `CONTEXT.md`: domain terms. Use them in code, comments and docs.

# General instructions

- Write tests for project-specific behaviour, business rules and edge cases that could realistically break. Leave the language, libraries and trivial code untested. A test's title is a claim its assertions must prove. Check the behaviour isn't already tested elsewhere before adding a test.
- Fix root causes rather than patching around structural problems. If the fix is structurally important, ask the user before making it.
- Test your claims instead of only recalling them when your confidence is not impeccable.
- DRY: keep the code simple and free of repetition.
  - Before writing a helper, search for an existing one that does the same thing.
  - Before writing a feature, trace how the closest existing feature does the same job and build on that path. If it would need restructuring to fit, ask first. In your reply, say which path you reused, or why you did not.
  - When you extend shared code, extract the common part rather than adding a parameter that every caller sets to a constant. Existing callers must keep their behaviour.
- Implementations must be the most efficient in CPU and memory. If that conflicts with the previous rule, ask the user.
- Ask the user before any important architectural decision (adding or removing a dependency, implementation approach, performance trade-offs). The approved dependencies are listed in design §11; anything else needs approval.
- Comments are terse. Only comment project-specific behaviour, business rules and edge cases: state the constraint and its reason, then stop. Leave out explanations of the language or a library, lists of what breaks if the rule is violated, arguments against alternatives (unless one was tried and failed), and spec citations.
- Function comments: one or two sentences. Public items are the exception: state what the caller cannot see in the signature (distinguishable errors, ownership, panics, lifetimes), not how it works.
- When replying or writing specs, docs or comments, use plain English, no jargon, no buzzwords.
- Update the docs (`docs/content`) as you work on the code; they must always be accurate.
- Keep previously built docs (`docs/site`) in place when testing with zensical; deleting them breaks the live server.
- In markdown, write each paragraph on one line; editors soft-wrap.
- Before declaring done: `cargo clippy --all-targets -- -D warnings`, `make test`, and `make miri` if `core/` changed.

# Rust core (`core/`)

Every byte that comes from a user is hostile. The core is where it is parsed, so these rules are strict:

- `#![forbid(unsafe_code)]`, and no dependencies.
- Read bytes only through `cursor`. Elsewhere, use cursor methods rather than slice indexing or pointer arithmetic on input or body bytes.
- Iterate with an explicit stack, limited by `max_depth`; core functions never recurse.
- Use checked arithmetic (`checked_add`, `checked_mul`, …) for every offset and length.
- Use only exact float operations: basic arithmetic and `float::pow2`, never `powi`/`powf`.
- Allocate buffers sized by input with `try_reserve`, after checking the declared length against the bytes remaining.
- Write every output through the `OutBuf` trait, never into a buffer that the glue would then have to copy.
- Return `Result<_, Error>` for every condition the input can cause. A panic means a bug.
- Loops over input call `ctx.check_interrupts()` periodically.

# pgrx glue (`postbor/`)

- `unsafe` lives only in the modules listed in design §9 (raw PostgreSQL APIs). Each `unsafe` block gets a one-line comment stating its invariant.
- Raise errors in expression position with `error::fail` (the `ereport!` macro cannot be used as an expression).
- Functions that need `VARIADIC "any"`, `internal` states, polymorphic rows or set materialization use `#[pg_extern(sql = false)]` taking the raw `FunctionCallInfo`, with their SQL written in an `extension_sql!` block that `requires = ["cbor_type"]`.
- Return core output through `PallocBuf`, so results land in PostgreSQL memory without a copy.
- Text arguments are UTF-8: the extension refuses non-UTF-8 databases (ADR-0010).
- Functions that sorts or hash joins call repeatedly (comparison, hashing) free their detoasted copies with `Cbor::free_copy`.
- Get the `cbor` type OID from `datum::cbor_oid()` (schema-independent, invalidated on `pg_type` changes), or from the call site when one is at hand.
- Byte-level logic belongs in the core. The glue converts PostgreSQL types, registers SQL objects and raises errors.
- A borrowed `cbor` value must outlive anything that borrows from it. For set-returning functions, keep the detoasted datum in the multi-call memory context.

# Tests

- Core: unit tests next to the code, and property tests as fuzz targets (`fuzz/`). Commit new fuzz corpus entries and crash reproducers as regression cases.
- SQL behaviour: `pg_regress` suites in `postbor/tests/pg_regress/`, run against pgrx-managed PostgreSQL 15–18 (`make test-matrix`; register them once with `cargo pgrx init`).
