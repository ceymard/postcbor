# UTF-8 databases only

postbor supports only UTF-8 databases; `CREATE EXTENSION` refuses any other encoding. CBOR text strings are UTF-8, and pgrx's `&str` arguments reject non-ASCII text in other encodings. Testing a LATIN1 database also showed the jsonb conversion producing mojibake. Supporting other encodings would mean reading every text argument as raw bytes and converting at each boundary (text arguments, `text[]` paths, column names, JSON text, output functions), for databases that are rare in practice.

This replaces the earlier plan (design discussion Q24) of converting in the glue for any database encoding.
