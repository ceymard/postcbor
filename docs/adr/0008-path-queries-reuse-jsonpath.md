# Path queries reuse the core jsonpath type (phase 3)

The SQL/JSON features (`JSON_TABLE`, `JSON_QUERY`, `JSON_VALUE`, `JSON_EXISTS`, `IS JSON`) are part of PostgreSQL's grammar and only work on json/jsonb. No extension can provide them for `cbor`. For `@?`, `@@` and `cbor_path_*`, we take the core `jsonpath` type as the query argument and write our own code that runs it over `cbor`. The core parser and the compiled path format are reused as they are. Only text keys can be expressed this way. A `cborpath` language of our own (integer and byte keys, tag tests) is deferred until someone needs it.

## Decisions taken with the phase 3 design (2026-11-19)

- The compiled `jsonpath` is decoded from its binary form, with its version checked, rather than printed and re-parsed.
- Numbers compare by value inside path expressions, as SQL/JSON requires; containment and equality on `cbor` stay typed.
- Results must match `jsonb_path_*` on the value converted to jsonb; CBOR types without a JSON counterpart get the behaviour in design §8.
- Scope: `@?`, `@@`, all `cbor_path_*` functions with `vars` and `silent`, the `_tz` variants, `like_regex`, datetime methods, and GIN support for simple paths.

