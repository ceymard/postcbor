# postcbor

A PostgreSQL extension that adds a `cbor` data type with the same role, operators and functions as `jsonb`, extended to the full CBOR data model.

## Language

### Values and storage

**CBOR value**:
A single CBOR data item as seen by SQL users: what a `cbor` column holds.
_Avoid_: document, object (for the whole value)

**Datum**:
The stored form of one CBOR value: a header, a body and an optional index section.
_Avoid_: blob, record

**Header**:
The first byte of a datum, holding the format version and flags.

**Body**:
The deterministic encoding of the CBOR value. It is valid CBOR on its own and is what output functions emit.
_Avoid_: payload, raw bytes

**Index section**:
The part of a datum after the body that holds entry offset tables for large containers.
_Avoid_: offset table (for the whole section), TOC

**Indexed container**:
An array or map with at least the index threshold of entries, and so listed in the index section.

**Entry table**:
The list of offsets, relative to the container start, of every entry (array element or map key) of one indexed container.

**Container directory**:
The list, in body order, of indexed containers and the location of their entry tables.

**Deterministic encoding**:
The single canonical byte form every CBOR value is stored in. It extends RFC 8949 core deterministic encoding with the normalization rules in the design doc.
_Avoid_: canonical CBOR, preferred serialization (both mean different, older things)

**Normalization**:
Turning any accepted input into its deterministic encoding, including unpacking.

### Data model

**Map**:
A CBOR map (major type 5). Its keys can be any CBOR value.
_Avoid_: object, dictionary

**Key**:
The first half of a map entry, of any CBOR type.
_Avoid_: field, property, member

**Typed path**:
A path given as a CBOR array, where each step keeps its exact CBOR type (integer, text, bytes, …).

**Text path**:
A path given as `text[]`, with jsonb rules: text keys, numeric steps used as array indexes.

**Tag**:
A CBOR tag number with its content. Always kept. Only some tags are interpreted.
_Avoid_: annotation, extension (alone)

**Interpreted tag**:
A tag whose meaning postcbor uses in some operation (casts, comparison, normalization): 0, 1, 2, 3, 4, 5, 37, 52, 54, 100.

**Simple value**:
A CBOR major type 7 value that is not a float: false, true, null, undefined, or an unassigned simple(n).

**Type rank**:
The position of a CBOR type in the sort order, compared before values.

### Input and output

**EDN**:
CBOR Extended Diagnostic Notation, the text form of `cbor` values.
_Avoid_: diagnostic notation (alone), diag

**Packed input**:
CBOR that uses a packing scheme (IETF Packed CBOR, cbor-x records) to share repeated items. It is always unpacked during normalization. postcbor never stores it.
_Avoid_: compressed CBOR

**Packing format**:
A named packing scheme for `cbor_pack` output: `ietf` (IETF Packed CBOR) or `cbor-x` (cbor-x records).

### Code structure

**Core**:
The pure Rust crate (`postcbor-core`) that holds every byte-level operation. It has no PostgreSQL dependency.
_Avoid_: library, engine

**Glue**:
The pgrx crate (`postcbor`) that exposes the core to SQL.
_Avoid_: bindings, wrapper
