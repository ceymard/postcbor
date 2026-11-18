-- Rough comparison with jsonb: small documents and large maps. Run with a release build:
--   psql -X -f bench/lookup.sql
\timing on
DROP TABLE IF EXISTS b_small_j, b_small_c, b_big_j, b_big_c;
CREATE TABLE b_small_j AS SELECT jsonb_build_object('id', i, 'name', 'user ' || i, 'tags', jsonb_build_array('a', 'b', i % 7), 'active', i % 2 = 0) AS d FROM generate_series(1, 200000) i;
CREATE TABLE b_small_c AS SELECT d::cbor AS d FROM b_small_j;
CREATE TABLE b_big_j AS SELECT (SELECT jsonb_object_agg('k' || lpad(k::text, 4, '0'), k * r) FROM generate_series(1, 1000) k) AS d FROM generate_series(1, 2000) r;
CREATE TABLE b_big_c AS SELECT d::cbor AS d FROM b_big_j;
VACUUM ANALYZE b_small_j, b_small_c, b_big_j, b_big_c;
SELECT 'small jsonb' t, pg_size_pretty(pg_total_relation_size('b_small_j')) UNION ALL SELECT 'small cbor', pg_size_pretty(pg_total_relation_size('b_small_c'))
UNION ALL SELECT 'big jsonb', pg_size_pretty(pg_total_relation_size('b_big_j')) UNION ALL SELECT 'big cbor', pg_size_pretty(pg_total_relation_size('b_big_c'));
\echo small: field access
SELECT count(*) FROM b_small_j WHERE d ->> 'name' = 'user 777';
SELECT count(*) FROM b_small_c WHERE d ->> 'name' = 'user 777';
\echo small: containment
SELECT count(*) FROM b_small_j WHERE d @> '{"active": true}';
SELECT count(*) FROM b_small_c WHERE d @> '{"active": true}';
\echo small: text output
SELECT sum(length(d::text)) FROM b_small_j;
SELECT sum(length(d::text)) FROM b_small_c;
\echo big: one key out of 1000
SELECT sum((d -> 'k0500')::int) FROM b_big_j;
SELECT sum((d -> 'k0500')::int) FROM b_big_c;
\echo big: last key
SELECT sum((d -> 'k1000')::int) FROM b_big_j;
SELECT sum((d -> 'k1000')::int) FROM b_big_c;
\echo equality and sort
SELECT count(DISTINCT d) FROM b_small_j;
SELECT count(DISTINCT d) FROM b_small_c;
