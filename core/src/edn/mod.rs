//! EDN, the text form of `cbor` values (design §5).

pub mod literals;
pub mod parse;
pub mod print;

pub use print::{PrintOpts, print};

use crate::ctx::Ctx;
use crate::datum;
use crate::error::Result;
use crate::normalize::Duplicates;
use crate::out::OutBuf;

/// Parses EDN text and appends the normalized datum to `out`.
pub fn to_datum(text: &[u8], out: &mut impl OutBuf, ctx: &mut impl Ctx) -> Result<()> {
    to_datum_with(text, out, ctx, Duplicates::Reject)
}

/// [`to_datum`] with a duplicate-key policy (JSON input keeps the last duplicate, as jsonb does).
pub fn to_datum_with(text: &[u8], out: &mut impl OutBuf, ctx: &mut impl Ctx, dups: Duplicates) -> Result<()> {
    let mut raw = Vec::new();
    parse::parse(text, &mut raw, ctx)?;
    // Offsets into the intermediate CBOR mean nothing to the author of the text.
    datum::from_cbor_with(&raw, out, ctx, dups).map_err(|e| crate::Error::new(e.kind, crate::error::NO_OFFSET))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::{Limits, PlainCtx};
    use crate::datum::Datum;
    use crate::error::ErrorKind;
    use crate::test_util::hex;
    use crate::value::Value;

    fn body(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        to_datum(text.as_bytes(), &mut out, &mut PlainCtx::default()).unwrap();
        Datum::parse(&out).unwrap().body.to_vec()
    }

    fn err(text: &str) -> ErrorKind {
        let mut out = Vec::new();
        to_datum(text.as_bytes(), &mut out, &mut PlainCtx::default()).unwrap_err().kind
    }

    fn show(text: &str, pretty: bool) -> String {
        let mut d = Vec::new();
        to_datum(text.as_bytes(), &mut d, &mut PlainCtx::default()).unwrap();
        let mut out = Vec::new();
        print(&Value::root(&Datum::parse(&d).unwrap()), &mut out, PrintOpts { pretty, ascii_only: false }).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn core_notation_parses() {
        assert_eq!(body("[1, -1, 1.5, \"a\", h'ff', true, null, undefined, simple(16)]"),
            hex("89 01 20 f93e00 6161 41ff f5 f6 f7 f0"));
        assert_eq!(body("{\"b\": 1, 2: [], 1(0): {}}"), hex("a3 02 80 6162 01 c100 a0"));
        assert_eq!(body("18446744073709551616"), hex("c2 49 010000000000000000"));
        assert_eq!(body("-18446744073709551617"), hex("c3 49 010000000000000000"));
        assert_eq!(body("-18446744073709551616"), hex("3b ffffffffffffffff"));
        assert_eq!(body("0x10"), hex("10"));
        assert_eq!(body("-0"), hex("00"));
        assert_eq!(body("-0.0"), hex("f98000"));
        assert_eq!(body("1e300"), hex("fb 7e37e43c8800759c"));
        assert_eq!(body("[NaN, Infinity, -Infinity]"), hex("83 f97e00 f97c00 f9fc00"));
        assert_eq!(body("'hi'"), hex("42 6869"));
        assert_eq!(body("\"\\ud83d\\ude00\""), hex("64 f09f9880"));
        assert_eq!(body("b64'SGk='"), hex("42 4869"));
        assert_eq!(body("b64'_w'"), hex("41 ff"));
    }

    #[test]
    fn edn_extensions_parse() {
        assert_eq!(body("# comment\n[1, / inline / 2]"), hex("82 01 02"));
        assert_eq!(body("h'01 # note\n 02 /x/ 03'"), hex("43 010203"));
        assert_eq!(body("<<1, \"a\">>"), hex("43 01 6161"));
        assert_eq!(body("<<>>"), hex("40"));
        // Packed items inside embedded CBOR are unpacked like any other input.
        assert_eq!(body("<<113([[\"ab\"], [simple(0), simple(0)]])>>"), body("<<[\"ab\", \"ab\"]>>"));
        assert_eq!(body("(_ h'01', h'02')"), hex("42 0102"));
        assert_eq!(body("[_ 1, 2]"), hex("82 01 02"));
        assert_eq!(body("1_1"), hex("01"));
        // Embedded items are stored in deterministic form.
        assert_eq!(body("<<{\"b\": 1, \"a\": 2}>>"), hex("47 a2 6161 02 6162 01"));
    }

    #[test]
    fn date_and_ip_literals_follow_the_draft() {
        assert_eq!(body("dt'1969-07-21T02:56:16Z'"), body("-14159024"));
        assert_eq!(body("dt'1969-07-21T02:56:16.0Z'"), body("-14159024.0"));
        assert_eq!(body("dt'1969-07-21T02:56:16.5Z'"), body("-14159023.5"));
        assert_eq!(body("DT'1969-07-21T02:56:16Z'"), body("1(-14159024)"));
        assert_eq!(body("dt'2024-01-01T01:00:00+01:00'"), body("1704067200"));
        assert_eq!(body("ip'192.0.2.42'"), hex("44 c000022a"));
        assert_eq!(body("IP'192.0.2.42'"), hex("d834 44 c000022a"));
        assert_eq!(body("IP'192.0.2.0/24'"), hex("d834 82 18 18 43 c00002"));
        assert_eq!(body("IP'2001:db8::42'"), hex("d836 50 20010db8000000000000000000000042"));
        assert_eq!(body("IP'2001:db8::/64'"), hex("d836 82 18 40 44 20010db8"));
        assert!(matches!(err("ip'192.0.2.1/24'"), ErrorKind::Syntax(_)));
        assert!(matches!(err("dt'2023-02-29T00:00:00Z'"), ErrorKind::Syntax(_)));
    }

    #[test]
    fn invalid_text_is_rejected() {
        for t in ["[1,]", "{1: 2,}", "{1}", "[1 2]", "1 2", "h'0'", "b32'AA'", "foo", "0x1.8p1", "\"\\x\"", "simple(24)", "", "1(", "-1(2)"] {
            assert!(matches!(err(t), ErrorKind::Syntax(_) | ErrorKind::Truncated), "{t:?}: {:?}", err(t));
        }
        assert_eq!(err("{1: 2, 1: 3}"), ErrorKind::DuplicateKey);
    }

    #[test]
    #[cfg_attr(miri, ignore = "too slow under Miri")]
    fn nesting_is_limited_without_using_the_stack() {
        let deep = "[".repeat(100_000);
        assert_eq!(err(&deep), ErrorKind::DepthLimit);
        let mut d = Vec::new();
        let mut ctx = PlainCtx(Limits { max_depth: 2, ..Limits::default() });
        assert!(to_datum(b"[[1]]", &mut d, &mut ctx).is_ok());
        assert_eq!(to_datum(b"[[[1]]]", &mut d, &mut ctx).unwrap_err().kind, ErrorKind::DepthLimit);
    }

    #[test]
    fn output_is_core_notation_and_reads_back() {
        let cases = [
            ("[1, -1, 1.5, 1.0, 100000.0, 1.1, -0.0]", "[1, -1, 1.5, 1.0, 100000.0, 1.1, -0.0]"),
            ("{\"b\": h'00ff', \"a\": [true, null, undefined, simple(16)]}", "{\"a\": [true, null, undefined, simple(16)], \"b\": h'00ff'}"),
            ("[NaN, Infinity, -Infinity, 1e300]", "[NaN, Infinity, -Infinity, 1e300]"),
            ("-18446744073709551616", "-18446744073709551616"),
            ("18446744073709551616", "2(h'010000000000000000')"),
            ("\"tab\\t \\\"q\\\" \\u0001 é\"", "\"tab\\t \\\"q\\\" \\u0001 é\""),
            ("1(<<1>>)", "1(h'01')"),
            ("[]", "[]"),
            ("{}", "{}"),
        ];
        for (input, want) in cases {
            let got = show(input, false);
            assert_eq!(got, want, "{input}");
            assert_eq!(body(&got), body(input), "reads back: {got}");
        }
    }

    #[test]
    fn pretty_output_matches_jsonb_pretty_layout() {
        assert_eq!(show("{\"a\": [1, {}], \"b\": 2(h'010000000000000000')}", true),
            "{\n    \"a\": [\n        1,\n        {}\n    ],\n    \"b\": 2(h'010000000000000000')\n}");
    }

    #[test]
    fn ascii_only_escapes_non_ascii() {
        let mut d = Vec::new();
        to_datum("\"é😀\"".as_bytes(), &mut d, &mut PlainCtx::default()).unwrap();
        let mut out = Vec::new();
        print(&Value::root(&Datum::parse(&d).unwrap()), &mut out, PrintOpts { pretty: false, ascii_only: true }).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "\"\\u00e9\\ud83d\\ude00\"");
    }
}
