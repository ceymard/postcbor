use core::fmt;

/// What went wrong. The glue maps each kind to a SQLSTATE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// Input ends in the middle of an item.
    Truncated,
    /// Bytes remain after the single top-level item.
    TrailingBytes,
    /// Additional information 28–30, or 31 where indefinite length is not allowed.
    Reserved,
    /// A break code outside an indefinite-length item.
    UnexpectedBreak,
    /// An indefinite-length string chunk that is not a definite string of the same type.
    InvalidChunk,
    InvalidUtf8,
    DuplicateKey,
    /// A map ended between a key and its value.
    MissingValue,
    /// Tag 2/3 content that is not a byte string.
    InvalidBignum,
    /// Simple value below 32 in the two-byte form.
    InvalidSimple,
    /// A declared length or count larger than the remaining input can hold.
    LengthTooLarge,
    DepthLimit,
    ExpansionLimit,
    OutOfMemory,
    /// Result larger than PostgreSQL's 1 GB value limit.
    TooLarge,
    /// A stored datum that does not follow the datum format.
    CorruptDatum,
    /// A stored datum written by a newer format version.
    UnsupportedVersion,
    /// An argument the operation cannot use (path step, scalar target, …); maps to invalid_parameter_value.
    InvalidArgument(&'static str),
    /// Internal signal: the input uses packing and must be unpacked first. Never reaches callers.
    Packed,
    /// Invalid EDN text; the message names the problem.
    Syntax(&'static str),
}

/// An error with the byte offset where it was detected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub offset: usize,
}

/// Offset of errors whose position has no meaning for the caller (e.g. text input found invalid after parsing).
pub const NO_OFFSET: usize = usize::MAX;

impl Error {
    pub fn new(kind: ErrorKind, offset: usize) -> Self {
        Error { kind, offset }
    }
}

pub type Result<T> = core::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self.kind {
            ErrorKind::Truncated => "unexpected end of input",
            ErrorKind::TrailingBytes => "unexpected bytes after the item",
            ErrorKind::Reserved => "reserved or invalid additional information",
            ErrorKind::UnexpectedBreak => "unexpected break code",
            ErrorKind::InvalidChunk => "invalid chunk in indefinite-length string",
            ErrorKind::InvalidUtf8 => "invalid UTF-8 in text string",
            ErrorKind::DuplicateKey => "duplicate map key",
            ErrorKind::MissingValue => "map key without a value",
            ErrorKind::InvalidBignum => "bignum content is not a byte string",
            ErrorKind::InvalidSimple => "invalid simple value",
            ErrorKind::LengthTooLarge => "declared length exceeds the input",
            ErrorKind::DepthLimit => "nesting depth exceeds postcbor.max_depth",
            ErrorKind::ExpansionLimit => "value expands beyond the configured limits",
            ErrorKind::OutOfMemory => "out of memory",
            ErrorKind::TooLarge => "value exceeds the 1 GB size limit",
            ErrorKind::CorruptDatum => "corrupt cbor datum",
            ErrorKind::UnsupportedVersion => "cbor datum written by a newer postcbor version",
            ErrorKind::Packed => "packed input",
            ErrorKind::Syntax(m) | ErrorKind::InvalidArgument(m) => m,
        };
        if self.offset == NO_OFFSET {
            return f.write_str(msg);
        }
        write!(f, "{msg} at byte {}", self.offset)
    }
}
