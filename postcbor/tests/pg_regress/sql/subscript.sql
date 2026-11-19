-- Fetch: untyped literals are text keys; integers index arrays or address integer keys.
SELECT ('{"a": {"b": [10, 20]}}'::cbor)['a']['b'][1], ('{"a": 1}'::cbor)['x'], ('[1, 2]'::cbor)[-1], ('{1: "one"}'::cbor)[1], ('{h''ff'': 2}'::cbor)['\xff'::bytea], ('{[1]: 3}'::cbor)['[1]'::cbor];
SELECT ('{"a": 1}'::cbor)[NULL::text];
SELECT ('[1]'::cbor)[1:2];
-- Assignment creates missing levels and pads arrays with nulls.
CREATE TABLE sub_t (id int, c cbor);
INSERT INTO sub_t VALUES (1, '{}'), (2, '[1]'), (3, NULL), (4, '{"a": 1}');
UPDATE sub_t SET c['a']['b'] = '1' WHERE id = 1;
UPDATE sub_t SET c[3] = '"x"' WHERE id = 2;
UPDATE sub_t SET c[1]['k'] = 'true' WHERE id = 3;
UPDATE sub_t SET c['a'] = NULL WHERE id = 4;
SELECT id, c FROM sub_t ORDER BY id;
UPDATE sub_t SET c['a']['b'] = '2' WHERE id = 4;
UPDATE sub_t SET c[NULL::text] = '1' WHERE id = 1;
DROP TABLE sub_t;
