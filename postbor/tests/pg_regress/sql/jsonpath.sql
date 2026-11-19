-- Path queries: CBOR-specific behaviour and limits (design §8 Path queries). jsonb parity is in jsonpath_diff.

-- Nesting: 256 items is the cap (here `$` and 255 key steps), on a document as deep.
SELECT cbor_path_query(('{"a":' || repeat('{"a":', 254) || '1' || repeat('}', 255))::cbor, ('$' || repeat('.a', 255))::jsonpath);
SELECT cbor_path_query('{}', ('$' || repeat('.a', 256))::jsonpath);
SELECT cbor_path_query('1', (repeat('(1 + ', 127) || '1' || repeat(')', 127))::jsonpath);
SELECT cbor_path_query('1', ('$' || repeat(' ? (exists(@[*]', 63) || ' ? (@ == 1)' || repeat('))', 63))::jsonpath);
-- .** walks a deep document without recursion.
SET postbor.max_depth = 10000;
SELECT count(*) FROM cbor_path_query((repeat('[', 5000) || '1' || repeat(']', 5000))::cbor, 'strict $.**');
RESET postbor.max_depth;

-- Numbers compare by value across CBOR's number forms; NaN sorts above everything.
SELECT cbor_path_query('[1, 1.0, 4([-1, 10]), 2(h''01''), 5([1, 3]), 6, NaN, Infinity]', '$[*] ? (@ == 1 || @ > 5.5)');
SELECT cbor_path_query('[18446744073709551616, -18446744073709551617]', '$[*] ? (@ > 18446744073709551615 || @ < -18446744073709551616)');
-- Arithmetic is decimal: whole results are integers, others decimal fractions with numeric's scale.
SELECT cbor_path_query('{"a": 2}', '$.a * 2.5');
SELECT cbor_path_query('{"a": 2}', '$.a * 3');
SELECT cbor_path_query('{"a": 1.5}', '$.a + 1');
SELECT cbor_path_query('1', '$ / 3');
SELECT cbor_path_query('9223372036854775807', '$ + 1');
SELECT cbor_path_query('1.0', '$ + 0');
SELECT cbor_path_query('4([-2, 150])', '$ + 0');

-- Tags other than numbers are transparent; the result keeps its tag.
SELECT cbor_path_query('42({"a": 32("http://x")})', '$.a');
SELECT cbor_path_query('42({"a": 32("http://x")})', '$.a ? (@ starts with "http")');
SELECT cbor_path_query('42({"a": 1})', '$.type()');

-- Types jsonpath lacks.
SELECT cbor_path_query('[h''01'', undefined, simple(99), null]', '$[*].type()');
SELECT cbor_path_query('[h''01'', h''02'']', '$[*] ? (@ == $b)', '{"b": h''01''}');
SELECT cbor_path_query('[undefined, null]', '$[*] ? (@ == null)');
SELECT cbor_path_query('h''01''', '$ == "AQ"');

-- Non-text keys: `.key` reaches text keys only; `.*` and `.keyvalue()` see every entry.
SELECT cbor_path_query('{1: "int", "1": "text", h''31'': "bytes"}', '$."1"');
SELECT cbor_path_query('{1: "int", "1": "text", h''31'': "bytes"}', '$.*');
SELECT cbor_path_query('{1: "int", "a": [1]}', '$.keyvalue()');

-- Errors: vars must be a map; a missing variable raises even in silent mode and in @?.
SELECT cbor_path_query('1', '$', '[1]');
SELECT cbor_path_query('1', '$x', silent => true);
SELECT cbor '1' @? '$ ? (@ == $x)';
-- Silent mode keeps the results found before an error.
SELECT cbor_path_query('[{"a": 1}, 2, {"a": 3}]', 'strict $[*].a', silent => true);
SELECT jsonb_path_query('[{"a": 1}, 2, {"a": 3}]', 'strict $[*].a', silent => true);
SELECT cbor_path_exists('[{"a": 1}, 2]', 'strict $[*].a', silent => true);
SELECT cbor_path_match('[1]', '$[*] == 1', silent => false);
SELECT cbor_path_match('[1, 2]', '$[*]', silent => false);
SELECT cbor '{"a": [1, 2]}' @@ '$.a[*] > 1', cbor '{"a": [1, 2]}' @? '$.a[*] ? (@ > 2)';
SELECT cbor_path_query_array('{"a": [1, {"b": 2}]}', '$.a[*]'), cbor_path_query_first('{"a": [1, {"b": 2}]}', '$.a[1]');
