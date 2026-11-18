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
