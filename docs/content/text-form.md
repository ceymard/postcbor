# Text form

`cbor` values are written and shown in CBOR Extended Diagnostic Notation (EDN), the text form defined for CBOR. EDN is a superset of JSON, so any JSON text is a valid `cbor` literal.

```sql
SELECT '{"name": "ann", 1: h''cafe'', "when": 1(1700000000), "n": 2.5}'::cbor;
-- {1: h'cafe', "n": 2.5, "name": "ann", "when": 1(1700000000)}
```

Inside an SQL string literal, a single quote is doubled: the EDN `h'cafe'` is written `h''cafe''`.

## Writing values

| Kind | EDN | Notes |
|---|---|---|
| Integer | `42`, `-7`, `0x2a`, `0o52`, `0b101010` | Any size: beyond 64 bits it is stored as a bignum (tag 2 or 3) |
| Float | `2.5`, `1e10`, `NaN`, `Infinity`, `-Infinity`, `-0.0` | A number with `.` or an exponent is a float |
| Text string | `"text"` | JSON escapes, including `\u` surrogate pairs |
| Byte string | `h'cafe'`, `b64'AQI'`, `'abc'` | Hex (spaces and comments allowed inside), base64 or base64url (padding optional), or the UTF-8 bytes of the text |
| Embedded CBOR | `<<1, "a">>` | A byte string holding the encodings of the items |
| Array | `[1, 2]` | |
| Map | `{"a": 1, 2: "b", h'00': null}` | Keys of any type |
| Tag | `1(1700000000)`, `32("https://example.com")` | Tag number, then the content in parentheses |
| Simple value | `true`, `false`, `null`, `undefined`, `simple(16)` | |
| Date and time | `dt'2024-01-01T00:00:00Z'`, `DT'…'` | RFC 3339 text read as seconds since 1970; `DT` wraps the number in tag 1 |
| IP address | `ip'192.0.2.1'`, `IP'2001:db8::/32'` | A byte string, or `[prefix length, bytes]` for a prefix; `IP` wraps it in tag 52 (IPv4) or 54 (IPv6) |

Comments are allowed anywhere whitespace is: `# …` to the end of the line, or `/ … /`. Encoding indicators (`_0` to `_3`, `_i`, and `_` for indefinite length, as in `[_ 1, 2]`) are read and ignored, since every value is stored in one form anyway.

```sql
SELECT '[_ 0x10, 18446744073709551616, h''00 ff'', b64''AQI'', <<1, "a">>, simple(99)] # comment'::cbor;
-- [16, 2(h'010000000000000000'), h'00ff', h'0102', h'016161', simple(99)]

SELECT 'dt''2024-01-01T00:00:00Z'''::cbor, 'DT''2024-01-01T00:00:00.5Z'''::cbor, 'IP''2001:db8::/32'''::cbor;
-- 1704067200 | 1(1704067200.5) | 54([32, h'20010db8'])
```

Not accepted, with an error naming the feature: `b32''` and `h32''` strings, string concatenation, the `...` ellipsis, raw strings, hexadecimal floats, other application literals, and trailing commas.

## How values are shown

Output is always written the same way: `", "` between items and `": "` after keys (as `jsonb` does), integers in decimal, floats in the shortest decimal that reads back as the same value (always with a `.` or an exponent, so `1.0` stays recognisably a float), bytes as lowercase `h'…'`, and integers beyond 64 bits as `2(h'…')` or `3(h'…')`.

`cbor_pretty(value)` and `cbor_diag(value, pretty => true)` indent with 4 spaces, one entry per line, like `jsonb_pretty`:

```sql
SELECT cbor_pretty('{"a": [1, 2], "b": {"c": h''ff''}}');
-- {
--     "a": [
--         1,
--         2
--     ],
--     "b": {
--         "c": h'ff'
--     }
-- }
```

## One stored form

Every value is stored in a single canonical form, whatever form it arrived in:

- map keys are sorted by their encoded bytes: integers, byte strings, text strings, arrays, maps, tags, then floats and simple values, and within each type shorter keys first;
- integers, lengths and tag numbers use their shortest encoding;
- floats use the shortest of 16, 32 or 64 bits that holds the value exactly; every NaN becomes one NaN, and `-0.0` stays distinct from `0.0`;
- indefinite-length arrays, maps and strings become definite;
- bignums that fit in 64 bits become plain integers;
- [packed CBOR](binary-form.md#packed-cbor) is unpacked.

So two values are equal exactly when they are the same CBOR value, and the output never depends on how the input was written:

```sql
SELECT '{"b": 1, "a": 2}'::cbor;
-- {"a": 2, "b": 1}
```

Input is rejected when a map has the same key twice (after normalization: `{1: 0, 2(h'01'): 0}` has key `1` twice), when a text string is not valid UTF-8, or when there is anything after the value. Other tags are kept as written; their content is only checked when an operation interprets them, for example a cast to `timestamptz`.

Byte strings are never looked into, including embedded CBOR (tag 24 or `<<…>>`): their bytes are kept as they are.
