# Stored form: deterministic CBOR body plus an index section

A datum is a one-byte header, then the value in deterministic encoding (the body), then an optional index section listing entry offsets for containers with many entries. The body is still valid CBOR, so output, `send`/`recv` and `::bytea` are plain copies, and equality is a byte comparison. The index section gives binary search and direct jumps inside large containers, which plain CBOR cannot do: arrays and maps record item counts, not byte lengths, so reaching an entry means walking every earlier entry.

Entry tables keep the offset of every 8th entry only, in 2 bytes when they fit: a first version with a 4-byte offset per entry made large values about 37% bigger than their body and kept them from compressing, so they ended up larger than jsonb. Offsets are relative to their container's start, and the container directory is in body order. Extracting a sub-value is therefore a copy of a body slice plus a contiguous run of index entries, shifted by a constant.

## Considered options

- **Raw CBOR, byte-exact (like `json`).** Rejected: every lookup is a linear walk, and equality needs normalization.
- **Deterministic CBOR only.** Rejected: lookups in large documents cost time proportional to all the bytes before the key.
- **A jsonb-style tree that is not CBOR.** Rejected: output and binary transfer would need a conversion every time.
- **IETF Packed CBOR or cbor-x records as the stored form.** Rejected: packing is not deterministic, so equality, hashing and btree would have to unpack on every comparison, and every operator would have to resolve references. See [ADR-0005](0005-unpack-on-input.md).

## Consequences

The header carries a format version, so a later format (for example a key dictionary inside each value) can be added without rewriting tables.
