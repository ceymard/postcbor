#![no_main]
//! Every editing operation on a stored value yields a canonical stored value (or a clean error).
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use postcbor_core::datum::Datum;
use postcbor_core::edit::{self, SetMode, Step};
use postcbor_core::ops;
use postcbor_core::value::Value;

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
    let (Some(a), Some(b)) = (postcbor_fuzz::store(&input.0), postcbor_fuzz::store(&input.1)) else { return };
    let (da, db) = (Datum::parse(&a).unwrap(), Datum::parse(&b).unwrap());
    let (va, vb) = (Value::root(&da), Value::root(&db));
    let mut out = Vec::new();
    let r = match &input.2 {
        Op::Concat => edit::concat(&va, &vb, &mut out, &postcbor_fuzz::ctx().0),
        Op::DeleteKey(k) => edit::delete_matching(&va, &[ops::key_text(k).to_vec()], &mut out, &postcbor_fuzz::ctx().0),
        Op::DeleteIndex(i) => edit::delete_index(&va, *i, &mut out, &postcbor_fuzz::ctx().0),
        Op::DeletePath(p) => edit::delete_path(&va, &steps(p), &mut out, &postcbor_fuzz::ctx().0),
        Op::Set(p, create) => edit::set_path(&va, &steps(p), vb.d.body, SetMode::Set { create: *create }, &mut out, &postcbor_fuzz::ctx().0),
        Op::Insert(p, after) => edit::set_path(&va, &steps(p), vb.d.body, SetMode::Insert { after: *after }, &mut out, &postcbor_fuzz::ctx().0),
        Op::StripNulls(arr, undef) => edit::strip_nulls(&va, *arr, *undef, &mut out, &postcbor_fuzz::ctx().0),
        Op::Contains => {
            postcbor_fuzz::check_contains(&va, &vb);
            return;
        }
        Op::Pack(ietf) => {
            use postcbor_core::pack::{PackFormat, pack};
            let mut packed = Vec::new();
            pack(&va, if *ietf { PackFormat::Ietf } else { PackFormat::CborX }, &mut packed).unwrap();
            let mut back = Vec::new();
            match postcbor_core::datum::from_cbor(&packed, &mut back, &mut postcbor_fuzz::ctx()) {
                // Packing can shrink a value past the expansion ratio allowed when reading it back.
                Err(e) if e.kind == postcbor_core::ErrorKind::ExpansionLimit => {}
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
                postcbor_fuzz::check_stored(&sub);
            }
            return;
        }
    };
    if r.is_ok() {
        postcbor_fuzz::check_stored(&out);
    }
});
