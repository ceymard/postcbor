//! Text of a decoded path that PostgreSQL's `jsonpath_in` reads back to the same path. For tests of the decoder:
//! every binary operation is parenthesized, so the text differs from `jsonpath_out`'s but parses identically.

use super::{BinOp, DatetimeMethod, ItemId, Kind, Method, Path, UnOp};

enum Task<'p> {
    Item(ItemId, bool),
    Text(&'p str),
    Owned(String),
}

fn json_string(out: &mut String, s: &[u8]) {
    out.push('"');
    for ch in String::from_utf8_lossy(s).chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn op_text(op: BinOp) -> &'static str {
    match op {
        BinOp::And => " && ",
        BinOp::Or => " || ",
        BinOp::Eq => " == ",
        BinOp::Ne => " != ",
        BinOp::Lt => " < ",
        BinOp::Gt => " > ",
        BinOp::Le => " <= ",
        BinOp::Ge => " >= ",
        BinOp::Add => " + ",
        BinOp::Sub => " - ",
        BinOp::Mul => " * ",
        BinOp::Div => " / ",
        BinOp::Mod => " % ",
        BinOp::StartsWith => " starts with ",
    }
}

fn method_text(m: Method) -> &'static str {
    match m {
        Method::Type => ".type()",
        Method::Size => ".size()",
        Method::Abs => ".abs()",
        Method::Floor => ".floor()",
        Method::Ceiling => ".ceiling()",
        Method::Double => ".double()",
        Method::KeyValue => ".keyvalue()",
        Method::Bigint => ".bigint()",
        Method::Boolean => ".boolean()",
        Method::Date => ".date()",
        Method::Integer => ".integer()",
        Method::Number => ".number()",
        Method::String => ".string()",
    }
}

/// The path as text.
pub fn print(path: &Path) -> String {
    let mut out = String::new();
    if !path.lax {
        out.push_str("strict ");
    }
    // Tasks run last-pushed first, so each item pushes its parts in reverse order.
    let mut tasks = vec![Task::Item(path.root, false)];
    while let Some(t) = tasks.pop() {
        let (id, in_chain) = match t {
            Task::Text(s) => {
                out.push_str(s);
                continue;
            }
            Task::Owned(s) => {
                out.push_str(&s);
                continue;
            }
            Task::Item(id, in_chain) => (id, in_chain),
        };
        let item = path.item(id);
        let mut parts: Vec<Task> = Vec::new();
        // A step after an expression needs the expression parenthesized: `(1 + 2).type()`.
        let wrap = item.next.is_some() && matches!(item.kind, Kind::Numeric(_) | Kind::Binary(..) | Kind::Unary(UnOp::Plus | UnOp::Minus, _) | Kind::LikeRegex { .. });
        if wrap {
            parts.push(Task::Text("("));
        }
        let dot = if in_chain { "." } else { "" };
        match &item.kind {
            Kind::Null => parts.push(Task::Text("null")),
            Kind::String(s) => {
                let mut o = String::new();
                json_string(&mut o, s);
                parts.push(Task::Owned(o));
            }
            Kind::Numeric(d) => parts.push(Task::Owned(d.to_text())),
            Kind::Bool(b) => parts.push(Task::Text(if *b { "true" } else { "false" })),
            Kind::Binary(op, l, r) => {
                parts.extend([Task::Text("("), Task::Item(*l, false), Task::Text(op_text(*op)), Task::Item(*r, false), Task::Text(")")]);
            }
            Kind::Unary(op, a) => {
                let (open, close) = match op {
                    UnOp::Not => ("!(", ")"),
                    UnOp::IsUnknown => ("((", ") is unknown)"),
                    UnOp::Plus => ("(+(", "))"),
                    UnOp::Minus => ("(-(", "))"),
                    UnOp::Filter => ("?(", ")"),
                    UnOp::Exists => ("exists (", ")"),
                };
                parts.extend([Task::Text(open), Task::Item(*a, false), Task::Text(close)]);
            }
            Kind::AnyArray => parts.push(Task::Text("[*]")),
            Kind::AnyKey => parts.push(Task::Owned(format!("{dot}*"))),
            Kind::IndexArray(subs) => {
                parts.push(Task::Text("["));
                for (i, (from, to)) in subs.iter().enumerate() {
                    if i > 0 {
                        parts.push(Task::Text(","));
                    }
                    parts.push(Task::Item(*from, false));
                    if let Some(to) = to {
                        parts.push(Task::Text(" to "));
                        parts.push(Task::Item(*to, false));
                    }
                }
                parts.push(Task::Text("]"));
            }
            Kind::Any { first, last } => {
                let b = |n: u32| if n == u32::MAX { "last".to_string() } else { n.to_string() };
                parts.push(Task::Owned(format!("{dot}**{{{} to {}}}", b(*first), b(*last))));
            }
            Kind::Key(k) => {
                let mut o = dot.to_string();
                json_string(&mut o, k);
                parts.push(Task::Owned(o));
            }
            Kind::Current => parts.push(Task::Text("@")),
            Kind::Root => parts.push(Task::Text("$")),
            Kind::Variable(v) => {
                let mut o = "$".to_string();
                json_string(&mut o, v);
                parts.push(Task::Owned(o));
            }
            Kind::Method(m) => parts.push(Task::Text(method_text(*m))),
            Kind::Datetime(m, arg) => {
                parts.push(Task::Text(match m {
                    DatetimeMethod::Datetime => ".datetime(",
                    DatetimeMethod::Time => ".time(",
                    DatetimeMethod::TimeTz => ".time_tz(",
                    DatetimeMethod::Timestamp => ".timestamp(",
                    DatetimeMethod::TimestampTz => ".timestamp_tz(",
                }));
                if let Some(a) = arg {
                    parts.push(Task::Item(*a, false));
                }
                parts.push(Task::Text(")"));
            }
            Kind::Decimal(p, s) => {
                parts.push(Task::Text(".decimal("));
                if let Some(p) = p {
                    parts.push(Task::Item(*p, false));
                }
                if let Some(s) = s {
                    parts.push(Task::Text(","));
                    parts.push(Task::Item(*s, false));
                }
                parts.push(Task::Text(")"));
            }
            Kind::Last => parts.push(Task::Text("last")),
            Kind::LikeRegex { expr, pattern, flags } => {
                let mut o = " like_regex ".to_string();
                json_string(&mut o, pattern);
                if *flags != 0 {
                    o.push_str(" flag \"");
                    for (bit, f) in [(1, 'i'), (2, 's'), (4, 'm'), (8, 'x'), (16, 'q')] {
                        if flags & bit != 0 {
                            o.push(f);
                        }
                    }
                    o.push('"');
                }
                parts.extend([Task::Text("("), Task::Item(*expr, false), Task::Owned(o), Task::Text(")")]);
            }
        }
        if wrap {
            parts.push(Task::Text(")"));
        }
        if let Some(n) = item.next {
            parts.push(Task::Item(n, true));
        }
        tasks.extend(parts.into_iter().rev());
    }
    out
}
