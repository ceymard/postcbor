# postbor design

This is the reference specification. Terms are defined in `CONTEXT.md`, and the reasons behind decisions are in `docs/adr/`. Implementation order and progress are tracked in `TODO.md`.

## 1. Goals and scope

- A `cbor` type that does for CBOR what `jsonb` does for JSON: the same operators, the same functions (named `cbor_*` instead of `jsonb_*`), and the same index support, extended to the full CBOR data model.
- Extension name `postbor`, type name `cbor`. Operator symbols are the same as jsonb's, plus `?#`, `?#|` and `?#&` for keys of any type.
- PostgreSQL 15–18.
- Install with `make && make install` (a wrapper around `cargo pgrx install`). Pre-built packages are out of scope for now.

Phases:
1. I/O, casts, operators, btree and hash operator classes, subscripting, every non-path function.
2. GIN operator classes (`cbor_ops`, `cbor_path_ops`).
3. Path queries over the core `jsonpath` type ([ADR-0008](../adr/0008-path-queries-reuse-jsonpath.md)).

## 2. Threat model and limits

Input is untrusted. Bytes arrive from web applications over the internet through `::cbor`, `cbor_recv` (binary protocol) and the `bytea` casts. Every input path goes through the same validation and normalization.

The stored datum is trusted after input, as with jsonb. Debug builds (`debug_assertions`) re-validate every datum they read, to catch format bugs.

| Setting | Default | Context | Applies to |
|---|---|---|---|
| `postbor.max_depth` | 512 | `SUSET` | Nesting depth on every input path and in builders. Each array, map and tag counts as one level. |
| `postbor.max_expansion_ratio` | 16 | `SUSET` | Body size after normalization divided by input size. Matters for packed input. |
| `postbor.max_expanded_size` | 256MB (unit kB) | `SUSET` | Absolute body size after normalization. |

The index section is not counted in the expansion limits. Results of edits (`||`, `cbor_set`, subscript assignment, …) are held to `postbor.max_depth` too: `||` can wrap a value in an array, and a stored value deeper than input accepts could not be dumped and restored (found by fuzzing). Values are also limited to PostgreSQL's 1 GB varlena maximum.

Errors raised, by SQLSTATE:

| Condition | SQLSTATE |
|---|---|
| Malformed CBOR, invalid EDN, invalid UTF-8, duplicate keys, invalid bignum | `22P02` invalid_text_representation (`22P03` invalid_binary_representation for `cbor_recv`) |
| Depth or expansion limit exceeded | `54000` program_limit_exceeded |
| Cast out of range | `22003` numeric_value_out_of_range |
| Cast from the wrong CBOR type | `22023` invalid_parameter_value |
| Allocation failure on an input-sized buffer | `53200` out_of_memory |

## 3. Data model and normalization

Every accepted input is normalized into the deterministic encoding. Normalization rules, in addition to well-formedness:

1. **Shortest heads**: integers, lengths, tag numbers and simple values use the shortest head encoding.
2. **Definite lengths only**: indefinite-length arrays and maps become definite. Indefinite-length strings are joined into one definite string. Every chunk must be a definite string of the same major type.
3. **Floats**: stored in the shortest of f16/f32/f64 that represents the value exactly, including subnormals. Any NaN (any sign, any payload) becomes `f97e00`. Infinity is `f97c00`, -Infinity is `f9fc00`, and -0.0 is `f98000` (kept distinct from 0.0).
4. **Map keys**: sorted by bytewise lexicographic order of their deterministic encodings (RFC 8949 core deterministic ordering). After key normalization, duplicate keys are an error. Example: `{1: 0, 2(h'01'): 0}` is rejected.
5. **Bignums (tags 2/3)**: the content must be a byte string. Leading zero bytes are stripped. A value that fits in 64 bits becomes a major type 0 or 1 integer. Tag 2: n ≤ 2⁶⁴−1. Tag 3: −1−n with n ≤ 2⁶⁴−1.
6. **Text strings** must be valid UTF-8.
7. **Simple values** 24–31, and the two-byte form `f8` with a value below 32, are rejected (not well-formed). Every other simple value is kept.
8. **Other tags** are kept as they are, with their content normalized like any other value. The packing tags listed in section 6 are not "other tags": rule 11 consumes them first. Interpreted tags other than 2/3 are not checked on input. Their content is validated when an operation interprets them, for example a cast to `timestamptz`.
9. **Byte strings**, including tag 24 (embedded CBOR) and `<<…>>` output, are opaque. Their content is not normalized.
10. **Single item only**: trailing bytes after the item are an error (CBOR sequences are not accepted). So are reserved additional-info values 28–30 and a break code outside an indefinite-length item.
11. **Packed input** is unpacked (section 6).

Interpreted tags:

| Tag | Meaning | Used by |
|---|---|---|
| 0 | RFC 3339 date-time text | cast to `timestamptz` |
| 1 | epoch date-time (number) | cast to `timestamptz`, `to_cbor(timestamptz)` |
| 2, 3 | bignum | normalization, comparison, `numeric` casts |
| 4 | decimal fraction | `numeric` casts |
| 5 | bigfloat | `numeric` casts (exact only when the exponent is ≥ 0) |
| 37 | UUID (16-byte string) | `uuid` casts |
| 52, 54 | IPv4 / IPv6 address or prefix | EDN `IP''` literal |
| 100 | days since epoch (RFC 8943) | `date` casts, `to_cbor(date)` |

## 4. Datum format

```
datum  = header body [index body_len]
header = 1 byte:
         bits 0-1  format version (0 = this format)
         bit  2    has_index
         bits 3-7  reserved, 0
body   = deterministic encoding of the value
index  = dir_count:u32
         dir_count × { container_offset:u32, table:u32 }
         tables: sampled entry offsets, u16 or u32 each, in directory order
body_len = u32 little-endian, the last 4 bytes of the datum, present only when has_index
```

- `body_len` sits at the end, not after the header, so the body can be written straight into the output buffer before it is known whether an index section will follow. The header byte is written last, in place.
- All integers in the index section are little-endian. They are read through the cursor (`from_le_bytes` on slices), so nothing needs alignment.
- **Indexed container**: an array or map with at least `INDEX_THRESHOLD` (16) entries. Smaller containers are scanned directly.
- **Container directory**: one entry per indexed container, sorted by `container_offset` (offset of the container's head from the start of the body), which is depth-first body order. `table` is the byte position of its entry table in the tables area; its top bit set means the table holds 2-byte offsets.
- **Entry table**: the offset of every `STRIDE`-th (8th) entry: entries 0, 8, 16, …, so ⌈n / 8⌉ offsets for n entries. Each offset is relative to the container's start and points at an array element, or at a map key (its value follows the key). A table uses 2-byte offsets when all of them fit, 4-byte otherwise. Sampling and narrow offsets cut the index from 4 bytes to about 0.25 byte per entry, which lets large values compress (see the benchmark in `TODO.md`).
- **Lookups**:
  - Array element i: read sample ⌊i / 8⌋, then step over i mod 8 elements.
  - Map key: binary search over the sampled keys (comparing the lookup key's deterministic encoding bytewise) for the last sample ≤ the key, then scan at most 8 entries from it.
  - Skipping a whole indexed container: jump to its last sample, then step over the rest.
  - To descend into a nested container: binary search the directory for its offset.
- **Extracting a sub-value** at body range `[s, e)`:
  1. The directory entries with `s ≤ container_offset < e` form a contiguous run `[a, b)`, and their tables are contiguous too.
  2. The new datum is: a header, the body slice, then (if `b > a`) the directory run with `container_offset − s` and table positions moved down by the run's first table position (width flags kept), the table bytes, and `body_len`.
  3. No entry table is rebuilt.
- **Building**: the index is built in one walk over the finished body (also used for values built by edits). The same walk enforces `postbor.max_depth`.
- **Detoasting**: datums are read with `pg_detoast_datum_packed` (a 1-byte varlena header is fine, since we only read bytes).
- **Format version**: any version other than 0 is rejected with an error asking the user to upgrade the extension.

## 5. Text format (EDN)

### Input (`cbor_in`, `text::cbor`)

Accepted:
- **Whitespace and comments**: `# …` to end of line, `/ … /`.
- **Integers**: decimal with optional sign, `0x`/`0o`/`0b` prefixes. Any magnitude: values beyond 64 bits become bignums (tags 2/3).
- **Floats**: decimal with `.` and/or an exponent; `NaN`, `Infinity`, `-Infinity`.
- **Encoding indicators**: `_0`–`_3` and `_i` after a number or a container or string opener, and `_` for indefinite length (`[_ …]`, `{_ …}`, `(_ "a", "b")`). They are parsed and ignored, since the value is normalized anyway.
- **Text**: `"text"` with JSON escapes, including `\u` surrogate pairs.
- **Byte strings**:
  - `'text'`: the UTF-8 bytes of the text.
  - `h'…'`: hex, with whitespace and comments allowed inside.
  - `b64'…'`: standard or URL-safe alphabet, padding optional.
  - `<<item, …>>`: the deterministic encodings of the items, concatenated.
- **Containers**: `[ … ]` and `{ key: value, … }`. A trailing comma is an error.
- **Tags**: `N(item)`.
- **Simple values**: `simple(n)`, `true`, `false`, `null`, `undefined`.
- **Application literals** (draft-ietf-cbor-edn-literals-29):

  | Literal | Result |
  |---|---|
  | `dt'…'` | RFC 3339 → epoch number: an integer, or a float when there is a fractional part |
  | `DT'…'` | the same number wrapped in tag 1 |
  | `ip'…'` | address → byte string; prefix (`/n`) → `[n, trimmed bytes]` |
  | `IP'…'` | the same wrapped in tag 52 (IPv4) or tag 54 (IPv6) |

Rejected with a syntax error naming the feature: `b32''`/`h32''`, string concatenation, ellipsis `...`, raw strings, hex floats, other application literals.

### Output (`cbor_out`, `->>` on anything other than a text string)

Core EDN only, written deterministically:
- **Separators**: `", "` between items and `": "` after keys, as `jsonb_out` does.
- **Integers**: decimal.
- **Bignums over 64 bits**: `2(h'…')` / `3(h'…')`.
- **Floats**: the shortest decimal that reads back as the same value. It always contains `.` or `e` (`1.0`, `1.5e300`). Special values print as `NaN`, `Infinity`, `-Infinity`, `-0.0`.
- **Text**: JSON escaping; other characters are written as they are (databases are UTF-8, [ADR-0010](../adr/0010-utf8-databases-only.md)).
- **Bytes**: `h'…'` in lowercase hex.
- **Tags**: `N(item)`.
- **Simple values**: `simple(n)`, `true`, `false`, `null`, `undefined`.

`cbor_diag(c, pretty => true)` and `cbor_pretty(c)` indent with 4 spaces, one entry per line, like `jsonb_pretty`.

Only UTF-8 databases are supported: `CREATE EXTENSION` refuses others ([ADR-0010](../adr/0010-utf8-databases-only.md)).

## 6. Packed input

Unpacking happens during normalization, on every input path (`cbor_in`, `cbor_recv`, `bytea::cbor`, `cbor_from_bytea`). There is no opt-out: stored data never contains packing ([ADR-0005](../adr/0005-unpack-on-input.md)).

**Idempotence (required):** normalizing a stored body again gives exactly the same body. If unpacking could be turned off, a stored packing tag would be unpacked on restore (`pg_dump` goes through `cbor_out`/`cbor_in` or `send`/`recv`), and the value would change. This is why there is no opt-out.

Packing schemes recognized:
- **IETF Packed CBOR** (draft-ietf-cbor-packed-19):
  - Table setup tags 113 and 1113.
  - Inside their scope: shared item references (simple values 0–15, tag 6 with an integer), argument references (tags 128–143, tag 6 with an array), and function tags 105 (ijoin), 106 (join) and 114 (record).
  - Outside any table setup tag: simple values 0–15 are plain simple values, and tags 6, 105, 106, 114 and 128–143 are plain tags. Tables supplied by the application environment (for example a media type) are not supported.
- **cbor-x records** (cbor-records spec; the tags the current cbor-x encoder emits):
  - 57342: record definitions
  - 57343: inline record
  - 57344–65535 (`0xe000 + id`): record references, unpacked to maps with text keys. cbor-x reads every tag from `0xe000` as a reference (8192 ids), wider than the cbor-records spec's 57344–57599.

  A reference to a structure not defined earlier in the same value is an error. This covers cbor-x shared and sequential structures, which are kept outside the data. cbor-x's legacy inline-record tag 105 is not supported: it clashes with IETF ijoin.
- **cbor-x's own packing** (`pack` option): an older packed-CBOR draft, using tag 51 for the table, with tag 6 and simple-value references inside its scope.
- **cbor-x bundled strings** (`bundleStrings`): not supported. cbor-x writes the strings after the value, so the input is not a single item and is refused.

Limits: expansion is bounded by `postbor.max_expansion_ratio` and `postbor.max_expanded_size`, nesting by `postbor.max_depth`. A reference cycle is an error.

Implementation (`core/src/pack.rs`):
- Normalization stops with an internal signal when it meets a table-setup or record tag (113, 1113, 51, 57342–65535); only then is the input decoded into an arena and resolved. Unpacked input pays nothing.
- Resolution uses an explicit task stack and per-tag scopes: new table entries use the new number space, inherited ones keep theirs (draft §3).
- Argument references combine argument and rump per draft §2.3–2.4: arrays concatenate, maps merge (right wins, `undefined` removes), strings concatenate taking the rump's string type, string with array joins. Function tags 105 (ijoin), 106 (join) and 114 (record) apply when the left-hand side is such a tag.
- cbor-x tag 51 is `[values, prefixes, suffixes, rump]`; only the value table is used (cbor-x's encoder writes empty prefix and suffix tables).
- Reference chains longer than 64 are rejected (loops); every arena node is charged against a memory budget of four times the output limit.
- Output packing is deterministic: `ietf` puts strings of 3+ bytes used at least twice into a tag 113 table (largest saving first, ties by bytes), except when the value holds plain simple values 0–15 or tags 6/128–143, which would read as references inside tag 113 (found by fuzzing): such values are returned unpacked; `cbor-x` turns maps whose text-key set repeats into an inline record definition (57343) at first use and record references (57344+) afterwards, up to 8191 structures (ids up to `0xfffe`; cbor-x confuses id 8191 with its inline-definition tag).
- A value packed more than `postbor.max_expansion_ratio` times smaller than its unpacked form is rejected on input; raise the setting for such data.

Output: `cbor_pack(c cbor, format text DEFAULT 'cbor-x') → bytea`.
- `'cbor-x'` emits records (57342/57344+).
- `'ietf'` emits tag 113.
- The packer must be deterministic: the same value always gives the same bytes.

## 7. Comparison, equality, hashing

- **Equality** is byte equality of bodies. The header and index section are never compared.
- **Hashing** works on the body bytes: `hash_bytes` / `hash_bytes_extended` with a seed.
- **Ordering** (btree support function 1) must satisfy `cmp(a, b) = 0 ⟺ body(a) = body(b)`. First by type rank:
  1. numbers: unsigned and negative integers, floats, bignums (tags 2/3)
  2. byte strings
  3. text strings
  4. arrays
  5. maps
  6. other tags
  7. unassigned simple values
  8. false
  9. true
  10. null
  11. undefined
- **Within a rank:**
  - **Numbers**: compared by exact mathematical value. Integers and floats are compared exactly, without going through f64. NaN sorts after every other number. When values are equal: integer < float, and -0.0 < 0.0.
  - **Byte and text strings**: bytewise lexicographic; a prefix sorts first. Text is compared as UTF-8 bytes, with no collation ([ADR-0007](../adr/0007-natural-collation-free-order.md)).
  - **Arrays**: element by element; a prefix sorts first.
  - **Maps**: fewer entries sort first. Then key 1, value 1, key 2, … in stored order.
  - **Tags**: by tag number, then by content.
  - **Simple values**: by number.

The property tests in section 10 check antisymmetry, transitivity, and agreement with byte equality and hashing.

## 8. SQL surface

### Type and I/O

`cbor` (varlena, storage `extended`), with:
- `cbor_in` / `cbor_out`: EDN.
- `cbor_recv` / `cbor_send`: the deterministic CBOR body. `recv` goes through full validation and normalization.

### Operators

| Operator | Right operand | Result | Notes |
|---|---|---|---|
| `->` | `int` | `cbor` | Array index (negative counts from the end), or integer key on a map |
| `->` | `text`, `bytea` | `cbor` | Text key, byte-string key |
| `->>` | same as `->` | `text` | Text string → its text; `null` → SQL NULL; anything else → EDN |
| `#>` / `#>>` | `text[]` | `cbor` / `text` | Text path, jsonb rules |
| `@>`, `<@` | `cbor` | `bool` | Containment, jsonb semantics. Tags: same number and the content contains, except decimal fractions (tag 4) and bigfloats (tag 5), which are numbers and must be equal |
| `?` | `text` | `bool` | Map key exists, or a top-level array element is that text string |
| `?\|`, `?&` | `text[]` | `bool` | Any / all |
| `?#` | `cbor` | `bool` | `?` with a key of any type (`cbor_has`) |
| `?#\|`, `?#&` | `cbor` (an array of keys) | `bool` | Any / all, keys of any type (`cbor_has_any`, `cbor_has_all`) |
| `\|\|` | `cbor` | `cbor` | jsonb concatenation semantics |
| `-` | `text`, `text[]`, `int`, `bytea` | `cbor` | Delete a key, or delete an element (`int`: by index) |
| `#-` | `text[]` | `cbor` | Delete at path |
| `= <> < <= > >=` | `cbor` | `bool` | Section 7 |
| `@?`, `@@` | `jsonpath` | `bool` | Phase 3 |

Out-of-range indexes and missing keys return NULL, as in jsonb.

**Why keys of any type and typed paths are not operators.** Tested on PG 18: once an operator `(cbor, cbor)` exists, an untyped literal on the right is read as `cbor`. `doc -> 'a'` then fails as invalid EDN, and `doc -> '1'` silently looks up integer key 1. PostgreSQL first tries the left operand's type for an unknown literal. A separate key type does not help for paths either: `#> '{a}'` with both `text[]` and another type on the right is rejected as "operator is not unique". Only types that an untyped literal never resolves to are overloaded on operators (`int`, `bytea`, `text`, `text[]`). Keys of any type and typed paths go through functions with their own names (below). The existence checks also get operators with their own symbols, `?#`, `?#|` and `?#&`, so that GIN indexes can serve them: a symbol with a single `(cbor, cbor)` definition captures no other operator's literals. 

Typed-key functions:

| Function | Equivalent |
|---|---|
| `cbor_get(doc, key cbor)`, `cbor_get_text(doc, key cbor)` | `->` / `->>` with a key of any type, or an array index |
| `cbor_get_path(doc, path cbor)`, `cbor_get_path_text(doc, path cbor)` | `#>` / `#>>` with a typed path |
| `cbor_has(doc, key cbor)`, `cbor_has_any(doc, keys cbor)`, `cbor_has_all(doc, keys cbor)` | `?`, `?\|`, `?&` with keys of any type (`keys` is a CBOR array) |
| `cbor_delete(doc, key cbor)`, `cbor_delete_path(doc, path cbor)` | `-`, `#-` |
| `cbor_set_path`, `cbor_set_lax_path`, `cbor_insert_path` | `cbor_set`, `cbor_set_lax`, `cbor_insert` with a typed path |

These use different names rather than overloads because function overloads `(cbor, text[])` and `(cbor, cbor)` are equally ambiguous for an untyped literal.

Selectivity estimators: `matchingsel`/`matchingjoinsel` for `@> <@ ? ?| ?& ?# ?#| ?#&`; `eqsel`/`scalarltsel` etc. for comparisons.

Operator classes: btree `cbor_ops` (default) and hash `cbor_ops` (default, with an extended hash function), and the GIN classes below.

### GIN indexes

Two operator classes, as for jsonb. Both are lossy: every match is rechecked against the row.

| Class | Operators | Entries | Storage |
|---|---|---|---|
| `cbor_ops` (default) | `@>` `?` `?\|` `?&` `?#` `?#\|` `?#&` | One per map key and one per atomic item, without their position | `bytea` |
| `cbor_path_ops` | `@>` | One per atomic item: a hash of the path leading to it and of the item | `int4` |

Both come from one walk over the value (core `gin`), in body order:
- **Atomic item**: one that containment compares whole: a scalar, a tag whose content (through further tags) is atomic, or a decimal fraction or bigfloat (tags 4, 5). Arrays, maps and other tags around them are walked into.
- **`cbor_ops` entry**: a role byte, then the item's deterministic encoding. Role 1 (key) for map keys, of any type and whole, and for array elements and the top-level value; role 2 for map values. Sharing the key role lets `?` find array elements, as jsonb does. An item whose encoding is 125 bytes or longer gets instead the role with bit 7 set and a 4-byte hash of the encoding.
- **`cbor_path_ops` entry**: a 32-bit hash over the path steps and the item: each map key's encoding and each walked tag's number, with a marker byte per step kind. Arrays add no step, since array containment ignores positions; so `[1, 2] @> 1` holds for the index too. The hash is FNV-1a with a final mix.
- Entries are on disk: changing the walk, the roles or the hashes requires a REINDEX.

Queries:
- `@>`: the query value's own entries; the row must have all of them. A query with no entries (`{}`, `[]`, `{"a": {}}`) scans the whole index and rechecks every row.
- `?`, `?|`, `?&`: the key-role entry of each text key. `?&` with no keys matches every row; `?|` with none matches no row.
- `?#`, `?#|`, `?#&`: the key-role entry of each key. A key that is not atomic cannot be found among array elements through entries, so `?#` with such a key, or `?#|` with any such key, rechecks every row; `?#&` skips such keys.
- Strategy numbers: 7 `@>`, 9 `?`, 10 `?|`, 11 `?&` (jsonb's), 12 `?#`, 13 `?#|`, 14 `?#&`.

### Subscripting

`cbor_subscript_handler`, modelled on `jsonbsubs.c`:
- The subscript's type picks the meaning, just like `->`: `int`, `text`, `bytea` or `cbor`.
- Fetch behaves like `->`.
- Assignment creates missing levels like jsonb subscripting: a missing key step creates a map, a missing integer step creates an array padded with nulls; assigning past the end of an existing array pads it with nulls. A NULL column is treated as empty. Assigning below a scalar is an error, as is a NULL subscript.
- Subscripts of type `int2`/`int4`/`int8` index arrays or address integer map keys; `bytea` addresses byte-string keys; `cbor` addresses keys of any type; anything else (and untyped literals) is text.

### Functions with a jsonb counterpart

| jsonb | cbor | Differences |
|---|---|---|
| `to_jsonb(anyelement)` | `to_cbor(anyelement)` | See "to_cbor mapping" below |
| `jsonb_build_array(VARIADIC "any")` | `cbor_build_array` | |
| `jsonb_build_object(VARIADIC "any")` | `cbor_build_object` | Keys keep their type (an `int` argument becomes an integer key) |
| `jsonb_object(text[])`, `jsonb_object(text[], text[])` | `cbor_object` | |
| `jsonb_agg`, `jsonb_agg_strict` | `cbor_agg`, `cbor_agg_strict` | |
| `jsonb_object_agg[_strict\|_unique\|_unique_strict]` | `cbor_object_agg[…]` | Keys of any type |
| `jsonb_array_elements`, `_text` | `cbor_array_elements`, `_text` | `_text` uses the `->>` rules |
| `jsonb_array_length` | `cbor_array_length` | |
| `jsonb_each`, `_text` | `cbor_each`, `_text` | `cbor_each` returns `(key cbor, value cbor)`; `_text` returns both as text |
| `jsonb_object_keys` | `cbor_object_keys` | Returns `setof cbor`, keeping the key's type |
| `jsonb_extract_path`, `_text` | `cbor_extract_path`, `_text` | `VARIADIC text[]` |
| `jsonb_populate_record`, `_recordset` | `cbor_populate_record`, `_recordset` | Text keys match columns; other keys are ignored. Nested composites and 1-D arrays are converted; tags 0/1, 37 and 100 go to `timestamptz`, `uuid` and `date` columns. `_valid` is not implemented yet |
| `jsonb_to_record`, `_recordset` | `cbor_to_record`, … | |
| `jsonb_strip_nulls(…, strip_in_arrays)` | `cbor_strip_nulls(…, strip_in_arrays DEFAULT false, strip_undefined DEFAULT false)` | `undefined` is only stripped when `strip_undefined` is true |
| `jsonb_set`, `jsonb_set_lax`, `jsonb_insert` | `cbor_set`, `cbor_set_lax`, `cbor_insert` | `text[]` path; typed-path versions are the `*_path` functions above |
| `jsonb_delete_path` (`#-`) | `cbor_delete_text_path` (`#-`) | Renamed so it cannot be confused with the typed `cbor_delete_path(cbor, cbor)` |
| `jsonb_pretty` | `cbor_pretty` | |
| `jsonb_typeof` | `cbor_typeof` | Returns `integer`, `float`, `bytes`, `text`, `array`, `map`, `tag`, `simple`, `boolean`, `null`, `undefined` |
| `jsonb_path_*` | `cbor_path_*` | Phase 3 |

The set-returning functions run in value-per-call mode, using core iterators that borrow the detoasted datum. If pgrx 0.20 requires `'static` iterators, the datum is copied once into the multi-call memory context.

`populate_record` value conversion:
- a text string goes through the column type's input function
- an integer or float goes to numeric columns directly
- bytes go to `bytea`
- a map goes to a composite column
- an array goes to an array column
- anything else goes through its EDN text into the column's input function

### CBOR-specific functions

| Function | Result |
|---|---|
| `cbor_tag(cbor) → int8` | The tag number, or NULL if the value isn't tagged. Error if above `int8` max |
| `cbor_untag(cbor) → cbor` | The tag content, or the value itself if it isn't tagged |
| `cbor_tagged(tag int8, value cbor) → cbor` | Wraps a value in a tag, then normalizes (so `cbor_tagged(2, h'01')` gives `1`) |
| `cbor_from_bytea(bytea) → cbor` | Same as `bytea::cbor`, as a function |
| `cbor_pack(cbor, format text DEFAULT 'cbor-x') → bytea` | Section 6 |
| `cbor_diag(cbor, pretty bool DEFAULT false) → text` | EDN |
| `cbor_simple(int) → cbor`, `cbor_undefined() → cbor` | |
| `cbor_byte_length(cbor) → int8` | Length of the body |
| `cbor_to_jsonb_strict(cbor) → jsonb` | Error on anything JSON can't represent |

### Casts (all explicit)

| From → to | Rule |
|---|---|
| `cbor → bool` | `true`/`false` only |
| `cbor → int2/int4/int8` | Integers in range |
| `cbor → float4/float8` | Integers or floats |
| `cbor → numeric` | Integers, floats (NaN and ±Infinity included), tags 2/3/4, tag 5 when exact |
| `cbor → timestamptz` | Tag 0 (RFC 3339 text) or tag 1 (epoch number) |
| `cbor → uuid` | Tag 37 with 16 bytes |
| `cbor → date` | Tag 100 |
| `cbor → bytea` | The deterministic body |
| `cbor → jsonb/json` | Lossy, RFC 8949 §6.1 ([ADR-0009](../adr/0009-cbor-to-jsonb-is-lossy.md)) |
| `bytea → cbor` | Decode and normalize |
| `jsonb/json → cbor` | Text keys. Integers become integers (bignum if needed). Non-integer numbers become the nearest f64 |

### to_cbor mapping

| PostgreSQL type | CBOR result |
|---|---|
| `bool` | true/false |
| `int2/4/8` | integer |
| `float4/8` | float |
| `numeric` | integer if integral, bignum if beyond 64 bits; otherwise a tag 4 decimal fraction. NaN and ±Infinity become floats |
| `text`, `varchar`, `char` | text string |
| `bytea` | byte string |
| `timestamptz` | `1(float epoch seconds)`, always a float, stored in its shortest exact width |
| `uuid` | `37(h'…')` |
| `date` | `100(days since epoch)` |
| `timestamp` (without time zone) | `1(float epoch seconds)`, treating the value as UTC |
| arrays | arrays, nested for multi-dimensional arrays |
| composites and records | map with text keys (column names) |
| `json`, `jsonb` | the conversion used by the cast |
| `cbor` | itself |
| domains | their base type |
| anything else | its text output, as a text string |

## 9. Code architecture

Cargo workspace, crates fetched from crates.io:

- **`core/` (`postbor-core`)**: pure Rust, no dependencies, `#![forbid(unsafe_code)]`. Every byte-level operation lives here.
  - `cursor`: the only code that indexes into input or body bytes. Every read is bounds-checked and returns `Result`.
  - `normalize`: input bytes → deterministic body, including unpacking (`pack`).
  - `pack`: unpacking packed input, and `cbor_pack`.
  - `encode`: shortest-form heads. `float`: float widths. `bigint`, `num`: bignums and decimal text for `numeric`.
  - `datum`: the stored form (header, body, index section); `index`: building and reading the index section, and rebasing it when extracting.
  - `value`: borrowed views (`Value<'a>`, which borrows its parsed `Datum`) and lazy iterators (`ArrayIter<'a>`, `MapIter<'a>`).
  - `edn`: parser and printer.
  - `cmp`: ordering, and the input to hashing.
  - `ops`: lookups, paths, containment and `?` checks. `edit`: operations that build a new value (concatenation, deletion, set/insert, strip_nulls).
  - `json`: the decision logic for converting to and from JSON. Building and reading the actual jsonb values happens in the glue.
  - `gin`: the walk over a value's keys and atomic items, and the GIN entries built from it.
  - `ctx`, `out`, `error`: limits and interrupt checks, the output buffer trait, errors.
- **`postbor/`**: the pgrx `cdylib`. It holds the SQL definitions, the conversions to and from PostgreSQL types, the settings, and turning core errors into `ereport` calls. `unsafe` is allowed only where raw PostgreSQL APIs are unavoidable: `datum` (varlena wrapper, `PallocBuf`), `io` (receive buffer, encoding conversion), `convert` (`to_cbor` over any type: arrays, composites, catalog lookups), `build` (`VARIADIC "any"` and aggregate states), `record` (row construction), `subscript`, and `gin` (GIN support functions). Each `unsafe` block carries a one-line comment stating its invariant.
- **jsonb conversion** goes through JSON text: `cbor → jsonb` prints JSON with the core's `json` module and calls `jsonb_in`; `jsonb/json → cbor` reads `jsonb_out`/`json_out` text with the EDN parser (JSON is a subset of EDN). Replace with direct `JsonbValue` building only if benchmarks show it matters.
- **`fuzz/`**: cargo-fuzz targets. Depends only on `postbor-core`. Not part of the default workspace build.

Core rules:
- **No `powi`/`powf`** or other float functions without exact results: only basic IEEE operations and bit-level construction (`float::pow2`). Miri caught `powi` giving different results.
- **No recursion.** Every walk keeps an explicit stack, limited by `max_depth`.
- **Checked arithmetic** for every offset and length, and `overflow-checks = true` in the release profile.
- **Fallible allocation** (`try_reserve`) for any buffer whose size comes from the input. The Rust allocator is `malloc`, and an allocation failure that isn't handled would abort the backend.
- **A declared length is never trusted for allocation.** It is first checked against the bytes remaining.
- **Errors** are `Result<_, Error { kind, offset }>`. A panic means a bug.
- **Limits and cancellation**: operations that loop over input take a context (`trait Ctx`) carrying the limits and a `check_interrupts()` hook, called every 4096 items. The glue implements the hook with pgrx's `check_for_interrupts!()`.
- **Output goes straight into PostgreSQL memory.** Every core function that produces bytes (datums, EDN, `->>` text, `bytea`) writes through a buffer trait (`OutBuf`: reserve, append, patch a byte at a position, length). The core implements it for `Vec<u8>` for tests, Miri and fuzzing.
  - The glue's implementation (`PallocBuf`) allocates in the current memory context, with the 4-byte varlena header reserved at the front. It grows by doubling with `repalloc`, sets `VARSIZE` when finished, and returns the buffer as the datum, with no copy.
  - Start capacity: the input size when there is one (normalization, modifying functions), otherwise a small constant.
  - Aggregates write into a buffer allocated in the aggregate memory context.
  - Running out of memory in `palloc`/`repalloc` raises a PostgreSQL error. pgrx turns it into a panic, so Rust cleans up correctly, and the memory context frees the buffer.

## 10. Testing

- **Core unit tests** (`cargo test -p postbor-core`): every normalization rule, every EDN form, format edge cases (index threshold boundaries, extraction rebasing).
- **Miri** on `postbor-core` tests.
- **Property tests**, written as fuzz targets with `arbitrary` that generate valid values:
  - decode → encode → decode is the same value
  - EDN out → in is the same value
  - `cmp` is antisymmetric and transitive, and `cmp = 0 ⟺` the bodies are equal, which implies equal hashes
  - an index lookup agrees with a linear scan
  - `cbor_set` followed by `->` returns the value that was set
  - extracting a sub-value and re-normalizing it gives the same datum
  - normalizing any stored body again gives the same body (idempotence, section 6)
- **Byte-input fuzz targets**: normalize (including packed input) and the EDN parser. The corpus is committed under `fuzz/corpus/`. Seeds: RFC 8949 Appendix A, regression cases, `cbor2` output.
- **Cross-check against Python `cbor2`** (a test-only tool, `uv run --with cbor2 tests/crosscheck.py [count]`): random values encoded by `cbor2` (plain and canonical) must be accepted, and their normalized form must decode to the same value, compared type-strictly (1 ≠ 1.0, -0.0 ≠ 0.0). It drives the core through `core/examples/normalize_lines.rs`.
- **SQL tests**: `pg_regress` suites under `postbor/tests/pg_regress/`, run by `cargo pgrx regress` against pgrx-managed PostgreSQL 15, 16, 17 and 18 (`make test-matrix`). `make test` runs them against the major of `PG_CONFIG`.

## 11. Dependencies

Approved: `pgrx` (and its dependency tree); for fuzzing only, `libfuzzer-sys` and `arbitrary`; for cross-checks only, Python `cbor2`. Any other dependency needs the user's approval first.

## 12. Open decisions

None. Add new ones here, and ask the user before implementing what depends on them.
