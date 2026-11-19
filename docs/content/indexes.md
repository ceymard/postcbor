# Indexes

## btree and hash

`cbor` has default btree and hash operator classes, so `CREATE INDEX ON t (c)`, `UNIQUE`, `ORDER BY`, `DISTINCT`, `GROUP BY` and hash joins work as for any type. The btree order is described in [Operators](operators.md#equality-and-ordering).

To index one field, index an expression, as with `jsonb`:

```sql
CREATE INDEX ON events ((payload -> 'device'));
```

## GIN

Two GIN operator classes, as for `jsonb`:

```sql
CREATE INDEX ON events USING gin (payload);                  -- cbor_ops, the default
CREATE INDEX ON events USING gin (payload cbor_path_ops);    -- @> only; smaller and more selective
```

| Class | Operators | Indexes |
|---|---|---|
| `cbor_ops` (default) | `@>`, `?`, `?|`, `?&`, `?#`, `?#|`, `?#&`, `@?`, `@@` | Each map key and each scalar, separately |
| `cbor_path_ops` | `@>` | Each scalar together with the keys leading to it |

`cbor_ops` can answer key existence because keys are indexed on their own; `cbor_path_ops` cannot, but its entries are more specific, so containment queries find fewer candidate rows. Both classes are lossy: every candidate row is checked again, so results are always exact.

Keys and values keep their CBOR type in the index: `1`, `"1"` and `1.0` are different entries, just as `@>` treats them as different values. For path queries the index also holds each number in a single form, so `@@ '$.n == 1'` finds rows where `n` is `1`, `1.0` or `4([0, 1])`.

`cbor_ops` keeps array elements and map values apart, where `jsonb_ops` does not. A query such as `@> '{"addr": {"zip": 5}}'` therefore finds fewer false candidates than on `jsonb` when other rows have `5` inside an array.

### Path queries

With `cbor_ops`, `@?` and `@@` use the index for conditions of the form `accessors == literal` (accessors being `.key`, `.*`, `.**`, `[…]`, `[*]` and filters on them), combined with `&&`, `||` and `!`, as `jsonb_ops` does. A path without such a condition scans the whole index. `cbor_path_ops` does not support path queries.

```sql
EXPLAIN (COSTS OFF) SELECT * FROM events WHERE payload @? '$.id ? (@ == 42)';
-- Bitmap Heap Scan on events
--   Recheck Cond: (payload @? '$."id"?(@ == 42)'::jsonpath)
--   ->  Bitmap Index Scan on events_payload_idx
--         Index Cond: (payload @? '$."id"?(@ == 42)'::jsonpath)
```

### Queries that scan the whole index

- `@>` with a value that has no scalars: `{}`, `[]`, `{"a": {}}`.
- `?#` with a key that is an array or map, and `?#|` when any of its keys is one.
- `@?` and `@@` without an `==` condition.

These still give the right result; they just do not benefit from the index.
