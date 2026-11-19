//! GIN support for `@?` and `@@` with `cbor_ops` (design §8): the entries a path's conditions need, as an AND/OR
//! tree, ported from PostgreSQL's `jsonb_gin.c`. Only `accessors == literal` conditions give entries, as in jsonb;
//! the literal's entry is its path-equality form ([`equality_form`]), which indexing adds next to each leaf's own
//! encoding when they differ.

use super::{BinOp, ItemId, Kind, Path, UnOp};
use crate::cursor::{MAJOR_NEGATIVE, MAJOR_SIMPLE, MAJOR_TAG, MAJOR_TEXT, MAJOR_UNSIGNED};
use crate::decimal::Num;
use crate::encode::write_head;
use crate::error::{Error, ErrorKind, NO_OFFSET, Result};
use crate::gin::Role;
use crate::num;
use crate::value::Value;

use super::eval::{self, JItem, PathFail};

/// The encoding two items share when jsonpath finds them equal: transparent tags removed, numbers in one form
/// (integers as integers, other values as decimal fractions without trailing zeros). None for items a path literal
/// cannot equal: byte strings, `undefined`, simple values, datetimes, NaN and infinities, containers.
pub fn equality_form(v: Value) -> Result<Option<Vec<u8>>> {
    let core = |e: eval::PathError| match *e.0 {
        PathFail::Core(e) => e,
        PathFail::Sql(..) => Error::new(ErrorKind::CorruptDatum, NO_OFFSET),
    };
    let v = eval::peel(v).map_err(core)?;
    let (h, _) = v.head()?;
    Ok(match h.major {
        MAJOR_TEXT => Some(v.bytes()?.to_vec()),
        MAJOR_SIMPLE if matches!(h.info, 20..=22) => Some(v.bytes()?.to_vec()),
        MAJOR_TAG if h.arg <= 1 => None,
        MAJOR_UNSIGNED | MAJOR_NEGATIVE | MAJOR_TAG | MAJOR_SIMPLE => match eval::num_of(&JItem::Doc(v)).map_err(core)? {
            Some(n) => num_form(&n)?,
            None => None,
        },
        _ => None,
    })
}

/// The equality form of a number; None for NaN and infinities.
fn num_form(n: &Num) -> Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    match n {
        Num::Int(i) if *i >= 0 => write_head(&mut out, MAJOR_UNSIGNED, *i as u64)?,
        Num::Int(i) => write_head(&mut out, MAJOR_NEGATIVE, (-1 - *i) as u64)?,
        Num::Dec(d) => {
            let mut d = (**d).clone();
            while d.scale > 0 && d.coef.last() == Some(&b'0') {
                d.coef.pop();
                d.scale -= 1;
            }
            if d.coef.is_empty() {
                d.neg = false;
                d.scale = 0;
            }
            num::write_numeric(&d.to_text(), &mut out)?;
        }
        Num::NaN | Num::Inf { .. } => return Ok(None),
    }
    Ok(Some(out))
}

/// One step of the query program, in postfix order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// The entry at this index.
    Entry(u32),
    /// All of the last `n` results.
    And(u32),
    /// Any of the last `n` results.
    Or(u32),
}

/// GIN's ternary check values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tri {
    False,
    True,
    Maybe,
}

/// Evaluates a program over the check value of each entry (`execute_jsp_gin_node`).
pub fn run(program: &[Op], check: impl Fn(usize) -> Tri) -> Tri {
    let mut stack: Vec<Tri> = Vec::with_capacity(program.len());
    for &op in program {
        let (n, and) = match op {
            Op::Entry(i) => {
                stack.push(check(i as usize));
                continue;
            }
            Op::And(n) => (n as usize, true),
            Op::Or(n) => (n as usize, false),
        };
        let args = stack.split_off(stack.len().saturating_sub(n));
        let decisive = if and { Tri::False } else { Tri::True };
        let r = if args.contains(&decisive) {
            decisive
        } else if args.contains(&Tri::Maybe) {
            Tri::Maybe
        } else if and {
            Tri::True
        } else {
            Tri::False
        };
        stack.push(r);
    }
    stack.pop().unwrap_or(Tri::Maybe)
}

/// The entries a path needs and how they combine; None when no entry can narrow the search (every row is a
/// candidate). `exists` is `@?` (the whole path must return something); otherwise `@@` (its condition holds).
pub struct Query {
    pub entries: Vec<(Role, Vec<u8>)>,
    pub program: Vec<Op>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Key,
    Any,
    AnyKey,
    AnyArray,
    IndexArray,
}

/// An accessor of the path so far, linked to the previous one.
struct PathEntry<'p> {
    parent: Option<usize>,
    step: Step,
    key: Option<&'p [u8]>,
}

enum Node {
    Entry(usize),
    And(Vec<usize>),
    Or(Vec<usize>),
}

enum Task {
    /// `extract_jsp_bool_expr`.
    Bool { id: ItemId, not: bool, path: Option<usize> },
    /// Combines the two results on top of the result stack.
    Combine { is_and: bool },
    /// `extract_jsp_path_expr`, resumed at `at`; `filter` when a filter's result is on the result stack.
    PathExpr { at: Option<ItemId>, path: Option<usize>, scalar: Option<ItemId>, nodes: Vec<usize>, filter: bool },
}

struct Builder<'p> {
    path: &'p Path<'p>,
    entries: Vec<(Role, Vec<u8>)>,
    nodes: Vec<Node>,
    accessors: Vec<PathEntry<'p>>,
}

impl<'p> Builder<'p> {
    fn node(&mut self, n: Node) -> usize {
        self.nodes.push(n);
        self.nodes.len() - 1
    }

    fn entry(&mut self, role: Role, bytes: Vec<u8>) -> usize {
        self.entries.push((role, bytes));
        let i = self.entries.len() - 1;
        self.node(Node::Entry(i))
    }

    /// `jsonb_ops__add_path_item`: false for accessors the index cannot follow.
    fn add_accessor(&mut self, path: &mut Option<usize>, id: ItemId) -> bool {
        let (step, key) = match &self.path.item(id).kind {
            Kind::Root => {
                *path = None;
                return true;
            }
            Kind::Key(k) => (Step::Key, Some(*k)),
            Kind::Any { .. } => (Step::Any, None),
            Kind::AnyKey => (Step::AnyKey, None),
            Kind::AnyArray => (Step::AnyArray, None),
            Kind::IndexArray(_) => (Step::IndexArray, None),
            _ => return false,
        };
        self.accessors.push(PathEntry { parent: *path, step, key });
        *path = Some(self.accessors.len() - 1);
        true
    }

    /// `jsonb_ops__extract_nodes`: for `path == literal`, the path's keys and the literal.
    fn extract_nodes(&mut self, path: Option<usize>, scalar: Option<ItemId>, nodes: &mut Vec<usize>) -> Result<()> {
        let Some(scalar) = scalar else { return Ok(()) };
        let mut p = path;
        while let Some(i) = p {
            if let (Step::Key, Some(k)) = (self.accessors[i].step, self.accessors[i].key) {
                let mut enc = Vec::new();
                write_head(&mut enc, MAJOR_TEXT, k.len() as u64)?;
                enc.extend_from_slice(k);
                let n = self.entry(Role::Key, enc);
                nodes.push(n);
            }
            p = self.accessors[i].parent;
        }
        let form = match &self.path.item(scalar).kind {
            Kind::Null => Some(vec![0xf6]),
            Kind::Bool(b) => Some(vec![if *b { 0xf5 } else { 0xf4 }]),
            Kind::String(s) => {
                let mut enc = Vec::new();
                write_head(&mut enc, MAJOR_TEXT, s.len() as u64)?;
                enc.extend_from_slice(s);
                Some(enc)
            }
            Kind::Numeric(n) => num_form(n)?,
            _ => None,
        };
        let Some(form) = form else { return Ok(()) };
        // Array elements and the top level are key entries, map values value entries. In lax mode, or after `.**`,
        // the literal may be either.
        let last = path.map(|i| self.accessors[i].step);
        let key = if self.path.lax {
            None
        } else {
            match last {
                None | Some(Step::AnyArray | Step::IndexArray) => Some(true),
                Some(Step::Any) => None,
                Some(_) => Some(false),
            }
        };
        let n = match key {
            Some(true) => self.entry(Role::Key, form),
            Some(false) => self.entry(Role::Value, form),
            None => {
                let a = self.entry(Role::Key, form.clone());
                let b = self.entry(Role::Value, form);
                self.node(Node::Or(vec![a, b]))
            }
        };
        nodes.push(n);
        Ok(())
    }

    /// The nodes of a path expression, AND-ed.
    fn combine(&mut self, nodes: Vec<usize>) -> Option<usize> {
        match nodes.len() {
            0 => None,
            1 => Some(nodes[0]),
            _ => Some(self.node(Node::And(nodes))),
        }
    }

    /// Runs the extraction without recursion: tasks and their results on explicit stacks.
    fn extract(&mut self, exists: bool) -> Result<Option<usize>> {
        let mut tasks = vec![if exists {
            Task::PathExpr { at: Some(self.path.root), path: None, scalar: None, nodes: Vec::new(), filter: false }
        } else {
            Task::Bool { id: self.path.root, not: false, path: None }
        }];
        let mut results: Vec<Option<usize>> = Vec::new();
        while let Some(task) = tasks.pop() {
            match task {
                Task::Bool { id, not, path } => {
                    let item = self.path.item(id);
                    match item.kind {
                        Kind::Binary(op @ (BinOp::And | BinOp::Or), l, r) => {
                            // De Morgan's laws carry a NOT down.
                            tasks.push(Task::Combine { is_and: not ^ (op == BinOp::And) });
                            tasks.push(Task::Bool { id: r, not, path });
                            tasks.push(Task::Bool { id: l, not, path });
                        }
                        Kind::Unary(UnOp::Not, a) => tasks.push(Task::Bool { id: a, not: !not, path }),
                        Kind::Unary(UnOp::Exists, a) if !not => tasks.push(Task::PathExpr { at: Some(a), path, scalar: None, nodes: Vec::new(), filter: false }),
                        Kind::Binary(BinOp::Eq, l, r) if !not => {
                            let literal = |id: ItemId| self.path.item(id).next.is_none() && matches!(self.path.item(id).kind, Kind::Null | Kind::Bool(_) | Kind::Numeric(_) | Kind::String(_));
                            if literal(l) {
                                tasks.push(Task::PathExpr { at: Some(r), path, scalar: Some(l), nodes: Vec::new(), filter: false });
                            } else if literal(r) {
                                tasks.push(Task::PathExpr { at: Some(l), path, scalar: Some(r), nodes: Vec::new(), filter: false });
                            } else {
                                results.push(None);
                            }
                        }
                        // `!=`, NOT EXISTS, NOT `==` and the rest give no entries.
                        _ => results.push(None),
                    }
                }
                Task::Combine { is_and } => {
                    let r = results.pop().flatten();
                    let l = results.pop().flatten();
                    results.push(match (l, r) {
                        (Some(a), Some(b)) => Some(self.node(if is_and { Node::And(vec![a, b]) } else { Node::Or(vec![a, b]) })),
                        // An AND can rely on one side (rows are rechecked); an OR needs both.
                        (a, b) if is_and => a.or(b),
                        _ => None,
                    });
                }
                Task::PathExpr { at, mut path, scalar, mut nodes, filter } => {
                    if filter && let Some(f) = results.pop().flatten() {
                        nodes.push(f);
                    }
                    let mut cur = at;
                    let mut supported = true;
                    let mut suspended = false;
                    while let Some(id) = cur {
                        let item = self.path.item(id);
                        match item.kind {
                            Kind::Current => {}
                            Kind::Unary(UnOp::Filter, arg) => {
                                tasks.push(Task::PathExpr { at: item.next, path, scalar, nodes: core::mem::take(&mut nodes), filter: true });
                                tasks.push(Task::Bool { id: arg, not: false, path });
                                suspended = true;
                                break;
                            }
                            _ => {
                                if !self.add_accessor(&mut path, id) {
                                    // Unsupported accessor: only the filters found so far.
                                    supported = false;
                                    break;
                                }
                            }
                        }
                        cur = item.next;
                    }
                    if suspended {
                        continue;
                    }
                    if supported {
                        self.extract_nodes(path, scalar, &mut nodes)?;
                    }
                    let n = self.combine(nodes);
                    results.push(n);
                }
            }
        }
        Ok(results.pop().flatten())
    }

    /// The tree under `root` in postfix order, without recursion.
    fn program(&self, root: usize) -> Vec<Op> {
        let mut out = Vec::new();
        let mut stack = vec![(root, false)];
        while let Some((n, done)) = stack.pop() {
            match &self.nodes[n] {
                Node::Entry(i) => out.push(Op::Entry(*i as u32)),
                Node::And(args) | Node::Or(args) if done => {
                    let k = args.len() as u32;
                    out.push(if matches!(self.nodes[n], Node::And(_)) { Op::And(k) } else { Op::Or(k) });
                }
                Node::And(args) | Node::Or(args) => {
                    stack.push((n, true));
                    stack.extend(args.iter().rev().map(|&a| (a, false)));
                }
            }
        }
        out
    }
}

/// `extract_jsp_query` for `cbor_ops`.
pub fn query(path: &Path, exists: bool) -> Result<Option<Query>> {
    let mut b = Builder { path, entries: Vec::new(), nodes: Vec::new(), accessors: Vec::new() };
    let Some(root) = b.extract(exists)? else { return Ok(None) };
    let program = b.program(root);
    if b.entries.is_empty() {
        return Ok(None);
    }
    Ok(Some(Query { entries: b.entries, program }))
}

#[cfg(test)]
mod tests {
    use super::super::eval::{PlainHost, Res, Seq, execute};
    use super::super::test_path::{P, compile};
    use super::*;
    use crate::ctx::PlainCtx;
    use crate::datum::Datum;
    use crate::edn;
    use crate::gin::{OpsEntry, index_entries};

    const EQ: u8 = 8;
    const AND: u8 = 4;
    const OR: u8 = 5;

    fn entry(role: Role, bytes: &[u8]) -> Vec<u8> {
        let e = OpsEntry::new(role, bytes);
        let (a, b) = e.parts();
        [a, b].concat()
    }

    /// The path's result on the document, and what the index says about it.
    fn check(lax: bool, path: &[P], doc: &str, exists: bool) -> (bool, Tri) {
        let bin = compile(lax, path);
        let path = Path::decode(&bin).unwrap();
        let mut bytes = Vec::new();
        edn::to_datum(doc.as_bytes(), &mut bytes, &mut PlainCtx::default()).unwrap();
        let d = Datum::parse(&bytes).unwrap();
        let v = Value::root(&d);
        let truth = if exists {
            execute(&path, v, None, false, false, &mut PlainHost, None).unwrap() == Res::Ok
        } else {
            let mut found = Seq::default();
            execute(&path, v, None, false, false, &mut PlainHost, Some(&mut found)).unwrap();
            matches!(found.single().map(eval::bool_or_null), Some(Ok(Some(Some(true)))))
        };
        let mut index = Vec::new();
        index_entries(&v, |e| {
            let (a, b) = e.parts();
            index.push([a, b].concat());
            Ok(())
        })
        .unwrap();
        let gin = match query(&path, exists).unwrap() {
            None => Tri::Maybe,
            Some(q) => run(&q.program, |i| if index.contains(&entry(q.entries[i].0, &q.entries[i].1)) { Tri::True } else { Tri::False }),
        };
        (truth, gin)
    }

    fn eq(l: Vec<P>, r: Vec<P>) -> P {
        P::Bin(EQ, l, r)
    }

    #[test]
    fn matching_documents_are_never_ruled_out() {
        let a_is_1 = || vec![eq(vec![P::Root, P::Key("a")], vec![P::Num("1")])];
        let docs = [r#"{"a": 1}"#, r#"{"a": 1.0}"#, r#"{"a": 4([-1, 10])}"#, r#"{"a": 5([0, 1])}"#, r#"{"a": [1]}"#, r#"{"a": 42(1)}"#, r#"7({"a": [42(1.0)]})"#, r#"{"a": 2}"#, r#"{"b": 1}"#];
        for lax in [true, false] {
            for d in docs {
                let (truth, gin) = check(lax, &a_is_1(), d, false);
                assert!(!truth || gin != Tri::False, "lax {lax}: {d}");
                // Documents without the key or the value are ruled out.
                if d.contains(": 2") || d.contains("\"b\"") {
                    assert_eq!(gin, Tri::False, "{d}");
                }
            }
        }
        let cases: Vec<(bool, Vec<P>, &str, bool)> = vec![
            (true, vec![eq(vec![P::Root, P::Key("a")], vec![P::Str("x")])], r#"{"a": 32("x")}"#, false),
            (true, vec![eq(vec![P::Root, P::Key("a")], vec![P::Str("x")])], r#"{"a": ["x"]}"#, false),
            (false, vec![eq(vec![P::Root, P::Key("a"), P::AnyArray], vec![P::Str("x")])], r#"{"a": ["x"]}"#, false),
            (true, vec![P::Root, P::Key("a"), P::AnyArray, P::Filter(vec![eq(vec![P::Cur], vec![P::Num("1.5")])])], r#"{"a": [5([-1, 3])]}"#, true),
            (true, vec![P::Root, P::Filter(vec![P::Bin(OR, vec![eq(vec![P::Cur, P::Key("a")], vec![P::Num("1")])], vec![eq(vec![P::Cur, P::Key("b")], vec![P::Num("2")])])])], r#"{"b": 2}"#, true),
            (true, vec![P::Root, P::Any(0, u32::MAX), P::Filter(vec![eq(vec![P::Cur], vec![P::Num("1")])])], r#"[[{"x": [1.0]}]]"#, true),
            (false, vec![P::Root, P::Any(0, u32::MAX), P::Filter(vec![eq(vec![P::Cur], vec![P::Num("1")])])], r#"{"x": {"y": 1}}"#, true),
            (true, vec![eq(vec![P::Root, P::Key("a"), P::Key("b")], vec![P::Num("0")])], r#"42({"a": {"b": -0.0}})"#, false),
            (true, vec![P::Bin(AND, vec![eq(vec![P::Root, P::Key("a")], vec![P::Num("1")])], vec![eq(vec![P::Root, P::Key("b")], vec![P::Root, P::Key("c")])])], r#"{"a": 1, "b": 2, "c": 2}"#, false),
            (true, vec![P::Not(vec![eq(vec![P::Root, P::Key("a")], vec![P::Num("1")])])], r#"{"a": 2}"#, false),
            (true, vec![P::Exists(vec![P::Root, P::Key("a")])], r#"{"a": 2}"#, false),
        ];
        for (lax, path, d, exists) in cases {
            let (truth, gin) = check(lax, &path, d, exists);
            assert!(truth, "the case should match: {d}");
            assert_ne!(gin, Tri::False, "{d}");
        }
    }

    #[test]
    fn conditions_without_literals_need_every_row() {
        assert!(query(&Path::decode(&compile(true, &[P::Not(vec![eq(vec![P::Root, P::Key("a")], vec![P::Num("1")])])])).unwrap(), false).unwrap().is_none());
        assert!(query(&Path::decode(&compile(true, &[P::Root, P::Key("a")])).unwrap(), true).unwrap().is_none());
    }

    #[test]
    fn equality_forms_unify_numbers_and_drop_tags() {
        let form = |t: &str| {
            let mut b = Vec::new();
            edn::to_datum(t.as_bytes(), &mut b, &mut PlainCtx::default()).unwrap();
            equality_form(Value::root(&Datum::parse(&b).unwrap())).unwrap()
        };
        for (a, b) in [("1", "1.0"), ("1", "4([-1, 10])"), ("1.5", "5([-1, 3])"), ("1.5", "4([-2, 150])"), ("0", "-0.0"), ("\"x\"", "32(\"x\")"), ("true", "42(true)"), ("18446744073709551616", "4([0, 18446744073709551616])")] {
            assert_eq!(form(a), form(b), "{a} {b}");
            assert!(form(a).is_some());
        }
        for t in ["h'01'", "undefined", "NaN", "0(\"2013-03-21T20:04:00Z\")", "[1]", "4([100000000, 1])"] {
            assert_eq!(form(t), None, "{t}");
        }
        assert_ne!(form("1"), form("1.5"));
    }
}
