//! Byte-level CBOR operations for postbor: normalization, the datum format, navigation, ordering and EDN.
//! Every function that reads untrusted bytes goes through [`cursor::Cursor`].
#![forbid(unsafe_code)]

pub mod cmp;
pub mod bigint;
pub mod ctx;
pub mod cursor;
pub mod datum;
pub mod edit;
pub mod edn;
pub mod encode;
pub mod error;
pub mod float;
pub mod gin;
pub mod index;
pub mod json;
pub mod normalize;
pub mod num;
pub mod ops;
pub mod out;
pub mod pack;
pub mod value;

#[cfg(test)]
pub(crate) mod test_util;

pub use ctx::{Ctx, Limits};
pub use error::{Error, ErrorKind, Result};
pub use out::OutBuf;
