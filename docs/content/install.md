# Install

postcbor is built from source; there are no pre-built packages yet.

## Requirements

- PostgreSQL 15, 16, 17 or 18, with its server development files (Debian/Ubuntu: `postgresql-server-dev-NN`) and the headers they include (Debian/Ubuntu: `libicu-dev`).
- Rust, at the version pinned in `rust-toolchain.toml`.
- `cargo-pgrx` 0.20.0: `cargo install cargo-pgrx --version 0.20.0 --locked`.
- clang, libclang and llvm from one LLVM release (Debian/Ubuntu: `clang libclang-dev llvm`), plus `build-essential`.

## Build and install

```sh
make PG_CONFIG=/path/to/pg_config
make install PG_CONFIG=/path/to/pg_config                              # as a user who can write PostgreSQL's directories
make install PG_CONFIG=/path/to/pg_config PGRX_INSTALL_FLAGS=--sudo    # or let it use sudo
```

`PG_CONFIG` selects the PostgreSQL installation; it defaults to the `pg_config` on your `PATH`. `make` registers that installation with pgrx in `target/pgrx-home`, so your `~/.pgrx` is left alone. This sequence is tested from scratch on the `postgres:18` Docker image.

Then, in each database:

```sql
CREATE EXTENSION postcbor;
```

## UTF-8 databases only

`CREATE EXTENSION postcbor` fails in a database whose encoding is not UTF-8. CBOR text strings are UTF-8, and supporting other encodings would mean converting every text argument, path, column name and output at the boundary, for databases that are rare in practice.
