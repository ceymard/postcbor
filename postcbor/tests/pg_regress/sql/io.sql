-- Text input is normalized: keys sorted, shortest forms, indefinite lengths removed.
SELECT '{"b": 1, "a": [1, 2.5, h''ff'', 1(0)], 10: null}'::cbor;
SELECT '[_ 1, 1.0, 1.5e300, -0.0, NaN, Infinity, -Infinity]'::cbor;
SELECT '18446744073709551616'::cbor, '-18446744073709551617'::cbor, '2(h''01'')'::cbor;
SELECT '<<1, "a">>'::cbor, '(_ "ab", "cd")'::cbor, 'b64''SGk='''::cbor;
SELECT 'dt''2024-01-01T00:00:00Z'''::cbor, 'DT''2024-01-01T00:00:00.5Z'''::cbor, 'IP''192.0.2.0/24'''::cbor;
SELECT '"é \u0001 \"q\""'::cbor;
-- Invalid text.
SELECT '[1, 2'::cbor;
SELECT '[1,]'::cbor;
SELECT '{1: 2, 1: 3}'::cbor;
SELECT '{1: 2, 2(h''01''): 3}'::cbor;
SELECT 'b32''AA'''::cbor;
-- Stored values read back unchanged, including ones whose text fills the output buffer exactly.
CREATE TABLE io_t (c cbor);
INSERT INTO io_t VALUES ('{"x": [1, 2, 3]}'), ('"abcdefghijklmn"'), ('[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17]');
SELECT c FROM io_t;
-- Binary send/recv round-trip through COPY.
COPY io_t TO '/tmp/postcbor_io_t.bin' WITH (FORMAT binary);
CREATE TABLE io_t2 (c cbor);
COPY io_t2 FROM '/tmp/postcbor_io_t.bin' WITH (FORMAT binary);
SELECT c FROM io_t2;
SELECT cbor_send('{"b": 1, "a": 2}');
DROP TABLE io_t, io_t2;
-- Limits.
SET postcbor.max_depth = 2;
SELECT '[[1]]'::cbor;
SELECT '[[[1]]]'::cbor;
RESET postcbor.max_depth;
