//! Path queries over `cbor` with PostgreSQL's `jsonpath` (design §8 Path queries).

use pgrx::prelude::*;
use postbor_core::jsonpath::Path;

use crate::datum::JsonPathArg;
use crate::error::{OrRaise, Source};

/// Debug builds only: the decoded path printed back as text, so tests can check that decoding loses nothing.
#[cfg(debug_assertions)]
#[pg_extern(immutable, strict, parallel_safe)]
fn postbor_jsonpath_print(path: JsonPathArg<'_>) -> String {
    let p = Path::decode(path.0).or_raise(Source::Stored);
    postbor_core::jsonpath::print::print(&p)
}
