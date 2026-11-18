# Packed CBOR is unpacked on input, never stored

Packed input (IETF Packed CBOR, cbor-x records) is expanded to its logical value during normalization. Packing only exists on output, through `cbor_pack()`. Storing packed values would break byte-comparison equality, and would force btree, hash and every operator to resolve references on each call. The storage gain is small: TOAST compression already removes repetition in values over about 2 KB.

Unpacking always runs, on every input path, with no opt-out. Otherwise a value stored with its packing would be unpacked on restore (`pg_dump`), and it would change.

Expansion is limited (ratio and absolute size settings), because packed input can expand far beyond its own size.

## Considered options

- **Shared dictionaries outside the value** (cbor-x shared structures, kept in a catalog table). Rejected: a value would depend on another table, which breaks `pg_dump`, logical replication and TOAST's assumption that a value stands on its own. Input that refers to such outside structures is rejected.
- **A key dictionary inside each value**, built deterministically. Left for later, if measurements on small, repetitive values justify it. The header's format version makes room for it.
