#![no_main]
//! Arbitrary bytes as a compiled jsonpath: refused cleanly, or decoded into a path that prints and evaluates over
//! sample documents without panicking. Whenever `@?` or `@@` holds for a document, the GIN query must not rule out
//! that document's index entries.
use libfuzzer_sys::fuzz_target;
use postcbor_core::ctx::PlainCtx;
use postcbor_core::datum::Datum;
use postcbor_core::gin::{OpsEntry, index_entries};
use postcbor_core::jsonpath::eval::{PlainHost, Res, Seq, bool_or_null, execute, write_array};
use postcbor_core::jsonpath::gin::{Tri, query, run};
use postcbor_core::jsonpath::{Path, print};
use postcbor_core::value::Value;

const DOCS: [&str; 4] = [
    r#"{"a": [1, 2.5, "x", {"b": null}], "c": 42(h'01'), 3: [true, undefined]}"#,
    r#"[[1, [2, 4([-2, 150])]], {"a": {"a": 1}}, 18446744073709551616]"#,
    r#""2020-01-01""#,
    r#"[5([-1, 3]), 4([100000000, 1]), 0("2013-03-21T20:04:00Z"), 0("x"), 1(1.5), 1(1e300), NaN]"#,
];

fuzz_target!(|data: &[u8]| {
    // The version word first, so most inputs reach the items; the first byte picks lax or strict.
    let Some((&mode, items)) = data.split_first() else { return };
    let mut payload = (0x01u32 | if mode & 1 == 1 { 0x8000_0000 } else { 0 }).to_ne_bytes().to_vec();
    payload.extend_from_slice(items);
    let Ok(p) = Path::decode(&payload) else { return };
    print::print(&p);
    let vars = doc(r#"{"x": 1, "y": [1, 2]}"#);
    let vars = Datum::parse(&vars).unwrap();
    for text in DOCS {
        let d = doc(text);
        let d = Datum::parse(&d).unwrap();
        for throw in [false, true] {
            let mut found = Seq::default();
            if execute(&p, Value::root(&d), Some(Value::root(&vars)), throw, false, &mut PlainHost, Some(&mut found)).is_ok() {
                write_array(&found, &mut Vec::new(), 1000).unwrap();
            }
            let _ = execute(&p, Value::root(&d), None, throw, false, &mut PlainHost, None);
        }
        // The operators: no vars, silent.
        let exists = execute(&p, Value::root(&d), None, false, false, &mut PlainHost, None).ok() == Some(Res::Ok);
        let mut found = Seq::default();
        let matched = execute(&p, Value::root(&d), None, false, false, &mut PlainHost, Some(&mut found)).is_ok()
            && matches!(found.single().map(bool_or_null), Some(Ok(Some(Some(true)))));
        let mut index = Vec::new();
        index_entries(&Value::root(&d), |e| {
            let (a, b) = e.parts();
            index.push([a, b].concat());
            Ok(())
        })
        .unwrap();
        for (holds, ex) in [(exists, true), (matched, false)] {
            if let (true, Some(q)) = (holds, query(&p, ex).unwrap()) {
                let has = |i: usize| {
                    let e = OpsEntry::new(q.entries[i].0, &q.entries[i].1);
                    let (a, b) = e.parts();
                    if index.contains(&[a, b].concat()) { Tri::True } else { Tri::False }
                };
                assert_ne!(run(&q.program, has), Tri::False, "{} on {text}", print::print(&p));
            }
        }
    }
});

fn doc(text: &str) -> Vec<u8> {
    let mut d = Vec::new();
    postcbor_core::edn::to_datum(text.as_bytes(), &mut d, &mut PlainCtx::default()).unwrap();
    d
}
