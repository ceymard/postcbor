# Path queries

Path queries use PostgreSQL's own `jsonpath` language and type: the same paths you write for `jsonb` work on `cbor`.

| `cbor` | `jsonb` counterpart |
|---|---|
| `value @? path` | `@?`: does the path return any item? |
| `value @@ path` | `@@`: the result of a path predicate |
| `cbor_path_exists(target, path, vars DEFAULT '{}', silent DEFAULT false)` | `jsonb_path_exists` |
| `cbor_path_match(…)` | `jsonb_path_match` |
| `cbor_path_query(…) → setof cbor` | `jsonb_path_query` |
| `cbor_path_query_array(…)` | `jsonb_path_query_array` |
| `cbor_path_query_first(…)` | `jsonb_path_query_first` |
| `cbor_path_exists_tz`, `cbor_path_match_tz`, `cbor_path_query_tz`, `cbor_path_query_array_tz`, `cbor_path_query_first_tz` | the `_tz` variants, which allow datetime conversions that depend on the session time zone |

`vars` is a `cbor` map whose text keys are the path's `$variables`.

```sql
SELECT v @? '$.items[*] ? (@.price > 5)', cbor_path_query_array(v, '$.items[*] ? (@.price > 5).id')
FROM (SELECT '{"items": [{"id": 1, "price": 2.5}, {"id": 2, "price": 10}]}'::cbor AS v) s;
-- t | [2]

SELECT cbor_path_exists('{"items": [{"id": 1}, {"id": 2}]}', '$.items[*] ? (@.id == $want)', '{"want": 2}');
-- t
```

For any value that converts to `jsonb` without loss, the results are those of the `jsonb_path_*` function on `value::jsonb`. A test suite runs PostgreSQL's own jsonpath tests on both types and compares the results; the only differences are the `.keyvalue()` ids described below.

## CBOR types in paths

jsonpath was designed for JSON. CBOR's extra types behave as follows:

- **Numbers compare by value** inside paths, as in SQL/JSON: `'1'::cbor @@ '$ == 1.0'` is true, although `'1'::cbor = '1.0'` is false. Bignums, decimal fractions (tag 4) and bigfloats (tag 5) are numbers.
- **Arithmetic is decimal**, as for `jsonb`, and its results have the same scale. A whole result is an integer; any other result is a decimal fraction (tag 4), so `$.p * 2` with `p = 2.5` gives `4([-1, 50])` (5.0). Items taken unchanged from the value keep their form: a float stays a float.
- **Byte strings** have `.type()` `"bytes"`. Two byte strings compare by their bytes; comparing one with another type is unknown, as for mismatched types in jsonpath.
- **Tags 0 and 1** (date and time) are datetimes, with `.type()` `"timestamp with time zone"`, when their content is a valid date and time. Tags 2–5 are numbers when their content is valid. **Other tags are transparent**: the path sees their content, and an item returned from the value keeps its tag.
- **Map keys that are not text**: `.key` only matches text keys. `.*` and `.keyvalue()` visit every entry, and `.keyvalue()` gives each key as its CBOR value.
- **`undefined` and simple values** have their own types (`.type()` is `"undefined"` or `"simple"`), and are not equal to `null`.
- **NaN and infinities** are numbers, ordered as `numeric` orders them.

```sql
SELECT cbor_path_query('{1: "a", "b": h''ff'', "t": 1(1700000000)}', '$.*.type()');
-- "string"
-- "bytes"
-- "timestamp with time zone"
```

`.keyvalue()` returns an `id` for each object, unique as in `jsonb`. The ids equal `jsonb`'s for maps at the top of the value; for nested maps they differ, because `jsonb` derives them from its own storage layout.

## PostgreSQL versions

The jsonpath methods added in PostgreSQL 17 (`.bigint()`, `.boolean()`, `.date()`, `.decimal()`, `.integer()`, `.number()`, `.string()`, `.time()`, `.time_tz()`, `.timestamp()`, `.timestamp_tz()`) are only available on PostgreSQL 17 and later, since the `jsonpath` type of earlier versions cannot express them.

## Indexes

`@?` and `@@` can use a GIN index with the default `cbor_ops` class, for conditions of the form `accessors == literal`, as `jsonb_ops` does. See [Indexes](indexes.md).
