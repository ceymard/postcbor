CREATE TYPE rec_inner AS (x int, y text);
CREATE TYPE rec AS (a int, b text, c numeric, d bytea, e timestamptz, f int[], g rec_inner, h cbor, i jsonb, j date, k uuid, l bool);
SELECT * FROM cbor_populate_record(NULL::rec, '{"a": 1, "b": "two", "c": 2(h''010000000000000000''), "d": h''ff'', "e": 1(1704067200), "f": [1, null, 3], "g": {"x": 5, "y": "z"}, "h": {1: 2}, "i": [h''01''], "j": 100(19723), "k": 37(h''a3bb189e8bf938889912ace4e6543002''), "l": true, 99: "ignored"}');
-- Missing keys keep the base row's values; null gives NULL.
SELECT * FROM cbor_populate_record(ROW(10, 'base', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL)::rec, '{"b": null}');
SELECT * FROM cbor_populate_recordset(NULL::rec_inner, '[{"x": 1, "y": "a"}, {"x": 2}]');
SELECT * FROM cbor_to_record('{"a": 1, "b": [1, 2]}') AS t(a int, b text);
SELECT * FROM cbor_to_recordset('[{"a": 1}, {"a": 2, "b": "x"}]') AS t(a int, b text);
SELECT * FROM cbor_populate_record(NULL::rec_inner, '{"x": "not a number"}');
SELECT * FROM cbor_populate_recordset(NULL::rec_inner, '{"x": 1}');
DROP TYPE rec;
DROP TYPE rec_inner;

-- As jsonb's populate_record: domains are checked, NULL and absent columns included when there is no base row.
CREATE DOMAIN rec_pos AS int[] CHECK (VALUE[1] > 0);
CREATE DOMAIN rec_nn AS int NOT NULL;
CREATE TYPE rec_d AS (a rec_pos, b rec_nn, c int);
SELECT cbor_populate_record(null::rec_d, '{"a": [2], "b": 1, "c": 3}');
SELECT cbor_populate_record(null::rec_d, '{"a": [-1], "b": 1}');
SELECT cbor_populate_record(null::rec_d, '{"b": null}');
SELECT cbor_populate_record(null::rec_d, '{"c": 5}');
-- With a base row, absent columns keep its values; an empty map returns it.
SELECT cbor_populate_record(row('{1}', 2, 3)::rec_d, '{"c": 9}'), cbor_populate_record(row('{1}', 2, 3)::rec_d, '{}');
-- Only maps populate a row.
SELECT cbor_populate_record(null::rec_d, '[1]');
SELECT cbor_populate_record(null::rec_d, '1');
-- Nested arrays are dimensions, taken from the first elements; sub-arrays must match.
CREATE TYPE rec_a AS (a int[], t text[]);
SELECT cbor_populate_record(null::rec_a, '{"a": [[1, 2], [3, 4]], "t": [["x"]]}'), cbor_populate_record(null::rec_a, '{"a": [[]]}'), cbor_populate_record(null::rec_a, '{"a": [[null], [1]]}'), cbor_populate_record(null::rec_a, '{"a": "{1,2}"}');
SELECT cbor_populate_record(null::rec_a, '{"a": [[1, 2], [3]]}');
SELECT cbor_populate_record(null::rec_a, '{"a": [[1], 2]}');
SELECT cbor_populate_record(null::rec_a, '{"a": [1, [2]]}');
SELECT cbor_populate_record(null::rec_a, '{"a": 5}');
SELECT cbor_populate_record(null::rec_a, '{"a": [[[[[[[1]]]]]]]}');
DROP TYPE rec_a;
DROP TYPE rec_d;
DROP DOMAIN rec_pos;
DROP DOMAIN rec_nn;
