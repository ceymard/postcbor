# Casts and JSON

All casts are explicit: write `value::int8`, `value::jsonb` and so on.

## From cbor

| To | Accepts | Error otherwise |
|---|---|---|
| `bool` | `true`, `false` | wrong type |
| `int2`, `int4`, `int8` | integers in range | wrong type, out of range |
| `float4`, `float8` | integers and floats | wrong type |
| `numeric` | integers, floats (including NaN and infinities), bignums (tags 2, 3), decimal fractions (tag 4), bigfloats (tag 5) | wrong type, beyond `numeric`'s range or scale |
| `timestamptz` | tag 0 (RFC 3339 text) or tag 1 (seconds since 1970) | wrong type, invalid content, out of range |
| `date` | tag 100 (days since 1970) | wrong type, invalid content |
| `uuid` | tag 37 holding 16 bytes | wrong type, invalid content |
| `bytea` | anything: gives the canonical CBOR encoding | |
| `json`, `jsonb` | anything, with losses (below) | |

```sql
SELECT '1(1700000000)'::cbor::timestamptz, '4([-2, 12345])'::cbor::numeric, '37(h''a0eebc999c0b4ef8bb6d6bb9bd380a11'')'::cbor::uuid;
-- 2023-11-14 22:13:20+00 | 123.45 | a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11
```

A value of the wrong type raises SQLSTATE `22023` (invalid_parameter_value); a value out of range raises `22003` (numeric_value_out_of_range).

## To json and jsonb

JSON cannot represent everything CBOR can. `::jsonb` and `::json` follow the conversion advice of RFC 8949, section 6.1:

| CBOR | JSON |
|---|---|
| byte strings | base64url text, without padding |
| keys that are not text | their EDN text |
| NaN, infinities | `null` |
| `undefined` | `null` |
| bignums, decimal fractions, bigfloats | numbers, when `numeric` can hold them |
| other tags | their content (the tag is dropped) |
| simple values other than `true`, `false`, `null` | `null` |

```sql
SELECT '{"b": h''ff'', 1: 2, "n": NaN, "t": 1(5), "d": 4([-2, 12345])}'::cbor::jsonb;
-- {"1": 2, "b": "_w", "d": 123.45, "n": null, "t": 5}
```

`cbor_to_jsonb_strict(value)` converts the same way but raises an error instead of losing information:

```sql
SELECT cbor_to_jsonb_strict('{"b": h''ff''}');
-- ERROR:  byte strings have no JSON form
```

## From json and jsonb

`jsonb::cbor` and `json::cbor` keep the structure. Integers become CBOR integers (bignums when they need more than 64 bits); other numbers become the nearest 64-bit float.

```sql
SELECT '{"a": 1, "b": 1.5, "c": 123456789012345678901234567890}'::jsonb::cbor;
-- {"a": 1, "b": 1.5, "c": 2(h'018ee90ff6c373e0ee4e3f0ad2')}
```

`jsonb` keeps `1.0` as written, so it becomes the float `1.0`, not the integer `1`.

## to_cbor

`to_cbor(anyelement)` converts any SQL value, as `to_jsonb` does:

| SQL type | CBOR |
|---|---|
| `bool` | `true`, `false` |
| `int2`, `int4`, `int8` | integer |
| `float4`, `float8` | float |
| `numeric` | integer when whole (a bignum beyond 64 bits), otherwise a decimal fraction (tag 4) keeping the scale; NaN and infinities become floats |
| `text`, `varchar`, `char` | text string |
| `bytea` | byte string |
| `timestamptz` | tag 1 with the seconds since 1970 as a float |
| `timestamp` | the same, reading the value as UTC |
| `date` | tag 100 with the days since 1970 |
| `uuid` | tag 37 with the 16 bytes |
| arrays | arrays, nested for multi-dimensional arrays |
| composite types and records | map with the column names as text keys |
| `json`, `jsonb` | as the cast above |
| `cbor` | the value itself |
| domains | as their base type |
| anything else | its text output, as a text string |

```sql
SELECT to_cbor(row(2.50::numeric, '\xff'::bytea, '2024-01-01'::date, ARRAY[[1, 2], [3, 4]]));
-- {"f1": 4([-2, 250]), "f2": h'ff', "f3": 100(19723), "f4": [[1, 2], [3, 4]]}
```
