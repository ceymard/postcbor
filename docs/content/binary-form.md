# Binary form

## CBOR bytes

`bytea::cbor` (or `cbor_from_bytea`) decodes CBOR bytes; `cbor::bytea` gives the canonical encoding. The binary protocol (`COPY … BINARY`, binary parameters and results) uses the same canonical encoding.

```sql
SELECT '\xa2616101616202'::bytea::cbor;
-- {"a": 1, "b": 2}

SELECT '{"a": 1}'::cbor::bytea;
-- \xa1616101
```

Bytes go through the same checks and the same [normalization](text-form.md#one-stored-form) as text input. Input must be a single CBOR item: a CBOR sequence (several items back to back) is refused.

## Packed CBOR

Packed CBOR shares repeated items (strings, map shapes) through tables and references, so the bytes are smaller. postcbor unpacks it on every input path: what is stored, compared and indexed is always the plain value.

Recognized on input:

- **cbor-x records**, cbor-x's default for arrays of similar objects (tags 57342, 57343, 57344 and above).
- **cbor-x packing** (cbor-x's `pack` option, tag 51).
- **IETF Packed CBOR** (draft-ietf-cbor-packed-19): table setup tags 113 and 1113, shared item and argument references, and the join and record functions (tags 105, 106, 114).

Refused:

- cbor-x shared or sequential structures, whose definitions live outside the value (in the encoder or a previous message): a reference to a structure not defined earlier in the same value is an error.
- cbor-x `bundleStrings`, which writes strings after the value, so the input is not a single item.
- IETF tables supplied by the application rather than in the value.

Outside a table setup tag, the reference tags and simple values 0–15 are ordinary values and are kept as written.

`cbor_pack(value, format)` gives packed bytes, `'cbor-x'` (the default) or `'ietf'`. The same value always packs to the same bytes.

```sql
SELECT encode(cbor_pack('[{"name": "ann", "age": 30}, {"name": "bob", "age": 41}]', 'cbor-x'), 'hex');
-- 82d9dfff8419e0008263616765646e616d65181e63616e6ed9e00082182963626f62

SELECT cbor_pack('[{"name": "ann", "age": 30}, {"name": "bob", "age": 41}]', 'cbor-x')::cbor;
-- [{"age": 30, "name": "ann"}, {"age": 41, "name": "bob"}]
```

`'cbor-x'` turns maps with a repeated set of text keys into records. `'ietf'` puts text and byte strings of 3 bytes or more that appear at least twice into a tag 113 table. A value holding plain simple values 0–15 or tags 6 or 128–143 is returned unpacked in `'ietf'` format, since inside tag 113 they would read as references.

Compatibility with cbor-x is tested against cbor-x 1.6.7: its records and `pack` output read back as the plain value, and `cbor_pack(…, 'cbor-x')` decodes in cbor-x to the original objects.

## Limits

Packed input can unpack to far more than its own size. Input is refused when the unpacked value is more than `postcbor.max_expansion_ratio` times the input (default 16) or larger than `postcbor.max_expanded_size` (default 256MB); see [Settings and limits](settings.md).
