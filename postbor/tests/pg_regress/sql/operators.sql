-- Untyped literals resolve to the text overloads.
SELECT '{"a": {"b": [10, 20]}}'::cbor -> 'a', '{"a": 1}'::cbor ->> 'a', '{"a": "x"}'::cbor ->> 'a';
SELECT '{"a": {"b": [10, 20]}}'::cbor #> '{a,b,1}', '{"a": {"b": [10, 20]}}'::cbor #>> '{a,b,-1}';
SELECT '{"a": null}'::cbor ->> 'a' IS NULL AS null_is_sql_null, '{"a": h''ff''}'::cbor ->> 'a' AS bytes_as_edn;
-- -> int: array index, or integer key on maps (COSE/CWT style).
SELECT '[1, 2, 3]'::cbor -> 0, '[1, 2, 3]'::cbor -> -1, '[1, 2, 3]'::cbor -> 5, '{1: "iss", -1: "x"}'::cbor -> 1, '{1: "iss", -1: "x"}'::cbor ->> -1;
SELECT '{h''ff'': 1}'::cbor -> '\xff'::bytea;
-- Typed keys and paths.
SELECT cbor_get('{1: {"a": [true]}}', '1'), cbor_get_path('{1: {"a": [true]}}', '[1, "a", 0]'), cbor_get_path_text('{1: {"a": ["s"]}}', '[1, "a", 0]');
SELECT cbor_extract_path('{"a": {"b": 1}}', 'a', 'b'), cbor_extract_path_text('{"a": {"b": "x"}}', 'a', 'b');
-- Containment and existence.
SELECT '{"a": 1, "b": [1, 2, {"c": 3}]}'::cbor @> '{"b": [{"c": 3}]}', '[1, 2]'::cbor @> '1', '1'::cbor @> '1.0', '{"a": 1}'::cbor <@ '{"a": 1, "b": 2}';
SELECT '{"a": 1}'::cbor ? 'a', '["a", "b"]'::cbor ? 'b', '{"a": 1}'::cbor ?| '{x,a}', '{"a": 1}'::cbor ?& '{x,a}';
SELECT cbor_has('{1: 2}', '1'), cbor_has_any('{1: 2}', '[3, 1]'), cbor_has_all('{1: 2}', '[3, 1]');
-- Building new values.
SELECT '{"a": 1, "c": 3}'::cbor || '{"b": 2, "a": 9}', '[1]'::cbor || '2', '1'::cbor || '[2]';
SELECT '{"a": 1, "b": 2}'::cbor - 'a', '["a", "b", "a"]'::cbor - 'a', '{"a": 1, "b": 2}'::cbor - '{a,b}'::text[], '[1, 2, 3]'::cbor - 1, '{1: 2, 3: 4}'::cbor - 3;
SELECT '1'::cbor - 'a';
SELECT cbor_delete('{1: 2, "1": 3}', '1'::cbor), '{"a": [1, {"b": 2}]}'::cbor #- '{a,1,b}', cbor_delete_path('{1: [1, 2]}', '[1, 0]');
SELECT cbor_set('{"a": [1, 2]}', '{a,0}', '"x"'), cbor_set('{"a": [1, 2]}', '{a,9}', '"x"'), cbor_set('{"a": 1}', '{b}', '2', false);
SELECT cbor_set_path('{1: {}}', '[1, h''ff'']', 'true');
SELECT cbor_insert('[1, 2]', '{1}', '"x"'), cbor_insert('[1, 2]', '{1}', '"x"', true);
SELECT cbor_insert('{"a": 1}', '{a}', '2');
SELECT cbor_set('{"a": [1]}', '{a,x}', '2');
SELECT cbor_set_lax('{"a": 1}', '{a}', NULL), cbor_set_lax('{"a": 1}', '{a}', NULL, true, 'delete_key'), cbor_set_lax('{"a": 1}', '{a}', NULL, true, 'return_target');
SELECT cbor_set_lax('{"a": 1}', '{a}', NULL, true, 'raise_exception');
SELECT cbor_strip_nulls('{"a": null, "b": [null, {"c": null}], "u": undefined}'), cbor_strip_nulls('{"a": null, "b": [null]}', true), cbor_strip_nulls('{"u": undefined}', false, true);
-- Ordering: numbers by value across integer and float, integer first on ties; text without collation.
SELECT c FROM unnest(ARRAY['"b"', '"aa"', '1.5', '1', '1.0', '-1', '2(h''010000000000000000'')', '[]', '{}', 'null', 'true', 'h''00''']::cbor[]) AS c ORDER BY c;
SELECT '1'::cbor = '1.0', '1'::cbor < '1.0', '{"a": 1, "b": 2}'::cbor = '{"b": 2, "a": 1}';
-- Hash and btree indexes, hash joins.
CREATE TABLE ops_t AS SELECT ('{"id": ' || i || '}')::cbor AS c FROM generate_series(1, 200) i;
CREATE INDEX ON ops_t USING btree (c);
CREATE INDEX ops_t_h ON ops_t USING hash (c);
SET enable_seqscan = off;
SELECT count(*) FROM ops_t WHERE c = '{"id": 42}';
SELECT count(*) FROM ops_t a JOIN ops_t b ON a.c = b.c;
RESET enable_seqscan;
SELECT count(DISTINCT c) FROM ops_t;
DROP TABLE ops_t;
