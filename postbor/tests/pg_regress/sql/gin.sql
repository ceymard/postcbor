-- GIN indexes: every query must find the same rows through the index as by a sequential scan.
CREATE TABLE gt (d cbor);
INSERT INTO gt SELECT format('{"a": %s, "tags": [%s, "t%s"], "nested": {"x": %s}}', i, i % 5, i % 3, i % 7)::cbor FROM generate_series(1, 200) i;
INSERT INTO gt SELECT format('{1: %s, h''ff'': "b", "s": 258([%s, 7]), "d": 4([-2, %s])}', i % 4, i % 3, i % 5)::cbor FROM generate_series(1, 100) i;
INSERT INTO gt SELECT format('[%s, "a", [1, 2], {"k": %s}]', i % 4, i % 2)::cbor FROM generate_series(1, 50) i;
INSERT INTO gt SELECT format('%s', i % 3)::cbor FROM generate_series(1, 30) i;
INSERT INTO gt SELECT format('{"%s": %s, [1, 2]: "ck"}', repeat('k', 200), i % 2)::cbor FROM generate_series(1, 20) i;
INSERT INTO gt VALUES ('{}'), ('[]'), ('"a"'), ('{"a": {}}');
-- Numbers in other forms and tagged scalars, for path queries.
INSERT INTO gt SELECT format('{"p": %s, "q": 42(%s), "r": [%s.0, "s%s"], "t": 32("u%s")}', i % 4, i % 3, i % 5, i % 2, i % 3)::cbor FROM generate_series(1, 60) i;
INSERT INTO gt VALUES ('{"p": 1.5}'), ('{"p": 4([-1, 15])}'), ('{"p": 5([-1, 3])}'), ('{"p": 4([-2, 150])}'), ('{"p": 0("2013-03-21T20:04:00Z")}'), ('{"p": 0("x")}');
ANALYZE gt;

-- (rows by sequential scan, rows through the index, whether the plan used the index)
CREATE FUNCTION idx_check(q text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE seq bigint; idx bigint; plan text; used bool := false;
BEGIN
    PERFORM set_config('enable_seqscan', 'on', true);
    PERFORM set_config('enable_bitmapscan', 'off', true);
    EXECUTE 'SELECT count(*) FROM gt WHERE ' || q INTO seq;
    PERFORM set_config('enable_seqscan', 'off', true);
    PERFORM set_config('enable_bitmapscan', 'on', true);
    EXECUTE 'SELECT count(*) FROM gt WHERE ' || q INTO idx;
    FOR plan IN EXECUTE 'EXPLAIN (COSTS OFF) SELECT count(*) FROM gt WHERE ' || q LOOP
        used := used OR plan LIKE '%Bitmap Index Scan%';
    END LOOP;
    RETURN format('%s %s %s', seq, idx, CASE WHEN used THEN 'index' ELSE 'no index' END);
END $$;

CREATE INDEX gt_ops ON gt USING gin (d);
SELECT q, idx_check(q) FROM (VALUES
    ($$d @> '{"a": 7}'$$),
    ($$d @> '{"tags": ["t1"]}'$$),
    ($$d @> '{"tags": [2, "t2"]}'$$),
    ($$d @> '{"nested": {"x": 3}}'$$),
    ($$d @> '{"a": 7, "nested": {"x": 0}}'$$),
    ($$d @> '{}'$$),
    ($$d @> '[]'$$),
    ($$d @> '{"a": {}}'$$),
    ($$d @> '{1: 2}'$$),
    ($$d @> '{h''ff'': "b"}'$$),
    ($$d @> '{"s": 258([7])}'$$),
    ($$d @> '{"d": 4([-2, 3])}'$$),
    ($$d @> '{"d": 4([3, -2])}'$$),
    ($$d @> '[[2]]'$$),
    ($$d @> '[{"k": 1}]'$$),
    ($$d @> '1'$$),
    ($$d @> '"a"'$$),
    ($$d @> ('{"' || repeat('k', 200) || '": 1}')::cbor$$),
    ($$d ? 'a'$$),
    ($$d ? 'tags'$$),
    ($$d ? 'x'$$),
    ($$d ?| '{x,a}'$$),
    ($$d ?| '{}'$$),
    ($$d ?& '{a,tags}'$$),
    ($$d ?& '{}'$$),
    ($$d ?# '1'$$),
    ($$d ?# 'h''ff'''$$),
    ($$d ?# '[1, 2]'$$),
    ($$d ?# '"a"'$$),
    ($$d ?#| '[1, "nested"]'$$),
    ($$d ?#| '[[1, 2], "nested"]'$$),
    ($$d ?#| '[]'$$),
    ($$d ?#& '[1, h''ff'']'$$),
    ($$d ?#& '[[1, 2], "ck"]'$$),
    ($$d ?#& '[]'$$),
    -- Path queries: `path == literal` conditions use the index; everything else checks every row.
    ($$d @@ '$.a == 7'$$),
    ($$d @@ '$.p == 1'$$),
    ($$d @@ '$.p == 1.5'$$),
    ($$d @@ '$.p == "x"'$$),
    ($$d @@ 'lax $.r == 2'$$),
    ($$d @@ 'strict $.r == 2'$$),
    ($$d @@ 'strict $.r[*] == 2'$$),
    ($$d @@ '$.q == 1'$$),
    ($$d @@ '$.t == "u1"'$$),
    ($$d @@ '$.d == 0.03'$$),
    ($$d @@ '$ == 1'$$),
    ($$d @@ '$.nested.x == 3 && $.a == 10'$$),
    ($$d @@ '$.nested.x == 3 || $.a == 10'$$),
    ($$d @@ '$.nested.x == 3 || $.a > 10'$$),
    ($$d @@ '!($.a == 7)'$$),
    ($$d @@ '$.a > 7'$$),
    ($$d @@ '$.t starts with "u"'$$),
    ($$d @? '$.a ? (@ == 7)'$$),
    ($$d @? '$ ? (@.p == 1.5)'$$),
    ($$d @? '$.s[*] ? (@ == 7)'$$),
    ($$d @? '$.tags[*] ? (@ == "t1")'$$),
    ($$d @? '$.** ? (@ == "s1")'$$),
    ($$d @? 'strict $.** ? (@ == 2)'$$),
    ($$d @? '$.a'$$),
    ($$d @? '$.nested ? (@.x == 3).x'$$)
) t(q);
DROP INDEX gt_ops;

CREATE INDEX gt_path ON gt USING gin (d cbor_path_ops);
SELECT q, idx_check(q) FROM (VALUES
    ($$d @> '{"a": 7}'$$),
    ($$d @> '{"tags": ["t1"]}'$$),
    ($$d @> '{"nested": {"x": 3}}'$$),
    ($$d @> '{"x": 3}'$$),
    ($$d @> '{}'$$),
    ($$d @> '{"a": {}}'$$),
    ($$d @> '{1: 2}'$$),
    ($$d @> '{"s": 258([7])}'$$),
    ($$d @> '{"s": [7]}'$$),
    ($$d @> '{"d": 4([-2, 3])}'$$),
    ($$d @> '[[2]]'$$),
    ($$d @> '[{"k": 1}]'$$),
    ($$d @> '1'$$)
) t(q);
-- Only containment can use this class.
SELECT idx_check($$d ? 'a'$$), idx_check($$d @@ '$.a == 7'$$);
DROP TABLE gt;
DROP FUNCTION idx_check;
