#![no_main]
//! Every editing operation on a stored value yields a canonical stored value (or a clean error).
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use postbor_core::datum::Datum;
use postbor_core::edit::{self, SetMode, Step};
use postbor_core::ops;
use postbor_core::value::Value;

fn steps(p: &[String]) -> Vec<Step<'_>> {
    p.iter().map(|s| Step::Text(s)).collect()
}

#[derive(Arbitrary, Debug)]
enum Op {
    Concat,
    DeleteKey(String),
    DeleteIndex(i64),
    DeletePath(Vec<String>),
    Set(Vec<String>, bool),
    Insert(Vec<String>, bool),
    StripNulls(bool, bool),
    Contains,
    Get(Vec<String>),
    Pack(bool),
}

fuzz_target!(|input: (Vec<u8>, Vec<u8>, Op)| {
    let (Some(a), Some(b)) = (postbor_fuzz::store(&input.0), postbor_fuzz::store(&input.1)) else { return };
    let (va, vb) = (Value::root(Datum::parse(&a).unwrap()), Value::root(Datum::parse(&b).unwrap()));
    let mut out = Vec::new();
    let r = match &input.2 {
        Op::Concat => edit::concat(&va, &vb, &mut out),
        Op::DeleteKey(k) => edit::delete_matching(&va, &[ops::key_text(k)], &mut out),
        Op::DeleteIndex(i) => edit::delete_index(&va, *i, &mut out),
        Op::DeletePath(p) => edit::delete_path(&va, &steps(p), &mut out),
        Op::Set(p, create) => edit::set_path(&va, &steps(p), vb.d.body, SetMode::Set { create: *create }, &mut out),
        Op::Insert(p, after) => edit::set_path(&va, &steps(p), vb.d.body, SetMode::Insert { after: *after }, &mut out),
        Op::StripNulls(arr, undef) => edit::strip_nulls(&va, *arr, *undef, &mut out),
        Op::Contains => {
            let _ = ops::contains(&va, &vb, &mut postbor_fuzz::ctx()).unwrap();
            return;
        }
        Op::Pack(ietf) => {
            use postbor_core::pack::{PackFormat, pack};
            let mut packed = Vec::new();
            pack(&va, if *ietf { PackFormat::Ietf } else { PackFormat::CborX }, &mut packed).unwrap();
            let mut back = Vec::new();
            match postbor_core::datum::from_cbor(&packed, &mut back, &mut postbor_fuzz::ctx()) {
                // Packing can shrink a value past the expansion ratio allowed when reading it back.
                Err(e) if e.kind == postbor_core::ErrorKind::ExpansionLimit => {}
                r => {
                    r.unwrap();
                    assert_eq!(back, a, "pack round trip");
                }
            }
            return;
        }
        Op::Get(p) => {
            let refs: Vec<Option<&str>> = p.iter().map(|s| Some(s.as_str())).collect();
            if let Some(v) = ops::get_text_path(&va, &refs).unwrap() {
                let mut sub = Vec::new();
                edit::copy_value(&v, &mut sub).unwrap();
                postbor_fuzz::check_stored(&sub);
            }
            return;
        }
    };
    if r.is_ok() {
        postbor_fuzz::check_stored(&out);
    }
});
