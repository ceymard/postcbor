# The cbor → jsonb cast is lossy; a strict function errors

`::jsonb` and `::json` follow the RFC 8949 §6.1 conversion advice:
- byte strings → base64url text
- non-text keys → their EDN text
- NaN and Infinity → null
- undefined → null
- tags → dropped, keeping the content

Bignums (tags 2/3) become JSON numbers rather than RFC 8949's base64url strings: jsonb stores arbitrary-precision numbers exactly, so nothing is lost.

Addendum (2026-11-19): decimal fractions and bigfloats (tags 4/5) also become JSON numbers (`4([-1, 50])` → `50e-1`, which jsonb reads as `5.0`), when `numeric` can hold them; beyond its range they are dropped like other tags. Path queries treat them as numbers and return decimal fractions as arithmetic results, so the cast must show them as numbers for results to match jsonb's.

Casts are mostly used for display and export, where an error would be more annoying than lossy output. `cbor_to_jsonb_strict()` raises an error on anything JSON cannot represent.

Also in the cast family:
- `bytea::cbor` *decodes* the bytes, like `text::cbor` parses them.
- `to_cbor(bytea)` *wraps* them as a CBOR byte string.
