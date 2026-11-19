# Settings and limits

postbor treats every input as hostile: values arrive from applications over the network through `::cbor`, `bytea::cbor` and the binary protocol. Every input path goes through the same checks and limits.

## Settings

| Setting | Default | Limits |
|---|---|---|
| `postbor.max_depth` | 512 | Nesting depth of a value. Each array, map and tag counts as one level |
| `postbor.max_expansion_ratio` | 16 | Size of a value after unpacking, divided by the size of its input |
| `postbor.max_expanded_size` | 256MB | Size of a value after unpacking |

Only superusers can change them (`SET`, `ALTER ROLE … SET`, `ALTER DATABASE … SET`, or the server configuration).

The depth limit also applies to values built by functions and operators (`||`, `cbor_set`, `cbor_build_array`, subscript assignment, …): a value deeper than input accepts could be stored but not dumped and restored.

The expansion limits matter for [packed CBOR](binary-form.md#packed-cbor), where a small input can unpack to a very large value. Raise `postbor.max_expansion_ratio` for data that is legitimately packed more than 16 times.

Values are also limited by PostgreSQL's maximum of 1 GB per field.

## Errors

| Condition | SQLSTATE |
|---|---|
| Malformed CBOR, invalid EDN, invalid UTF-8, duplicate map keys, invalid bignum | `22P02` invalid_text_representation for text input; `22P03` invalid_binary_representation for bytes (`bytea::cbor`, the binary protocol) |
| Depth or expansion limit exceeded | `54000` program_limit_exceeded |
| Cast out of range | `22003` numeric_value_out_of_range |
| Cast from the wrong CBOR type | `22023` invalid_parameter_value |
| Not enough memory for an input-sized buffer | `53200` out_of_memory |

```sql
SELECT repeat('[', 600)::cbor;
-- ERROR:  cbor value rejected: nesting depth exceeds postbor.max_depth at byte 512
```
