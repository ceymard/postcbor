# postcbor

A PostgreSQL extension that adds a `cbor` type, the CBOR counterpart of `jsonb`. Values are stored in one canonical binary form, with `jsonb`'s operators, functions and indexes, extended to everything CBOR can hold: byte strings, tags, map keys of any type, exact integers of any size, and floats kept apart from integers. Packed CBOR, including cbor-x records, is unpacked on input.

```sql
CREATE EXTENSION postcbor;
SELECT '{"name": "ann", 1: h''cafe'', "when": 1(1700000000)}'::cbor -> 1;
-- h'cafe'
```

**Built with AI.** postcbor was written entirely by Claude (Anthropic's AI model) through Claude Code: code, tests, fuzz targets, specification and docs. A human set the goals, chose the architecture and reviewed the design and the tests, but not the code line by line. See [How this was built](docs/content/how-built.md) for what stands behind it.

## Install

PostgreSQL 15–18 (UTF-8 databases only), Rust, `cargo-pgrx` 0.20.0 and clang; see [Install](docs/content/install.md) for the details.

```sh
make PG_CONFIG=/path/to/pg_config
make install PG_CONFIG=/path/to/pg_config
```

## Documentation

The documentation lives in [`docs/content`](docs/content/index.md); `make docs` builds it as a site into `docs/site`. The specification is [`docs/content/design.md`](docs/content/design.md).
