# Performance

Measured on 2026-11-18 (storage) and 2026-11-19 (timings) against `jsonb`: PostgreSQL 18, built without assertions for the timings, and a release build of postbor, on one development machine. Ratios are `cbor` time divided by `jsonb` time, measured back to back (below 1 means `cbor` is faster). They are a rough guide, not a benchmark suite: run `bench/lookup.sql` on your own data and hardware before deciding.

## Storage

| Data | `cbor` | `jsonb` |
|---|---|---|
| 200,000 small values (`{"id", "name", "tags": [3 items], "active"}`) | 15 MB | 27 MB |
| 2,000 maps of 1,000 keys | 16 MB | 19 MB |

## Small values

200,000 rows of the small values above, no index, best of 5 runs:

| Query | cbor / jsonb |
|---|---|
| `->> 'name'` | 1.46 |
| `@> '{"active": true}'` | 1.65 |
| `@> '{"id": 5}'` | 1.30 |
| `? 'name'` | 1.16 |
| `? 'id'` | 1.04 |
| text output | 1.1 |
| `DISTINCT` | 0.87 |

Reading one key of a small map is slower than in `jsonb`: below 16 entries, `cbor` steps over the earlier entries one by one, where `jsonb` searches a table of offsets.

## Large maps

Reading one key out of 1,000: ratio 0.8. Maps and arrays with 16 entries or more carry an index of their entries, so a lookup does not scan.

## GIN indexes

200,000 small values with a nested map:

- Index sizes are the same as `jsonb`'s: 29 MB for `cbor_ops` and `jsonb_ops`, 24 MB for `cbor_path_ops` and `jsonb_path_ops`. Index builds are 3–5% faster.
- Indexed `@>` queries run within 10–20% of `jsonb`, with one exception in `cbor`'s favour: `@> '{"active": true, "addr": {"zip": 5}}'` with the default class takes 1.9 ms against 10.1 ms, because `cbor_ops` keeps array elements and map values apart (see [Indexes](indexes.md#gin)).

## Path queries

Without an index, on 200,000 small values, path operators are 2 to 4 times slower than `jsonb`'s:

| Query | `cbor` | `jsonb` |
|---|---|---|
| `@@ '$.id < 1000'` | 33 ms | 10 ms |
| `@? '$.addr ? (@.city == "c1")'` | 37 ms | 12 ms |
| `@? '$.** ? (@ == 1)'` | 256 ms | 61 ms |

Most of the gap is the cost of each function call rather than the path evaluation itself.
