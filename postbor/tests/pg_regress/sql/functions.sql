-- Inspection.
SELECT cbor_typeof(c) FROM unnest(ARRAY['1', '-1', '1.5', 'h''00''', '"t"', '[]', '{}', '1(0)', 'simple(16)', 'true', 'null', 'undefined']::cbor[]) c;
SELECT cbor_array_length('[1, 2, 3]'), cbor_byte_length('[1, 2, 3]');
SELECT cbor_array_length('{}');
SELECT cbor_pretty('{"a": [1, {"b": h''ff''}], "c": {}}');
SELECT cbor_diag('{"a": 1}'), cbor_diag('[1]', true);
-- Tags and simple values.
SELECT cbor_tag('1(0)'), cbor_tag('0') IS NULL, cbor_untag('37(h''00'')'), cbor_untag('5'), cbor_tagged(42, '[1]'), cbor_tagged(2, 'h''01''');
SELECT cbor_simple(16), cbor_simple(255), cbor_undefined();
SELECT cbor_simple(24);
-- Set-returning functions.
SELECT * FROM cbor_each('{"a": 1, 2: [true], h''ff'': null}');
SELECT * FROM cbor_each_text('{"a": "x", 2: [true], "n": null}');
SELECT cbor_object_keys('{"a": 1, 2: 3}');
SELECT cbor_array_elements('[1, "two", [3]]');
SELECT cbor_array_elements_text('[1, "two", null]');
SELECT cbor_array_elements('{}');
-- Builders.
SELECT cbor_build_array(1, 'two', 3.5::float8, NULL, true, '\x01'::bytea), cbor_build_array();
SELECT cbor_build_object('a', 1, 2, 'two', 'a', 3), cbor_build_object();
SELECT cbor_build_object('a');
SELECT cbor_object('{a,1,b,2}'), cbor_object('{a,b}', '{1,NULL}');
-- to_cbor and casts.
SELECT to_cbor(1), to_cbor(-5::int2), to_cbor(1.5::float4), to_cbor(1.50::numeric), to_cbor(12345678901234567890123::numeric), to_cbor('NaN'::numeric);
SELECT to_cbor('text'::text), to_cbor('\xdead'::bytea), to_cbor('a3bb189e-8bf9-3888-9912-ace4e6543002'::uuid), to_cbor(true);
SELECT to_cbor('2024-01-01 00:00:00.5+00'::timestamptz), to_cbor('2024-01-01'::date), to_cbor(ARRAY[[1,2],[3,4]]), to_cbor(ARRAY[1,NULL]);
SELECT to_cbor(ROW(1, 'x', ARRAY[true])), to_cbor('{"a": [1, 2.5, "s", null], "a": 3}'::json), to_cbor('{"x": 18446744073709551616}'::jsonb);
SELECT '1'::cbor::int4, '-1'::cbor::int8, '1.5'::cbor::float8, '1'::cbor::numeric, '4([-2, 27315])'::cbor::numeric, '2(h''010000000000000000'')'::cbor::numeric, 'true'::cbor::bool;
-- Bigfloats are exact decimals: m × 2^-n has n places, up to numeric's 16383.
SELECT '5([-1, 3])'::cbor::numeric, '5([-3, -3])'::cbor::numeric, '5([3, 3])'::cbor::numeric;
SELECT '5([-16384, 1])'::cbor::numeric;
SELECT '40000'::cbor::int2;
SELECT '"x"'::cbor::int4;
SELECT '1(1704067200.5)'::cbor::timestamptz AT TIME ZONE 'UTC', '0("2024-01-01T00:00:00Z")'::cbor::timestamptz AT TIME ZONE 'UTC', '100(19723)'::cbor::date, '37(h''a3bb189e8bf938889912ace4e6543002'')'::cbor::uuid;
SELECT '[1, h''fbff'', NaN, {1: undefined}, 1(5)]'::cbor::jsonb, '{"a": [1]}'::cbor::json;
SELECT cbor_to_jsonb_strict('{"a": [1, "x"]}');
SELECT cbor_to_jsonb_strict('[h''00'']');
SELECT '{"b": 1, "a": [1.5, null, "x"]}'::jsonb::cbor, '{"a": 1, "a": 2}'::json::cbor;
SELECT '\x83010203'::bytea::cbor, '[1, 2]'::cbor::bytea, cbor_from_bytea('\xa16161f5'), '\x9f01ff'::bytea::cbor;
SELECT '\x8301'::bytea::cbor;
-- Aggregates.
SELECT cbor_agg(x), cbor_agg_strict(y) FROM (VALUES (1, NULL::int), (2, 5), (3, NULL)) v(x, y);
SELECT cbor_object_agg(k, v), cbor_object_agg_strict(k, n) FROM (VALUES ('a', 1, NULL::int), ('b', 2, 3), ('a', 9, 4)) v(k, v, n);
SELECT cbor_object_agg_unique(k, v) FROM (VALUES ('a', 1), ('a', 2)) v(k, v);
SELECT cbor_agg(x) FROM (SELECT 1 WHERE false) v(x);
SELECT x, cbor_agg(x) OVER (ORDER BY x) FROM generate_series(1, 3) x;
SELECT cbor_object_agg(i, i * i) FROM generate_series(1, 20) i;
-- Parallel aggregation combines partial states.
CREATE TABLE agg_t AS SELECT i FROM generate_series(1, 5000) i;
SELECT set_config(CASE WHEN current_setting('server_version_num')::int >= 160000 THEN 'debug_parallel_query' ELSE 'force_parallel_mode' END, 'on', false) IS NOT NULL AS parallel_forced;
SET parallel_setup_cost = 0; SET parallel_tuple_cost = 0; SET min_parallel_table_scan_size = 0; SET max_parallel_workers_per_gather = 2;
SELECT cbor_array_length(cbor_agg(i)) AS n, (SELECT count(*) FROM cbor_each(cbor_object_agg(i, i))) AS keys FROM agg_t;
RESET ALL;
DROP TABLE agg_t;
-- Edits cannot produce values deeper than input accepts.
SET postbor.max_depth = 2;
SELECT '[1(1)]'::cbor || '[2]' AS within_limit;
SELECT '1(1(1))'::cbor || '2' AS one_level_too_deep;
CREATE TABLE deep_t (c cbor);
INSERT INTO deep_t VALUES ('{"a": {}}');
UPDATE deep_t SET c['a']['b'] = '[1]';
DROP TABLE deep_t;
RESET postbor.max_depth;
