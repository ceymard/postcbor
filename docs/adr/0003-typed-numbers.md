# Integers and floats are different values

`1` (integer) and `1.0` (float) are not equal, unlike jsonb where all numbers are `numeric`. CBOR keeps this distinction on purpose, and equality stays a byte comparison of the deterministic encoding. Float width is not kept: floats are stored in the shortest width that represents the value exactly, so f16 `1.0` and f64 `1.0` are the same value. There is one canonical NaN, and -0.0 is distinct from 0.0. Bignums (tags 2/3) that fit in 64 bits are stored as plain integers, so `2(h'01') = 1`. Decimal fractions and bigfloats (tags 4/5) are kept exactly as written.

Users who want numbers compared loosely across types cast to `numeric`.
