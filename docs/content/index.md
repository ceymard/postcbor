# postbor

postbor is a PostgreSQL extension that adds a `cbor` type: the CBOR counterpart of `jsonb`. Values are stored in one canonical binary form, with the operators, functions and indexes you know from `jsonb`, extended to everything CBOR can hold: byte strings, tags, map keys of any type, exact integers of any size, floats kept apart from integers, `undefined` and simple values.

!!! info "Built with AI"
    postbor was written entirely by an AI model, Claude, directed by a human. See [How this was built](how-built.md) before you rely on it.

```sql
CREATE EXTENSION postbor;

SELECT '{"name": "ann", 1: h''cafe'', "when": 1(1700000000), "n": 2.5}'::cbor;
-- {1: h'cafe', "n": 2.5, "name": "ann", "when": 1(1700000000)}

SELECT '{"name": "ann", 1: h''cafe''}'::cbor -> 1;
-- h'cafe'
```

## When to use it

- **Your application already speaks CBOR.** JavaScript applications using cbor-x can send their request bodies straight to a `cbor` column: cbor-x records and packing are unpacked on input, and `cbor_pack` gives them back in cbor-x's form.
- **CWT and COSE tokens.** Their claims maps use integer keys and byte strings; `cwt -> 1` reads key 1 directly.
- **Device and telemetry payloads**, which are often CBOR already, with tags for dates and binary fields.
- **Exact data.** Integers beyond 64 bits stay exact, `1` and `1.0` stay distinct, bytes stay bytes. Small values also take less space than in `jsonb`.

## Where to go next

- [Install](install.md) it.
- [Coming from jsonb](coming-from-jsonb.md) lists the differences that might surprise you.
- [Text form](text-form.md) shows how to write values in SQL.
- [Operators](operators.md), [Functions](functions.md), [Path queries](path-queries.md) and [Indexes](indexes.md) are the reference.

postbor supports PostgreSQL 15 to 18, on UTF-8 databases only.
