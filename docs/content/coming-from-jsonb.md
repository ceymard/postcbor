# Coming from jsonb

`cbor` works like `jsonb`: same operators, `cbor_*` functions for every `jsonb_*` one, same index classes. These are the differences that might surprise you.

## Integers and floats are different values

`jsonb` stores every number as `numeric`, so `1` and `1.0` are equal. In CBOR they are different types, and `cbor` keeps them apart:

```sql
SELECT '1'::cbor = '1.0'::cbor, '{"a": 1}'::cbor @> '{"a": 1.0}';
-- f | f
```

Equality, containment, `DISTINCT`, btree, hash and GIN indexes are all typed. Path queries are the exception: inside a `jsonpath`, numbers compare by value, as SQL/JSON requires (`'1'::cbor @@ '$ == 1.0'` is true). Cast to `numeric` to compare numbers loosely in SQL.

Floats are binary: `0.1` is the nearest 64-bit float, not the exact decimal. For exact decimals, use `numeric`'s conversion: `to_cbor(0.1::numeric)` gives the decimal fraction `4([-1, 1])`.

## Keys can be anything

Map keys can be integers, byte strings, floats, tags, even arrays. The operators accept text, integer and byte-string keys:

```sql
SELECT v -> 'a', v -> 1, v -> '\x01'::bytea
FROM (SELECT '{"a": 1, 1: 2, h''01'': 3}'::cbor AS v) s;
-- 1 | 2 | 3
```

In `jsonb`, an integer on the right only indexes arrays; on a `cbor` map it reads that integer key. Text that looks like a number is still text: `-> '1'` reads the text key `"1"`, not integer key 1.

For other keys, and for paths whose steps keep their type, use [`cbor_get`, `cbor_get_path` and friends](operators.md#keys-of-any-type). A quoted literal passed to an overloaded function is read as text: write `cbor_delete(v, '1'::cbor)` to delete integer key 1.

`cbor_each` and `cbor_object_keys` return keys as `cbor`, not `text`, so their type survives.

## `->>` on non-strings gives EDN

`->>` gives a text string's text, as in `jsonb`. Anything else comes back as its EDN text, so bytes show as `h'…'` and tags as `1(…)`:

```sql
SELECT '{"b": h''ff'', "t": 1(1700000000)}'::cbor ->> 'b';
-- h'ff'
```

## Key order

As in `jsonb`, keys are sorted rather than kept in insertion order. Text keys come in the same order as in `jsonb` (shorter first, then bytewise). Keys of other types sort by type: integers and byte strings before text, then arrays, maps, tags, and finally floats and simple values.

```sql
SELECT '{"b": 1, "a": 2, 10: 3}'::cbor;
-- {10: 3, "a": 2, "b": 1}
```

## Text sorts by code point

`jsonb` sorts strings with the database collation; `cbor` sorts them by their UTF-8 bytes, which is code-point order. `"B" < "a"` in `cbor`, whatever the collation.

## Converting to jsonb loses information

`::jsonb` turns bytes into base64url text, non-text keys into their EDN text, `NaN`, infinities and `undefined` into `null`, and drops tags. Use `cbor_to_jsonb_strict` to get an error instead. See [Casts](casts.md#to-json-and-jsonb).

## More values exist

`undefined` and simple values have their own types and are not `null`. `cbor_strip_nulls` removes `null` only, unless you pass `strip_undefined => true`. `cbor_typeof` returns `integer` or `float` rather than `number`, and `map` rather than `object`.
