# postbor

postbor adds a `cbor` type to PostgreSQL: CBOR values stored in a canonical, indexed binary form, with the operators and functions you know from `jsonb`, extended to everything CBOR can hold (byte strings, tags, integer and binary map keys, undefined, simple values, exact integers and floats).

## Install

postbor requires a UTF-8 database.


Requirements:
- PostgreSQL 15–18 with its server development files (Debian/Ubuntu: `postgresql-server-dev-NN`) and the headers they include (Debian/Ubuntu: `libicu-dev`).
- Rust (the version pinned in `rust-toolchain.toml`) and `cargo-pgrx` 0.20.0: `cargo install cargo-pgrx --version 0.20.0 --locked`.
- clang, libclang and llvm from one LLVM release (Debian/Ubuntu: `clang libclang-dev llvm`), plus `build-essential`.

```sh
make PG_CONFIG=/path/to/pg_config
make install PG_CONFIG=/path/to/pg_config                              # as a user who can write PostgreSQL's directories
make install PG_CONFIG=/path/to/pg_config PGRX_INSTALL_FLAGS=--sudo    # or let it use sudo
```

`make` registers the PostgreSQL installation with pgrx in `target/pgrx-home` (your `~/.pgrx` is left alone). This sequence is tested from scratch on the `postgres:18` Docker image.

```sql
CREATE EXTENSION postbor;
```

## Text form

`cbor` values are written and shown in CBOR diagnostic notation (EDN), a superset of JSON:

```sql
SELECT '{"name": "ann", 1: h''cafe'', "when": 1(1700000000), "n": 2.5}'::cbor;
-- {1: h'cafe', "n": 2.5, "name": "ann", "when": 1(1700000000)}
```

Input also accepts comments (`# …`, `/ … /`), embedded CBOR (`<<1, 2>>`), base64 bytes (`b64'…'`), dates (`dt'2024-01-01T00:00:00Z'`, `DT'…'` for tag 1) and IP addresses (`ip'192.0.2.1'`, `IP'…'` for tags 52/54).

Values are stored in one canonical form: map keys sorted, shortest integers and floats, no indefinite lengths. Two values are equal exactly when they are the same CBOR value; an integer is never equal to a float (`1` ≠ `1.0`).

## Binary form

`bytea::cbor` decodes CBOR bytes; `cbor::bytea` and the binary protocol give the canonical encoding. Packed CBOR (IETF Packed CBOR, cbor-x records and cbor-x packing) is unpacked on input; `cbor_pack(value, 'cbor-x' | 'ietf')` produces a packed `bytea`.

## Operators

| Operator | Example | Result |
|---|---|---|
| `->` `->>` | `doc -> 'a'`, `doc -> 0`, `cwt -> 1`, `doc -> '\x01'::bytea` | Key or index; `->>` gives text |
| `#>` `#>>` | `doc #> '{a,0,b}'` | Path of text keys and indexes |
| `@>` `<@` | `doc @> '{"tags": ["x"]}'` | Containment |
| `?` `?\|` `?&` | `doc ? 'a'` | Key (or string element) exists |
| `\|\|` | `a \|\| b` | Merge maps, concatenate arrays |
| `-` `#-` | `doc - 'a'`, `doc - 0`, `doc #- '{a,b}'` | Delete |
| `=` `<` … | | Equality, ordering, btree and hash indexes |
| `[…]` | `doc['a'][0]`, `SET doc['a'] = '1'` | Subscripting |

Integer keys address arrays by position and maps by integer key, so `cwt -> 1` reads key 1 of a CWT claims map. For keys of other types and typed paths use the functions: `cbor_get(doc, key)`, `cbor_get_path(doc, '[1, "a", h''ff'']')`, `cbor_has`, `cbor_delete`, `cbor_set_path`, ….

## Functions

Every `jsonb_*` function has a `cbor_*` counterpart: `to_cbor`, `cbor_build_array`, `cbor_build_object`, `cbor_object`, `cbor_agg`, `cbor_object_agg` (and `_strict`/`_unique` variants), `cbor_each`, `cbor_each_text`, `cbor_object_keys`, `cbor_array_elements`, `cbor_array_length`, `cbor_extract_path`, `cbor_populate_record(set)`, `cbor_to_record(set)`, `cbor_set`, `cbor_set_lax`, `cbor_insert`, `cbor_strip_nulls`, `cbor_pretty`, `cbor_typeof`.

CBOR-specific: `cbor_tag`, `cbor_untag`, `cbor_tagged`, `cbor_simple`, `cbor_undefined`, `cbor_diag`, `cbor_byte_length`, `cbor_pack`, `cbor_from_bytea`, `cbor_to_jsonb_strict`.

## Casts

To `bool`, `int2/4/8`, `float4/8`, `numeric` (including bignums and decimal fractions), `timestamptz` (tags 0/1), `date` (tag 100), `uuid` (tag 37), `bytea`, `json`, `jsonb`; from `bytea`, `json`, `jsonb`. `to_cbor` maps `timestamptz` to tag 1, `date` to tag 100, `uuid` to tag 37, `numeric` fractions to tag 4, records to maps and arrays to arrays.

`::jsonb` is lossy where JSON cannot represent a value (bytes become base64url, non-text keys become their EDN text, NaN and undefined become null, tags are dropped); `cbor_to_jsonb_strict` raises an error instead.

## Settings

| Setting | Default | |
|---|---|---|
| `postbor.max_depth` | 512 | Maximum nesting depth of input |
| `postbor.max_expansion_ratio` | 16 | Maximum unpacked size relative to packed input |
| `postbor.max_expanded_size` | 256MB | Maximum unpacked size |

All three can only be changed by superusers.

## Not yet available

GIN indexes and path queries (`@?`, `@@`, `cbor_path_*`) are planned; see `TODO.md`.
