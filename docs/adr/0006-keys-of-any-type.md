# Map keys of any type; operators only take text, int and bytea keys

Maps accept keys of any CBOR type, because real CBOR uses integer keys (COSE, CWT) and byte-string keys. Duplicate keys are rejected on input; jsonb silently keeps the last one, and RFC 8949 calls such maps invalid.

Operators choose the kind of key from the SQL type of the argument:
- `-> int`: an index on an array, an integer key on a map. The container's type decides, so `cwt -> 1` reads key 1.
- `-> text`: a text key.
- `-> bytea`: a byte-string key.

Keys of any type, and typed paths (a CBOR array whose steps keep their exact type), use functions with their own names (`cbor_get`, `cbor_get_path`, `cbor_has`, `cbor_delete_path`, …), not `(cbor, cbor)` operator overloads. This was tested on PG 18. Once `->(cbor, cbor)` exists, PostgreSQL reads an untyped right-hand literal as `cbor`: `doc -> 'a'` fails as invalid EDN, and `doc -> '1'` silently looks up integer key 1. A distinct key type does not save `#>`: with both `text[]` and another type on the right, `doc #> '{a}'` is rejected as "operator is not unique". Function overloads have the same ambiguity, hence the distinct names.
