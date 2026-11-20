# Contributing

## Layout

- `core/`: the pure Rust crate (`postcbor-core`) that does every byte-level operation: parsing, normalization, EDN, comparison, containment, packing, GIN entries, jsonpath evaluation. No dependencies and no `unsafe` code.
- `postcbor/`: the pgrx extension that exposes the core to SQL: types, operators, functions, casts, settings, errors.
- `fuzz/`: cargo-fuzz targets over the core.
- `docs/content/`: this site. [`design.md`](design.md) is the specification.

In the repository, `AGENTS.md` holds the coding rules, `CONTEXT.md` the domain terms, `docs/adr/` the recorded decisions and their reasons, and `TODO.md` the implementation plan.

## Tests

| Command | Runs |
|---|---|
| `make test` | core unit tests, then the SQL suites (`postcbor/tests/pg_regress/`) against pgrx's PostgreSQL of the same major version as `PG_CONFIG` |
| `make test-matrix` | the same on PostgreSQL 15, 16, 17 and 18 (register them once with `cargo pgrx init`) |
| `make clippy` | clippy with warnings as errors |
| `make miri` | the core tests under Miri (nightly toolchain) |
| `make fuzz` | each fuzz target for `FUZZ_SECONDS` (default 60; nightly toolchain) |
| `make crosscheck` | random values encoded by Python's `cbor2`, normalized by the core and compared |
| `make cborx-check CBORX_NODE_MODULES=…` | round trips through a real cbor-x installation |

CI (GitHub Actions, `.github/workflows/ci.yml`) runs clippy and the SQL suites on each PostgreSQL version, Miri, and each fuzz target for 30 seconds.

New fuzz corpus entries and crash reproducers are committed as regression cases.

## Docs

```sh
make docs         # build into docs/site
make docs-serve   # serve with live reload
```

Both run a pinned version of [Zensical](https://zensical.org) through `uvx`, so only [uv](https://docs.astral.sh/uv/) needs to be installed. The build is strict: a broken link fails it.

The published site is versioned: each release `vX.Y.Z` has its docs under `X.Y/`, `latest/` points at the newest release (the default), and `dev/` follows `main`. CI publishes them (`.github/workflows/docs.yml`) with `make docs-deploy`, which runs Zensical's fork of [mike](https://github.com/squidfunk/mike) and commits each version to the `gh-pages` branch. A patch release replaces its `X.Y` docs.
