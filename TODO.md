# TODO

Implementation plan. Read `docs/content/design.md` first; it is the specification, and section numbers below refer to it. Check off each step as it is done. A step is done when its tests pass, `cargo clippy --all-targets -- -D warnings` is clean, and the docs reflect it.

## Phase 0: skeleton

Notes from implementation:
- The core's `writer` module is split: heads in `encode`, building in `edit`, index in `index`/`datum`.
- jsonb conversion goes through JSON text (`json` module → `jsonb_in`; `jsonb_out` → EDN parser) rather than `pushJsonbValue`; replace if benchmarks show it matters.
- Builders and aggregates produce raw CBOR and normalize it once (`Duplicates::KeepLast`); embedded `cbor` values are re-validated. Possible optimization: skip validation of trusted bodies.
- Set-returning functions allocate each row in the multi-call memory context (pgrx default); fine for now, revisit for very large results.

- [x] Cargo workspace: `core/` (`postbor-core`, `#![forbid(unsafe_code)]`), `postbor/` (pgrx 0.20, features `pg15`–`pg18`), `fuzz/` (excluded from the default build). Pin the toolchain in `rust-toolchain.toml` (stable, edition 2024). Release profile: `overflow-checks = true`, `panic = "unwind"`.
- [x] `Makefile`: `install` (`cargo pgrx install --release --pg-config $(PG_CONFIG)`, with `PG_CONFIG ?= pg_config` and the pg feature picked from `pg_config --version`), `test` (core tests, then pg_regress against the local PG), `test-matrix` (pgrx-managed PG 15–18), `fuzz` (each target, with a time budget), `miri`. Done when `make && make install` then `CREATE EXTENSION postbor` works on the local PG 18. Verified from scratch on the `postgres:18` Docker image (distro PostgreSQL, needs `libicu-dev`).
- [ ] Set up CI (there is none yet) so it runs `make test-matrix`, `make miri`, and a short `make fuzz` on every change.
- [ ] Set up a zensical site for `docs/content` (`zensical.toml`) and add user docs pages as features land.

## Phase 1: core

- [x] `cursor`: bounds-checked reads, head decoding, checked length arithmetic.
- [x] `normalize` (without packing): design §3 rules 1–10, limits from §2. Tests cover each rule and each rejection.
- [x] `OutBuf` trait (§9) with the `Vec<u8>` implementation. Every core output goes through it.
- [x] `writer` + `index`: the deterministic encoder that builds the index section (§4); lookup, extraction and rebasing. Tests at threshold boundaries.
- [x] `value`: borrowed views and iterators.
- [x] `cmp`: §7 ordering. Property fuzz target for the ordering laws.
- [x] `edn`: parser and printer (§5). Property fuzz target for the EDN round trip.
- [x] `ops`: path get, containment, `?` family, concatenation, delete, set/insert (`create_if_missing`, `insert_after`), set_lax, strip_nulls.
- [x] `json`: conversion rules for both directions (§8 casts, ADR-0009).
- [ ] `walk`: typed (path, value) pairs (needed by phase 2).
- [x] Fuzz targets: normalize, EDN parse, and the properties of §10. Seed corpus from RFC 8949 Appendix A.
- [x] `cbor2` cross-check script (test-only): `uv run --with cbor2 tests/crosscheck.py [count]`.
- [x] Packing: unpack the schemes in design §6, with limits and cycle detection; `pack` output for both formats. Property: normalization is idempotent.
- [ ] Packing gaps: cbor-x tag 51 prefix/suffix tables (cbor-x's encoder leaves them empty), IETF integration tag 1115 and stand-in items beyond references, cbor-x bundled strings (57337).

## Phase 1: glue

- [x] `PallocBuf`: the `OutBuf` implementation that writes straight into a `palloc` varlena (§9).
- [x] `datum`: the `cbor` varlena type, borrowed (detoasted) and owned forms, `FromDatum`/`IntoDatum`/`SqlTranslatable`. Look up how pgrx 0.20 examples define custom varlena types.
- [x] Settings (§2) registered in `_PG_init`. Mapping from core errors to SQLSTATEs (§2).
- [x] I/O: `cbor_in`, `cbor_out`, `cbor_recv`, `cbor_send`, (UTF-8 databases only, ADR-0010).
- [x] Comparison operators, btree and hash operator classes.
- [x] Access operators (`-> ->> #> #>>`) with every overload in §8. The pg_regress suite checks that `doc -> 'a'` and `doc #> '{a}'` resolve to the text overloads.
- [x] `@> <@ ? ?| ?& || - #-` with every overload and selectivity estimator.
- [x] Typed-key functions (`cbor_get*`, `cbor_has*`, `cbor_delete*`, `cbor_*_path`).
- [x] Casts (§8 table) and `to_cbor` (§8 mapping), including records, arrays and domains (`check_stack_depth` on nesting).
- [x] jsonb conversion module (`pushJsonbValue` / `JsonbIteratorNext`), and `cbor_to_jsonb_strict`.
- [x] Builders and aggregates (`cbor_build_*`, `cbor_object`, `cbor_agg*`, `cbor_object_agg*`).
- [x] Set-returning functions (`cbor_each*`, `cbor_array_elements*`, `cbor_object_keys`) in value-per-call mode.
- [x] `cbor_populate_record*`, `cbor_to_record*`.
- [ ] `cbor_populate_record_valid` (needs soft input errors, PG 16+; decide behaviour on PG 15).
- [x] Modifying functions (`cbor_set`, `cbor_set_lax`, `cbor_insert`, `cbor_strip_nulls`), `cbor_pretty`, `cbor_typeof`, `cbor_array_length`, `cbor_extract_path*`.
- [x] CBOR-specific functions (§8).
- [x] Subscripting handler.
- [x] pg_regress suites for each of the above, passing on PG 15–18 (`io`, `operators`, `functions`, `records`, `subscript`, `packing`).
- [x] First benchmark against jsonb (`bench/lookup.sql`, release build, PG 18, 2026-11-18):
  - 200k small documents: cbor 15 MB vs jsonb 27 MB; `->>` and `@>` about 20% slower than jsonb; text output 2× faster; `DISTINCT` 3.3× faster.
  - 2k documents of 1000 keys: one-key lookup 1.6× faster than jsonb, but cbor is larger (31 MB vs 19 MB): the index section (4 bytes per entry) adds ~37% to the body and keeps values below pglz's 25% gain, so they are stored uncompressed while jsonb compresses.
- [ ] Shrink the index section (needs the user's decision, changes the datum format): e.g. u16 offsets for containers under 64 KB, and/or one offset every 8 entries (binary search, then a short scan). Then re-run the benchmark and tune `INDEX_THRESHOLD`.
- [ ] Find why `->>`/`@>` on small documents trail jsonb by ~20% (per-call overhead in the glue is the first suspect).

## Promised in the design, not done yet

- [x] Debug builds re-validating every stored datum they read (design §2): `io::parse_datum` re-normalizes and compares; every pg_regress run uses it.
- [ ] `walk` in the core (typed path/value pairs; needed by phase 2).
- [x] Combine/serialize functions for the aggregates (parallel aggregation), tested with a forced parallel plan.
- [ ] cbor-x compatibility checked against real cbor-x output (current fixtures were written from reading cbor-x's source; needs Node.js and cbor-x, ask the user first).
- [x] Interrupt checks in long operations: normalization, unpacking and containment through `Ctx`; comparison, EDN/JSON printing and strip_nulls through the core's interrupt hook (`ctx::Ticker`).

## Phase 2: GIN

- [ ] Design session with the user: key forms (`cbor_ops` / `cbor_path_ops`), non-text keys, tags, hashing.
- [ ] Implement, test and document.

## Phase 3: path queries

- [ ] Run core `jsonpath` over `cbor` (ADR-0008): `@?`, `@@`, `cbor_path_exists/match/query/query_array/query_first`, plus the `_tz` variants.
- [ ] GIN support for `@?` / `@@`, if phase 2's design allows it.
