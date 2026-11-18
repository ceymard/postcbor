//! Core errors → PostgreSQL errors with SQLSTATEs (design §2).

use pgrx::prelude::*;
use postbor_core::{Error, ErrorKind};

/// Where the failing bytes came from; decides the SQLSTATE of malformed input.
#[derive(Clone, Copy)]
pub enum Source {
    Text,
    Binary,
    Stored,
}

/// Raises the PostgreSQL error for a core error.
pub fn raise(e: Error, src: Source) -> ! {
    use PgSqlErrorCode::*;
    let code = match e.kind {
        ErrorKind::DepthLimit | ErrorKind::ExpansionLimit | ErrorKind::TooLarge => ERRCODE_PROGRAM_LIMIT_EXCEEDED,
        ErrorKind::OutOfMemory => ERRCODE_OUT_OF_MEMORY,
        ErrorKind::CorruptDatum | ErrorKind::UnsupportedVersion => ERRCODE_DATA_CORRUPTED,
        _ => match src {
            Source::Binary => ERRCODE_INVALID_BINARY_REPRESENTATION,
            Source::Text => ERRCODE_INVALID_TEXT_REPRESENTATION,
            Source::Stored => ERRCODE_DATA_CORRUPTED,
        },
    };
    let msg = match src {
        _ if code == ERRCODE_PROGRAM_LIMIT_EXCEEDED || code == ERRCODE_OUT_OF_MEMORY => format!("cbor value rejected: {e}"),
        Source::Text => format!("invalid input syntax for type cbor: {e}"),
        Source::Binary => format!("invalid binary cbor: {e}"),
        Source::Stored => format!("{e}"),
    };
    ereport!(ERROR, code, msg);
}

/// Unwraps a core result, raising the matching PostgreSQL error.
pub trait OrRaise<T> {
    fn or_raise(self, src: Source) -> T;
}

impl<T> OrRaise<T> for postbor_core::Result<T> {
    fn or_raise(self, src: Source) -> T {
        match self {
            Ok(v) => v,
            Err(e) => raise(e, src),
        }
    }
}

/// Raises an error with an explicit SQLSTATE; usable in expression position.
pub fn fail(code: PgSqlErrorCode, msg: impl Into<String>) -> ! {
    let msg: String = msg.into();
    ereport!(ERROR, code, msg);
}
