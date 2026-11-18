-- Packed input is unpacked on every input path; stored values never contain packing.
SELECT '113([["foobar", h''666f6f62'', "fo"], [128("t"), 129("art"), 130("obart")]])'::cbor;
SELECT '113([[114(["key1", "key2", "key0"])], [128(["value 1", 2, false]), 128(["", 0])]])'::cbor;
SELECT '[57343([57344, ["name", "age"], "ann", 7]), 57344(["bob", 9])]'::cbor;
SELECT '\xd83384816361626380808281e0e0'::bytea::cbor AS cbor_x_tag51;
-- Outside a table setup tag, references are plain values.
SELECT '[simple(0), 6(1), 128("x")]'::cbor;
-- Errors: undefined record, reference loop, table too small.
SELECT '57344([1])'::cbor;
SELECT '113([[simple(0)], simple(0)])'::cbor;
SELECT '113([["a"], simple(5)])'::cbor;
-- cbor_pack output reads back to the same value.
WITH v(c) AS (SELECT cbor_agg(cbor_build_object('name', 'user ' || i, 'status', 'active', 'role', 'administrator')) FROM generate_series(1, 30) i)
SELECT cbor_pack(c, 'cbor-x')::cbor = c AS cbor_x_round_trip, cbor_pack(c, 'ietf')::cbor = c AS ietf_round_trip,
       length(cbor_pack(c, 'cbor-x')) < cbor_byte_length(c) AS cbor_x_smaller, length(cbor_pack(c, 'ietf')) < cbor_byte_length(c) AS ietf_smaller FROM v;
SELECT cbor_pack('[{"a": 1}, {"a": 2}]', 'cbor-x'), cbor_pack('["abcd", "abcd"]', 'ietf');
SELECT cbor_pack('1', 'zip');
