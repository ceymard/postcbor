-- cbor_populate_record_valid (PostgreSQL 17+, as jsonb_populate_record_valid): whether cbor_populate_record would
-- succeed. The alternative expected output is PostgreSQL 15 and 16, which lack soft domain checks.
CREATE DOMAIN rv_pos AS int[] CHECK (VALUE[1] > 0);
CREATE DOMAIN rv_nn AS int NOT NULL;
CREATE TYPE rv AS (a rv_pos, b rv_nn, c int, d date, u uuid);
SELECT cbor_populate_record_valid(null::rv, '{"a": [2], "b": 1, "c": 3}') AS ok,
       cbor_populate_record_valid(null::rv, '{"b": 1, "c": "x"}') AS bad_input,
       cbor_populate_record_valid(null::rv, '{"a": [-1], "b": 1}') AS bad_check,
       cbor_populate_record_valid(null::rv, '{"c": 1}') AS null_domain,
       cbor_populate_record_valid(null::rv, '[1]') AS not_map,
       cbor_populate_record_valid(null::rv, '{"b": 1, "a": [[1], [2, 3]]}') AS ragged,
       cbor_populate_record_valid(null::rv, '{"b": 1, "d": 100("x")}') AS bad_tag,
       cbor_populate_record_valid(null::rv, '{"b": 1, "u": 37(h''00'')}') AS bad_uuid,
       cbor_populate_record_valid(null::rv, '{"b": 1, "c": "1\u0000"}') AS nul;
-- With a base row, absent columns are not checked; NULL for both arguments gives NULL.
SELECT cbor_populate_record_valid(row('{1}', 2, 3, null, null)::rv, '{"c": 4}') AS base_ok,
       cbor_populate_record_valid(row('{1}', 2, 3, null, null)::rv, '{"c": "x"}') AS base_bad,
       cbor_populate_record_valid(null::rv, null) IS NULL AS both_null;
DROP TYPE rv;
DROP DOMAIN rv_pos;
DROP DOMAIN rv_nn;
