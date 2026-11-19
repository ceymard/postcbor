# Functions

Every `jsonb_*` function has a `cbor_*` counterpart that takes and returns `cbor`. Where this page says "as `jsonb_x`", the function behaves like its `jsonb` counterpart, described in PostgreSQL's [JSON functions documentation](https://www.postgresql.org/docs/current/functions-json.html), and only the differences are listed. Functions that exist only for CBOR are described in full [below](#cbor-specific-functions).

## Building values

| Function | Differences from jsonb |
|---|---|
| `to_cbor(anyelement)` | See [the type mapping](casts.md#to_cbor) |
| `cbor_build_array(VARIADIC "any")` | As `jsonb_build_array` |
| `cbor_build_object(VARIADIC "any")` | Keys keep their type: an `int` argument becomes an integer key, a `bytea` a byte-string key |
| `cbor_object(text[])`, `cbor_object(keys text[], values text[])` | As `jsonb_object`: text keys and text values |
| `cbor_agg(anyelement)`, `cbor_agg_strict` | As `jsonb_agg`, `jsonb_agg_strict` |
| `cbor_object_agg(key, value)`, `_strict`, `_unique`, `_unique_strict` | As `jsonb_object_agg` and variants; keys of any type |

```sql
SELECT cbor_build_object(1, 'one', 'k', '\x00'::bytea, 'n', NULL);
-- {1: "one", "k": h'00', "n": null}

SELECT cbor_object_agg(k, v) FROM (VALUES (1, 'a'), (2, 'b')) x(k, v);
-- {1: "a", 2: "b"}
```

## Taking values apart

| Function | Differences from jsonb |
|---|---|
| `cbor_each(cbor)` | Returns `(key cbor, value cbor)`: keys keep their type |
| `cbor_each_text(cbor)` | Returns `(key text, value text)`, both converted with the `->>` rules |
| `cbor_object_keys(cbor)` | Returns `setof cbor`, keeping each key's type |
| `cbor_array_elements(cbor)` | As `jsonb_array_elements` |
| `cbor_array_elements_text(cbor)` | As `jsonb_array_elements_text`, with the `->>` rules |
| `cbor_array_length(cbor)` | As `jsonb_array_length` |
| `cbor_extract_path(cbor, VARIADIC text[])`, `_text` | As `jsonb_extract_path`, `_text` |
| `cbor_typeof(cbor)` | Returns one of `integer`, `float`, `bytes`, `text`, `array`, `map`, `tag`, `simple`, `boolean`, `null`, `undefined` |

```sql
SELECT * FROM cbor_each('{1: "a", "b": h''ff''}');
-- 1   | "a"
-- "b" | h'ff'
```

The `->>` rules: a text string gives its text, `null` gives SQL NULL, anything else gives its EDN text.

## Changing values

| Function | Differences from jsonb |
|---|---|
| `cbor_set(target, path text[], new_value, create_if_missing DEFAULT true)` | As `jsonb_set` |
| `cbor_set_lax(…, null_value_treatment DEFAULT 'use_json_null')` | As `jsonb_set_lax` |
| `cbor_insert(target, path text[], new_value, insert_after DEFAULT false)` | As `jsonb_insert` |
| `cbor_set_path(target, path cbor, new_value, create_if_missing DEFAULT true)` | `cbor_set` with a typed path |
| `cbor_insert_path(target, path cbor, new_value, insert_after DEFAULT false)` | `cbor_insert` with a typed path |
| `cbor_delete_text_path(cbor, text[])` | As `jsonb_delete_path` (the `#-` operator) |
| `cbor_strip_nulls(target, strip_in_arrays DEFAULT false, strip_undefined DEFAULT false)` | As `jsonb_strip_nulls`; `undefined` is removed too when `strip_undefined` is true |
| `cbor_pretty(cbor)` | As `jsonb_pretty`, in EDN |

A typed path is a CBOR array whose steps keep their type: `'["a", 5, h''01'']'` is text key `"a"`, then integer 5 (an array index, or an integer key of a map), then byte-string key `h'01'`.

```sql
SELECT cbor_set_path('{"a": {}}', '["a", 5]', '"x"');
-- {"a": {5: "x"}}

SELECT cbor_strip_nulls('{"a": null, "b": undefined, "c": [null]}', strip_in_arrays => true, strip_undefined => true);
-- {"c": []}
```

## Records

| Function | Differences from jsonb |
|---|---|
| `cbor_populate_record(base anyelement, cbor)`, `cbor_populate_recordset` | As `jsonb_populate_record`, `_recordset` (see below) |
| `cbor_populate_record_valid(base anyelement, cbor)` | As `jsonb_populate_record_valid`. PostgreSQL 17 and later |
| `cbor_to_record(cbor)`, `cbor_to_recordset(cbor)` | As `jsonb_to_record`, `_recordset` |

Text keys are matched to column names; keys of other types are ignored. Values convert as in `jsonb`, plus: byte strings go to `bytea` columns, and tags 0 and 1 go to `timestamptz`, tag 37 to `uuid` and tag 100 to `date` columns. Other values go through their EDN text into the column type's input function.

```sql
CREATE TYPE person AS (id int, name text, seen timestamptz);
SELECT * FROM cbor_populate_record(NULL::person, '{"id": 7, "name": "ann", "seen": 1(1700000000), 5: "ignored"}');
-- 7 | ann | 2023-11-14 22:13:20+00
```

## Path queries

`cbor_path_exists`, `cbor_path_match`, `cbor_path_query`, `cbor_path_query_array`, `cbor_path_query_first` and their `_tz` variants, as the `jsonb_path_*` functions. See [Path queries](path-queries.md).

## CBOR-specific functions

### Keys of any type

`cbor_get`, `cbor_get_text`, `cbor_get_path`, `cbor_get_path_text`, `cbor_has`, `cbor_has_any`, `cbor_has_all`, `cbor_delete` and `cbor_delete_path` are the operators with keys of any type; see [Operators](operators.md#keys-of-any-type).

### Tags and simple values

`cbor_tag(cbor) → bigint`
:   The tag number, or NULL if the value is not tagged. An error if the number is above the largest `bigint`.

`cbor_untag(cbor) → cbor`
:   The tag's content, or the value itself if it is not tagged.

`cbor_tagged(tag bigint, value cbor) → cbor`
:   The value wrapped in a tag, then normalized, so `cbor_tagged(2, 'h''01''')` gives `1` (a bignum that fits in 64 bits).

`cbor_simple(n int) → cbor`
:   The simple value `n`, from 0–23 or 32–255. 20 to 23 give `false`, `true`, `null` and `undefined`; 24–31 are not valid simple values.

`cbor_undefined() → cbor`
:   `undefined`.

```sql
SELECT cbor_tag('1(5)'), cbor_untag('1(5)'), cbor_tagged(32, '"https://example.com"'), cbor_simple(16);
-- 1 | 5 | 32("https://example.com") | simple(16)
```

### Bytes and text

`cbor_from_bytea(bytea) → cbor`
:   The same as `bytea::cbor`: decodes and normalizes CBOR bytes, unpacking packed CBOR.

`cbor_pack(value cbor, format text DEFAULT 'cbor-x') → bytea`
:   The value as packed CBOR, `'cbor-x'` or `'ietf'`. See [Packed CBOR](binary-form.md#packed-cbor).

`cbor_diag(value cbor, pretty bool DEFAULT false) → text`
:   The EDN text, indented like `cbor_pretty` when `pretty` is true.

`cbor_byte_length(cbor) → bigint`
:   The length of the canonical encoding in bytes, as `cbor::bytea` would give it.

`cbor_to_jsonb_strict(cbor) → jsonb`
:   Like `::jsonb`, but an error on anything JSON cannot represent exactly. See [Casts](casts.md#to-json-and-jsonb).
