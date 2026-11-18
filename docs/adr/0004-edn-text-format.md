# EDN is the text format

`cbor_in` and `cbor_out` use CBOR Extended Diagnostic Notation (EDN), not JSON. JSON cannot represent byte strings, tags, non-text keys or undefined, so a JSON text form could not be read back reliably. Input accepts the RFC 8949 §8 core plus a chosen subset of the EDN draft: comments, embedded CBOR `<<…>>`, whitespace and comments inside `h''`, and the `dt`/`DT` and `ip`/`IP` application literals. Output is core EDN only. Encoding indicators are accepted and ignored, because every value is normalized to its deterministic encoding (see [ADR-0002](0002-storage-deterministic-body-plus-index.md)).

JSON goes through casts in both directions (`cbor::jsonb`, `jsonb::cbor`, and the same for `json`).
