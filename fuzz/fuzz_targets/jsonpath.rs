#![no_main]
//! Arbitrary bytes as a compiled jsonpath: refused cleanly, or decoded into a path that prints and evaluates over
//! sample documents without panicking.
use libfuzzer_sys::fuzz_target;
use postbor_core::ctx::PlainCtx;
use postbor_core::datum::Datum;
use postbor_core::jsonpath::eval::{PlainHost, Seq, execute, write_array};
use postbor_core::jsonpath::{Path, print};
use postbor_core::value::Value;

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
    }
});

fn doc(text: &str) -> Vec<u8> {
    let mut d = Vec::new();
    postbor_core::edn::to_datum(text.as_bytes(), &mut d, &mut PlainCtx::default()).unwrap();
    d
}
