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
- [x] `walk`: `gin::walk` (map keys, atomic items and their path).
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
- [x] Shrink the index section: one offset every 8 entries, 2-byte offsets when they fit. 1000-key documents: 16 MB vs jsonb 19 MB (was 31 MB), values now compress; one-key lookup still faster than jsonb. `INDEX_THRESHOLD` stays 16 (a 10-entry map gains nothing measurable from an index).
- Benchmarks before 2026-11-19 ran on pgrx's PostgreSQL, which is built with assertions and `RANDOMIZE_ALLOCATED_MEMORY` (jsonb spent 49% of `->>` filling allocations): their cbor/jsonb ratios are unreliable. Benchmark and profile on a PostgreSQL built without `--enable-cassert`; `perf` needs `-e cycles:u --user-callchains` under `perf_event_paranoid=2`, and can only follow processes it starts (`postgres --single`).
- [ ] Small documents still trail jsonb (non-assertion PG 18, 200k rows, best of 5, ratio cbor/jsonb measured back to back, 2026-11-19): `->> 'name'` 1.46, `@> {"active": true}` 1.65, `@> {"id": 5}` 1.30, `? 'name'` 1.16, `? 'id'` 1.04 (first pass: 1.84, 2.37, 2.03, 1.39, 1.22). Text output 1.1; large-map lookups (0.8) and `DISTINCT` (0.87) are faster than jsonb.
  - Done: inline arguments read without calling into PostgreSQL to detoast; text arguments and `->>` results not re-checked for UTF-8; one-byte heads decoded without the general path; `Value` borrows its datum (16 bytes instead of 56); lookup keys are a head plus borrowed content, compared without being joined (key build 11 → 0 ns), and stored keys compared on their first byte before `memcmp`; containment frames hold positions only; `Datum::parse` inlined for unindexed values; settings read only when an operation needs them (each pgrx setting read runs a thread check).
  - What remains is mostly the format: on maps below the index threshold a lookup steps over every earlier key and value (~8 ns each, more for nested values), where jsonb binary-searches a per-container offset table. Also: pgrx's entry wrapper and the guarded `palloc` of text results (~11 ns per guarded call), and the 32-byte core `Error` inside every `Result` (~10% of containment time).

## Promised in the design, not done yet

- [x] Debug builds re-validating every stored datum they read (design §2): `io::parse_datum` re-normalizes and compares; every pg_regress run uses it.
- [x] `walk` in the core: `gin::walk`.
- [x] Combine/serialize functions for the aggregates (parallel aggregation), tested with a forced parallel plan.
- [x] cbor-x compatibility checked against real cbor-x 1.6.7, both directions (`make cborx-check CBORX_NODE_MODULES=…`). Found and fixed: record ids beyond 256, empty maps and more than 8191 shapes in `cbor_pack`.
- [x] Interrupt checks in long operations: normalization, unpacking and containment through `Ctx`; comparison, EDN/JSON printing and strip_nulls through the core's interrupt hook (`ctx::Ticker`).

## Phase 2: GIN

- [x] Design (with the user, 2026-11-19): `cbor_ops` and `cbor_path_ops` like jsonb's; `?#`, `?#|`, `?#&` operators so typed keys are indexable; tags 4 and 5 compared whole in containment, other tags walked into (path steps in `cbor_path_ops`).
- [x] Core `gin`: walk over keys and atomic items, entries for both classes; tests that contained values have a subset of entries; fuzzed (`edit` target: contained values derived from each input).
- [x] Support functions and operator classes; `gin` suite: every query gives the same rows through the index as by a sequential scan, and uses the index.
- [x] Benchmark against jsonb's classes (non-assertion PG 18, 200k small documents with a nested map, 2026-11-19): same index sizes (29 MB `cbor_ops`/`jsonb_ops`, 24 MB path classes), builds 3–5% faster. Indexed `@>` within 10–20% of jsonb, except `@> {"active": true, "addr": {"zip": 5}}` with the default class: 1.9 ms vs 10.1, because `cbor_ops` keeps array elements and map values apart (jsonb's flag for `5` also matches `tags: [.., 5]`).
- [x] Operators taking `text[]` (`?|`, `?&`, `-`, `#>`, `#-`, `cbor_set`, …) read it as `TextArray`: one `deconstruct_array` call with text's fixed layout, instead of pgrx's `Array`, which looks the element type up in the catalog on every call. `?|` and `#>>` per row: 44 → 32 ms (jsonb 15); the rest is the walk past nested values.

## Phase 3: path queries

Decided 2026-11-19 (design §8 Path queries, ADR-0008).

- [x] Decode the compiled `jsonpath` (core `jsonpath`, iterative, version check, nesting cap of 256 items) and PostgreSQL `numeric` literals (core `decimal`). `jsonpath_decode` suite: every literal of PostgreSQL's jsonpath tests that parses on the running version decodes, prints (debug-only `postbor_jsonpath_print`) and reads back to the same path, on PG 15–18; fuzz target `jsonpath`.
- [x] Differential harness: `tests/jsonpath/extract_cases.py` turns PostgreSQL's `jsonb_jsonpath.sql` (823 cases on PG 18) into a suite that runs each statement on jsonb and on a cbor copy, compares rows as jsonb (an error on both sides is equal) and prints only differences. It joins the regression suites with the evaluator.
- [x] Evaluator (core `jsonpath::eval`, a port of `jsonpath_exec.c`) and the SQL functions and operators: accessors, lax/strict, filters, arithmetic, item methods, `like_regex`, `vars`, `.keyvalue()`. On PG 18 the differential suite (`jsonpath_diff`, now a regression suite) matches 393 cases with rows and 185 with the same error.
- [x] Datetime methods, datetime comparison, `_tz` behaviour, tags 0 and 1 as datetimes. Differential suite: on PG 17–18, 555 cases match with rows and 246 with the same error; on PG 15–16, 345 and 473. The rest is decimal fractions and `.keyvalue()` ids.
- [ ] Decimal fraction results show as `[exponent, mantissa]` after `::jsonb` (question for the user).
- [ ] `.keyvalue()` ids differ from jsonb's except for objects at the top of the document (document in design §8).
- [x] Core unit tests for the evaluator (hand-built paths, Miri), fuzzing of evaluation, and regression tests at the 256-item cap (`jsonpath` suite).
- [x] Differential suite on PG 15–17: PG 17 matches PG 18; PG 15 and 16 (fewer jsonpath features) have their own expected output (`jsonpath_diff_1.out`).
- [x] Cache the decoded path per call site (`fn_extra`; set-returning functions decode once per call).
- [ ] Path operators are 3–6× slower than jsonb's on small documents (PG 18 without assertions, 200,000 rows): `@@ '$.id < 1000'` 46 ms vs 10, `@? '$.addr ? (@.city == "c1")'` 52 vs 11, `@? '$.** ? (@ == 1)'` 374 vs 60. Evaluation alone (core, plain host) takes 66 ns for `1 < 1000` and 129 ns for `$.id < 1000`; the profile shows store-forwarding stalls when path items are copied.
- [ ] GIN support for `@?` / `@@`.
