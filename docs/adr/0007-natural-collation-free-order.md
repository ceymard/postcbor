# Sort order is natural and collation-free

The btree order sorts by type rank first, then by value within the type: numbers by numeric value, text by UTF-8 bytes (code-point order). It is not the byte order of the deterministic encoding, which would put `-2` after `-1` and `"b"` before `"aa"`, since shorter strings sort first in that encoding. Text comparison ignores the database collation, unlike jsonb, so comparisons are faster and an OS collation upgrade cannot corrupt indexes.

Equal-valued integers and floats sort integer first, and -0.0 sorts before 0.0. This keeps `cmp = 0` exactly equivalent to byte equality of bodies (see [ADR-0003](0003-typed-numbers.md)).
