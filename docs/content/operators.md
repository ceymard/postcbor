# Operators

The operators are `jsonb`'s, with the same symbols and meaning, plus `?#`, `?#|` and `?#&` for keys of any type. Missing keys and out-of-range indexes give NULL, as in `jsonb`.

| Operator | Right side | Result | Meaning |
|---|---|---|---|
| `->` | `int` | `cbor` | Array element (negative counts from the end), or integer key of a map |
| `->` | `text`, `bytea` | `cbor` | Text key, byte-string key |
| `->>` | same as `->` | `text` | Same, as text (see below) |
| `#>`, `#>>` | `text[]` | `cbor`, `text` | Value at a text path |
| `@>`, `<@` | `cbor` | `bool` | Contains, is contained in |
| `?` | `text` | `bool` | Text key exists, or the top-level array has that text string |
| `?|`, `?&` | `text[]` | `bool` | Any of, all of the text keys exist |
| `?#` | `cbor` | `bool` | Key of any type exists, or the top-level array has that element |
| `?#|`, `?#&` | `cbor` (an array of keys) | `bool` | Any of, all of the keys exist |
| `||` | `cbor` | `cbor` | Merge maps, concatenate arrays |
| `-` | `text`, `text[]`, `int`, `bytea` | `cbor` | Delete a key, several text keys, or an array element |
| `#-` | `text[]` | `cbor` | Delete at a text path |
| `@?`, `@@` | `jsonpath` | `bool` | [Path query](path-queries.md) matches, path predicate holds |
| `=`, `<>`, `<`, `<=`, `>`, `>=` | `cbor` | `bool` | Equality and [ordering](#equality-and-ordering) |

## Reading values

```sql
SELECT v -> 'a', v -> 1, v -> '\x01'::bytea, v #> '{a,b,1}', v ->> 1
FROM (SELECT '{"a": {"b": [10, 20]}, 1: "one", h''01'': "bytes"}'::cbor AS v) s;
-- {"b": [10, 20]} | "one" | "bytes" | 20 | one
```

An integer on the right reads an array element, or the integer key of a map, so `cwt -> 1` reads key 1 of a CWT claims map.

`->>` and `#>>` give a text string as its text, `null` as SQL NULL, and anything else as its EDN text: `'{"b": h''ff''}'::cbor ->> 'b'` is `h'ff'`.

A path for `#>` (a text path) follows `jsonb`'s rules: each step is a text key, or an array index when it reads as an integer and the value at that point is an array.

## Keys of any type

The operators only take text, integer and byte-string keys. For other keys (floats, tags, `true`, arrays…) and for paths whose steps keep their type (a typed path, written as a CBOR array), use the functions:

| Function | Like |
|---|---|
| `cbor_get(value, key cbor)`, `cbor_get_text(value, key cbor)` | `->`, `->>` |
| `cbor_get_path(value, path cbor)`, `cbor_get_path_text(value, path cbor)` | `#>`, `#>>` |
| `cbor_has(value, key cbor)`, `cbor_has_any(value, keys cbor)`, `cbor_has_all(value, keys cbor)` | `?#`, `?#|`, `?#&` |
| `cbor_delete(value, key cbor)`, `cbor_delete_path(value, path cbor)` | `-`, `#-` |
| `cbor_set_path`, `cbor_insert_path` | `cbor_set`, `cbor_insert` (see [Functions](functions.md#changing-values)) |

```sql
SELECT cbor_get(v, 'h''01'''), cbor_get_path(v, '["a", "b", 0]'), v ?# '1', v ? '1'
FROM (SELECT '{"a": {"b": [10, 20]}, 1: "one", h''01'': "bytes"}'::cbor AS v) s;
-- "bytes" | 10 | t | f
```

These are separate names, not overloads of `->` and `#>`, because of how PostgreSQL types a quoted literal. With an operator taking `cbor` on the right, `v -> 'a'` would read `'a'` as CBOR text (invalid EDN), and `v -> '1'` would quietly look up integer key 1 instead of text key `"1"`.

The same applies to function overloads: `cbor_delete(v, '1')` reads `'1'` as text and deletes the text key `"1"`. Write `cbor_delete(v, '1'::cbor)` to delete integer key 1.

## Containment

`@>` follows `jsonb`: a map contains another when it has each of its keys with a containing value; an array contains another when each element of the other is contained in some element, in any order; a scalar contains only itself. A tag contains a value with the same tag number whose content it contains. Decimal fractions and bigfloats (tags 4 and 5) are numbers, compared whole.

```sql
SELECT '{"a": 1, "tags": ["x", "y"]}'::cbor @> '{"tags": ["x"]}';
-- t
```

Containment is typed: `'{"a": 1}'::cbor @> '{"a": 1.0}'` is false, since `1` and `1.0` are different values.

## Merging and deleting

```sql
SELECT '{"a": 1}'::cbor || '{"b": 2}', '[1]'::cbor || '2', '{"a": 1, 1: 2}'::cbor - 1, '{"a": {"b": 1, "c": 2}}'::cbor #- '{a,b}';
-- {"a": 1, "b": 2} | [1, 2] | {"a": 1} | {"a": {"c": 2}}
```

`||` follows `jsonb`: two maps merge (the right side wins on equal keys), and anything else is concatenated as arrays, a non-array side counting as a one-element array. On a map, `- 1` deletes integer key 1; on an array it deletes element 1.

## Subscripting

`value[…]` reads like `->` and can be assigned in `UPDATE`. The subscript's type picks the meaning: integers index arrays or address integer keys, `bytea` addresses byte-string keys, `cbor` addresses keys of any type, and anything else, including a quoted literal, is a text key.

```sql
CREATE TABLE t (c cbor);
INSERT INTO t VALUES ('{"a": [1]}');
UPDATE t SET c['a'][3] = '"x"';
UPDATE t SET c[1] = 'true';
SELECT c, c['a'][0] FROM t;
-- {1: true, "a": [1, null, null, "x"]} | 1
```

Assignment follows `jsonb`: missing levels are created (a map for a key, an array for an integer), and assigning past the end of an array pads it with nulls. A NULL column is treated as empty. Assigning below a scalar is an error, and so is a NULL subscript.

## Equality and ordering

Two values are equal when they are the same CBOR value: same types, same contents. `1` and `1.0` are not equal, and `-0.0` is not equal to `0.0`. Cast to `numeric` to compare numbers across types.

The sort order (used by `ORDER BY`, btree indexes and `DISTINCT`) first compares type, in this order:

1. numbers: integers, floats and bignums, compared by exact value
2. byte strings
3. text strings
4. arrays
5. maps
6. other tags
7. simple values other than those below
8. `false`
9. `true`
10. `null`
11. `undefined`

Within a type:

- Numbers compare by their exact mathematical value, so `1 < 1.5 < 2`. NaN sorts after every other number. When an integer and a float are equal, the integer sorts first, and `-0.0` sorts before `0.0`.
- Byte strings and text strings compare byte by byte, a prefix first. Text ignores the database collation and sorts by code point (its UTF-8 bytes), so an operating system collation upgrade cannot corrupt indexes.
- Arrays compare element by element, a prefix first.
- Maps with fewer entries sort first; then key 1, value 1, key 2, … in stored key order.
- Tags compare by number, then by content. Simple values compare by number.
