//! Packed input (design §6): IETF Packed CBOR (draft-19: tags 113/1113, shared and argument references,
//! join/ijoin/record functions), cbor-x packing (tag 51) and cbor-x records (tags 57342–65535).
//! Unpacking decodes into an arena, resolves references with an explicit task stack, and serializes plain CBOR.

use crate::ctx::{Ctx, INTERRUPT_EVERY};
use crate::cursor::*;
use crate::encode::write_head;
use crate::error::{Error, ErrorKind, NO_OFFSET, Result};
use crate::out::try_push;

/// Tags that start unpacking when met during normalization.
pub fn is_packing_tag(tag: u64) -> bool {
    matches!(tag, 51 | 113 | 1113 | RECORD_DEFS | RECORD_INLINE) || is_record_ref(tag)
}

const RECORD_DEFS: u64 = 57342;
const RECORD_INLINE: u64 = 57343;
const RECORD_FIRST: u64 = 57344;
/// cbor-x reads any tag from 0xe000 as a record reference (id = tag & 0x1fff), so 8192 ids, not the 256 of the
/// cbor-records spec.
const RECORD_LAST: u64 = 0xffff;
/// Highest id written: cbor-x resolves 0xffff's id (8191) against the inline-definition tag 0xdfff, so it is avoided.
const RECORD_LAST_WRITTEN: u64 = 0xfffe;
const UNDEFINED: u8 = 0xf7;
/// Longest chain of references followed while resolving one item; stops reference loops.
const MAX_CHASE: u32 = 64;

fn is_record_ref(t: u64) -> bool {
    (RECORD_FIRST..=RECORD_LAST).contains(&t)
}

fn bad(msg: &'static str) -> Error {
    Error::new(ErrorKind::InvalidArgument(msg), NO_OFFSET)
}

type Id = usize;

#[derive(Clone)]
enum Node {
    /// An integer, float or simple value, as its raw encoding.
    Scalar(Vec<u8>),
    Str { major: u8, data: Vec<u8> },
    Array(Vec<Id>),
    Map(Vec<(Id, Id)>),
    Tag(u64, Id),
}

/// Reference tables in force: entries are input nodes with the scope they are interpreted in.
#[derive(Default)]
struct Scope {
    /// False at the root: references are plain values outside any table setup tag.
    packed: bool,
    shared: Vec<(Id, usize)>,
    args: Vec<(Id, usize)>,
}

struct Arena {
    nodes: Vec<Node>,
    /// Memory left for nodes; every node and its payload is charged, so blow-ups stop early.
    budget: usize,
}

impl Arena {
    fn add(&mut self, n: Node) -> Result<Id> {
        let payload = match &n {
            Node::Scalar(raw) => raw.len(),
            Node::Str { data, .. } => data.len(),
            Node::Array(v) => v.len() * core::mem::size_of::<Id>(),
            Node::Map(v) => v.len() * 2 * core::mem::size_of::<Id>(),
            Node::Tag(..) => 0,
        };
        let cost = core::mem::size_of::<Node>() + payload;
        self.budget = self.budget.checked_sub(cost).ok_or(Error::new(ErrorKind::ExpansionLimit, NO_OFFSET))?;
        try_push(&mut self.nodes, n)?;
        Ok(self.nodes.len() - 1)
    }
    fn get(&self, id: Id) -> Result<&Node> {
        self.nodes.get(id).ok_or(bad("invalid packed reference"))
    }
}

// ---- decoding into the arena ----

enum DFrame {
    Array { id: Id, left: Option<u64> },
    Map { id: Id, left: Option<u64>, key: Option<Id> },
    Tag { tag: u64 },
}

/// Decodes one well-formed item (definite or indefinite) into the arena; returns its id.
fn decode(input: &[u8], a: &mut Arena, max_depth: usize) -> Result<Id> {
    let mut c = Cursor::new(input);
    let mut stack: Vec<DFrame> = Vec::new();
    loop {
        // Close containers that are complete.
        let mut done: Option<Id> = None;
        match stack.last() {
            Some(DFrame::Array { left: Some(0), id }) | Some(DFrame::Map { left: Some(0), id, .. }) => {
                done = Some(*id);
            }
            Some(DFrame::Array { left: None, id }) | Some(DFrame::Map { left: None, id, .. }) if c.peek()? == BREAK => {
                c.byte()?;
                done = Some(*id);
            }
            _ => {}
        }
        let mut item = match done {
            Some(id) => {
                if let Some(DFrame::Map { key: Some(_), .. }) = stack.last() {
                    return Err(c.err(ErrorKind::MissingValue));
                }
                stack.pop();
                id
            }
            None => {
                let at = c.pos();
                let h = c.head()?;
                match h.major {
                    MAJOR_UNSIGNED | MAJOR_NEGATIVE | MAJOR_SIMPLE => {
                        if h.is_break() {
                            return Err(Error::new(ErrorKind::UnexpectedBreak, at));
                        }
                        let raw = input.get(at..c.pos()).ok_or(c.err(ErrorKind::Truncated))?.to_vec();
                        a.add(Node::Scalar(raw))?
                    }
                    MAJOR_BYTES | MAJOR_TEXT => {
                        let mut data = Vec::new();
                        if h.is_indefinite() {
                            loop {
                                if c.peek()? == BREAK {
                                    c.byte()?;
                                    break;
                                }
                                let ch = c.head()?;
                                if ch.major != h.major || ch.is_indefinite() {
                                    return Err(c.err(ErrorKind::InvalidChunk));
                                }
                                data.extend_from_slice(c.take(ch.arg)?);
                            }
                        } else {
                            data.extend_from_slice(c.take(h.arg)?);
                        }
                        a.add(Node::Str { major: h.major, data })?
                    }
                    MAJOR_ARRAY | MAJOR_MAP | MAJOR_TAG => {
                        if stack.len() >= max_depth {
                            return Err(Error::new(ErrorKind::DepthLimit, at));
                        }
                        let left = if h.is_indefinite() {
                            None
                        } else {
                            let per = if h.major == MAJOR_MAP { 2 } else { 1 };
                            if h.major != MAJOR_TAG && h.arg > (c.remaining() / per) as u64 {
                                return Err(Error::new(ErrorKind::LengthTooLarge, at));
                            }
                            Some(h.arg)
                        };
                        let frame = match h.major {
                            MAJOR_ARRAY => DFrame::Array { id: a.add(Node::Array(Vec::new()))?, left },
                            MAJOR_MAP => DFrame::Map { id: a.add(Node::Map(Vec::new()))?, left, key: None },
                            _ => DFrame::Tag { tag: h.arg },
                        };
                        try_push(&mut stack, frame)?;
                        continue;
                    }
                    _ => return Err(Error::new(ErrorKind::Reserved, at)),
                }
            }
        };
        // Attach the finished item to its parent; tags close as soon as their content is done.
        loop {
            match stack.last_mut() {
                None => {
                    if !c.is_empty() {
                        return Err(c.err(ErrorKind::TrailingBytes));
                    }
                    return Ok(item);
                }
                Some(DFrame::Tag { tag }) => {
                    let t = *tag;
                    stack.pop();
                    item = a.add(Node::Tag(t, item))?;
                }
                Some(DFrame::Array { id, left }) => {
                    if let Some(l) = left {
                        *l -= 1;
                    }
                    let id = *id;
                    if let Some(Node::Array(v)) = a.nodes.get_mut(id) {
                        try_push(v, item)?;
                    }
                    break;
                }
                Some(DFrame::Map { id, left, key }) => match key.take() {
                    None => {
                        *key = Some(item);
                        break;
                    }
                    Some(k) => {
                        if let Some(l) = left {
                            *l -= 1;
                        }
                        let id = *id;
                        if let Some(Node::Map(v)) = a.nodes.get_mut(id) {
                            try_push(v, (k, item))?;
                        }
                        break;
                    }
                },
            }
        }
    }
}

// ---- resolution ----

/// Work items; results go on the value stack.
enum Task {
    Eval { id: Id, scope: usize, chase: u32 },
    BuildArray(usize),
    BuildMap(usize),
    BuildTag(u64),
    /// Pops the content of a tag 6 and turns it into a shared or argument reference.
    Ref6 { scope: usize, chase: u32 },
    /// Pops right-hand then left-hand side; `rump_is_right` says which side gives the string type.
    Combine { rump_is_right: bool },
    /// Pops the evaluated key array of a record definition and stores it under `tag`.
    DefineRecord { tag: u64 },
    /// Pops `n` values and pairs them with the keys of record `tag`.
    BuildRecord { tag: u64, n: usize },
    PushRecordLayer,
    PopRecordLayer,
}

struct Resolver<'c, C: Ctx> {
    a: Arena,
    scopes: Vec<Scope>,
    /// Record definitions (tag → evaluated key ids), innermost layer last.
    records: Vec<Vec<(u64, Vec<Id>)>>,
    tasks: Vec<Task>,
    values: Vec<Id>,
    ctx: &'c mut C,
    steps: u32,
}

impl<C: Ctx> Resolver<'_, C> {
    fn pop(&mut self) -> Result<Id> {
        self.values.pop().ok_or(bad("invalid packed structure"))
    }

    fn push_task(&mut self, t: Task) -> Result<()> {
        try_push(&mut self.tasks, t)
    }

    fn array_items(&self, id: Id) -> Result<Vec<Id>> {
        match self.a.get(id)? {
            Node::Array(v) => Ok(v.clone()),
            _ => Err(bad("packed table setup expects an array")),
        }
    }

    fn new_scope(&mut self, parent: usize, shared: &[Id], args: &[Id]) -> Result<usize> {
        let id = self.scopes.len();
        let p = self.scopes.get(parent).ok_or(bad("invalid scope"))?;
        // New entries use the new number space; inherited ones keep theirs.
        let mut s = Scope { packed: true, shared: Vec::new(), args: Vec::new() };
        s.shared.extend(shared.iter().map(|&n| (n, id)));
        s.shared.extend(p.shared.iter().copied());
        s.args.extend(args.iter().map(|&n| (n, id)));
        s.args.extend(p.args.iter().copied());
        try_push(&mut self.scopes, s)?;
        Ok(id)
    }

    fn shared(&self, scope: usize, index: u64, chase: u32) -> Result<Task> {
        let s = self.scopes.get(scope).ok_or(bad("invalid scope"))?;
        let &(id, sc) = usize::try_from(index).ok().and_then(|i| s.shared.get(i)).ok_or(bad("packed reference outside the shared item table"))?;
        Ok(Task::Eval { id, scope: sc, chase: chase + 1 })
    }

    /// Queues an argument reference: argument and rump combined (straight: argument first).
    fn argument(&mut self, scope: usize, index: u64, straight: bool, rump: Id, chase: u32) -> Result<()> {
        let s = self.scopes.get(scope).ok_or(bad("invalid scope"))?;
        let &(arg, asc) = usize::try_from(index).ok().and_then(|i| s.args.get(i)).ok_or(bad("packed reference outside the argument table"))?;
        let (l, ls, r, rs) = if straight { (arg, asc, rump, scope) } else { (rump, scope, arg, asc) };
        self.push_task(Task::Combine { rump_is_right: straight })?;
        self.push_task(Task::Eval { id: r, scope: rs, chase: chase + 1 })?;
        self.push_task(Task::Eval { id: l, scope: ls, chase: chase + 1 })
    }

    fn record_keys(&self, tag: u64) -> Result<Vec<Id>> {
        for layer in self.records.iter().rev() {
            if let Some((_, k)) = layer.iter().rev().find(|(t, _)| *t == tag) {
                return Ok(k.clone());
            }
        }
        Err(bad("cbor-x record reference without a definition in the value"))
    }

    fn eval(&mut self, id: Id, scope: usize, chase: u32) -> Result<()> {
        if chase > MAX_CHASE {
            return Err(Error::new(ErrorKind::ExpansionLimit, NO_OFFSET));
        }
        let packed = self.scopes.get(scope).is_some_and(|s| s.packed);
        let node = self.a.get(id)?.clone();
        match node {
            Node::Array(items) => {
                self.push_task(Task::BuildArray(items.len()))?;
                for &c in items.iter().rev() {
                    self.push_task(Task::Eval { id: c, scope, chase })?;
                }
            }
            Node::Map(entries) => {
                self.push_task(Task::BuildMap(entries.len()))?;
                for &(k, v) in entries.iter().rev() {
                    self.push_task(Task::Eval { id: v, scope, chase })?;
                    self.push_task(Task::Eval { id: k, scope, chase })?;
                }
            }
            Node::Scalar(ref raw) if packed && raw.len() == 1 && raw[0] >= 0xe0 && raw[0] < 0xf0 => {
                let t = self.shared(scope, u64::from(raw[0] - 0xe0), chase)?;
                self.push_task(t)?;
            }
            Node::Scalar(_) | Node::Str { .. } => try_push(&mut self.values, id)?,
            Node::Tag(t, content) => self.eval_tag(t, content, scope, chase, packed)?,
        }
        Ok(())
    }

    fn eval_tag(&mut self, t: u64, content: Id, scope: usize, chase: u32, packed: bool) -> Result<()> {
        match t {
            113 | 1113 | 51 => {
                let parts = self.array_items(content)?;
                let (shared, args, rump) = match (t, parts.as_slice()) {
                    (113, [table, rump]) => {
                        let items = self.array_items(*table)?;
                        (items.clone(), items, *rump)
                    }
                    (1113, [s, a, rump]) => (self.array_items(*s)?, self.array_items(*a)?, *rump),
                    // cbor-x: [values, prefixes, suffixes, rump]; its encoder leaves prefixes and suffixes empty.
                    (51, [values, _, _, rump]) => (self.array_items(*values)?, Vec::new(), *rump),
                    _ => return Err(bad("malformed packed table setup")),
                };
                let s = self.new_scope(scope, &shared, &args)?;
                self.push_task(Task::Eval { id: rump, scope: s, chase })
            }
            6 if packed => {
                self.push_task(Task::Ref6 { scope, chase })?;
                self.push_task(Task::Eval { id: content, scope, chase })
            }
            128..=143 if packed => self.argument(scope, (t - 128) % 8, t < 136, content, chase),
            RECORD_INLINE => {
                let parts = self.array_items(content)?;
                let [idn, keys, values @ ..] = parts.as_slice() else { return Err(bad("malformed cbor-x inline record")) };
                let tag = self.record_id(*idn)?;
                // Define first (keys evaluated), then build this record from its values.
                self.push_task(Task::BuildRecord { tag, n: values.len() })?;
                for &v in values.iter().rev() {
                    self.push_task(Task::Eval { id: v, scope, chase })?;
                }
                self.push_task(Task::DefineRecord { tag })?;
                self.push_task(Task::Eval { id: *keys, scope, chase })
            }
            RECORD_DEFS => {
                let parts = self.array_items(content)?;
                let [idn, defs @ .., primary] = parts.as_slice() else { return Err(bad("malformed cbor-x record definitions")) };
                let first = self.record_id(*idn)?;
                self.push_task(Task::PopRecordLayer)?;
                self.push_task(Task::Eval { id: *primary, scope, chase })?;
                for (i, &d) in defs.iter().enumerate().rev() {
                    self.push_task(Task::DefineRecord { tag: first + i as u64 })?;
                    self.push_task(Task::Eval { id: d, scope, chase })?;
                }
                self.push_task(Task::PushRecordLayer)
            }
            t if is_record_ref(t) => {
                let values = self.array_items(content)?;
                self.push_task(Task::BuildRecord { tag: t, n: values.len() })?;
                for &v in values.iter().rev() {
                    self.push_task(Task::Eval { id: v, scope, chase })?;
                }
                Ok(())
            }
            _ => {
                self.push_task(Task::BuildTag(t))?;
                self.push_task(Task::Eval { id: content, scope, chase })
            }
        }
    }

    /// Record id from its integer node: a tag number in cbor-x's record range.
    fn record_id(&self, id: Id) -> Result<u64> {
        match self.a.get(id)? {
            Node::Scalar(raw) => {
                let mut c = Cursor::new(raw);
                let h = c.head()?;
                if h.major == MAJOR_UNSIGNED && is_record_ref(h.arg) {
                    return Ok(h.arg);
                }
                Err(bad("cbor-x record id out of range"))
            }
            _ => Err(bad("cbor-x record id must be an integer")),
        }
    }

    fn int_of(&self, id: Id) -> Result<Option<i128>> {
        Ok(match self.a.get(id)? {
            Node::Scalar(raw) => {
                let mut c = Cursor::new(raw);
                let h = c.head()?;
                match h.major {
                    MAJOR_UNSIGNED => Some(i128::from(h.arg)),
                    MAJOR_NEGATIVE => Some(-1 - i128::from(h.arg)),
                    _ => None,
                }
            }
            _ => None,
        })
    }

    fn run(&mut self, root: Id) -> Result<Id> {
        self.push_task(Task::Eval { id: root, scope: 0, chase: 0 })?;
        while let Some(task) = self.tasks.pop() {
            self.steps = self.steps.wrapping_add(1);
            if self.steps.is_multiple_of(INTERRUPT_EVERY) {
                self.ctx.check_interrupts();
            }
            match task {
                Task::Eval { id, scope, chase } => self.eval(id, scope, chase)?,
                Task::BuildArray(n) => {
                    let at = self.values.len().checked_sub(n).ok_or(bad("invalid packed structure"))?;
                    let items = self.values.split_off(at);
                    let id = self.a.add(Node::Array(items))?;
                    try_push(&mut self.values, id)?;
                }
                Task::BuildMap(n) => {
                    let at = self.values.len().checked_sub(2 * n).ok_or(bad("invalid packed structure"))?;
                    let flat = self.values.split_off(at);
                    let id = self.a.add(Node::Map(flat.chunks(2).map(|kv| (kv[0], kv[1])).collect()))?;
                    try_push(&mut self.values, id)?;
                }
                Task::BuildTag(t) => {
                    let c = self.pop()?;
                    let id = self.a.add(Node::Tag(t, c))?;
                    try_push(&mut self.values, id)?;
                }
                Task::Ref6 { scope, chase } => {
                    let c = self.pop()?;
                    if let Some(n) = self.int_of(c)? {
                        // Zigzag: 6(N≥0) → 16+2N, 6(N<0) → 16−2N−1.
                        let idx = if n >= 0 { 16 + 2 * n } else { 16 - 2 * n - 1 };
                        let t = self.shared(scope, u64::try_from(idx).map_err(|_| bad("packed reference out of range"))?, chase)?;
                        self.push_task(t)?;
                    } else if let Node::Array(v) = self.a.get(c)? {
                        let [n, rump] = v.as_slice() else { return Err(bad("malformed tag 6 reference")) };
                        let (n, rump) = (*n, *rump);
                        let n = self.int_of(n)?.ok_or(bad("malformed tag 6 reference"))?;
                        let (idx, straight) = if n >= 0 { (8 + n, true) } else { (8 - n - 1, false) };
                        // The rump is already evaluated: wrap it so `argument` evaluates it as a plain value.
                        self.argument(scope, u64::try_from(idx).map_err(|_| bad("packed reference out of range"))?, straight, rump, chase)?;
                    } else {
                        return Err(bad("malformed tag 6 reference"));
                    }
                }
                Task::Combine { rump_is_right } => {
                    let r = self.pop()?;
                    let l = self.pop()?;
                    let id = match self.a.get(l)?.clone() {
                        Node::Tag(105, c) => self.join(r, c, false)?,
                        Node::Tag(106, c) => self.join(c, r, rump_is_right)?,
                        Node::Tag(114, c) => self.record_fn(c, r)?,
                        _ => self.concat(l, r, rump_is_right)?,
                    };
                    try_push(&mut self.values, id)?;
                }
                Task::DefineRecord { tag } => {
                    let keys = self.pop()?;
                    let keys = self.array_items(keys)?;
                    match self.records.last_mut() {
                        Some(layer) => try_push(layer, (tag, keys))?,
                        None => return Err(bad("invalid record scope")),
                    }
                }
                Task::BuildRecord { tag, n } => {
                    let keys = self.record_keys(tag)?;
                    let at = self.values.len().checked_sub(n).ok_or(bad("invalid packed structure"))?;
                    let vals = self.values.split_off(at);
                    if vals.len() > keys.len() {
                        return Err(bad("cbor-x record has more values than keys"));
                    }
                    let id = self.a.add(Node::Map(keys.into_iter().zip(vals).collect()))?;
                    try_push(&mut self.values, id)?;
                }
                Task::PushRecordLayer => try_push(&mut self.records, Vec::new())?,
                Task::PopRecordLayer => {
                    self.records.pop();
                }
            }
        }
        let root = self.pop()?;
        if !self.values.is_empty() {
            return Err(bad("invalid packed structure"));
        }
        Ok(root)
    }

    /// Concatenation (draft §2.4).
    fn concat(&mut self, l: Id, r: Id, rump_is_right: bool) -> Result<Id> {
        let (ln, rn) = (self.a.get(l)?.clone(), self.a.get(r)?.clone());
        let n = match (ln, rn) {
            (Node::Array(a), Node::Array(b)) => Node::Array(a.into_iter().chain(b).collect()),
            (Node::Map(a), Node::Map(b)) => Node::Map(self.merge_maps(&a, &b)?),
            (Node::Str { major: lm, data: ld }, Node::Str { major: rm, data: rd }) => {
                let mut data = ld;
                data.extend_from_slice(&rd);
                Node::Str { major: if rump_is_right { rm } else { lm }, data }
            }
            (Node::Str { .. }, Node::Array(_)) => return self.join(l, r, true),
            (Node::Array(_), Node::Str { .. }) => return self.join(r, l, false),
            _ => return Err(bad("packed concatenation of incompatible items")),
        };
        self.a.add(n)
    }

    /// Map concatenation: right-hand entries replace left-hand ones; an undefined value removes the key.
    fn merge_maps(&self, a: &[(Id, Id)], b: &[(Id, Id)]) -> Result<Vec<(Id, Id)>> {
        let mut by_key: std::collections::BTreeMap<Vec<u8>, (Id, Id)> = std::collections::BTreeMap::new();
        for &(k, v) in a {
            by_key.insert(self.bytes_of(k)?, (k, v));
        }
        for &(k, v) in b {
            let kb = self.bytes_of(k)?;
            if matches!(self.a.get(v)?, Node::Scalar(raw) if raw.as_slice() == [UNDEFINED]) {
                by_key.remove(&kb);
            } else {
                by_key.insert(kb, (k, v));
            }
        }
        Ok(by_key.into_values().collect())
    }

    /// join(joiner, array): elements concatenated with the joiner between them, built in one pass.
    fn join(&mut self, joiner: Id, arr: Id, _rump_is_right: bool) -> Result<Id> {
        let items = self.array_items(arr)?;
        let j = self.a.get(joiner)?.clone();
        let Some(&first) = items.first() else {
            // Neutral element of the joiner's type.
            let n = match j {
                Node::Str { major, .. } => Node::Str { major, data: Vec::new() },
                Node::Map(_) => Node::Map(Vec::new()),
                _ => Node::Array(Vec::new()),
            };
            return self.a.add(n);
        };
        if items.len() == 1 {
            return Ok(first);
        }
        let n = match (&j, self.a.get(first)?.clone()) {
            // The first element sets the result type for mixed strings.
            (Node::Str { data: jd, .. }, Node::Str { major, .. }) => {
                let mut data = Vec::new();
                for (i, &e) in items.iter().enumerate() {
                    if i > 0 {
                        data.extend_from_slice(jd);
                    }
                    match self.a.get(e)? {
                        Node::Str { data: d, .. } => data.extend_from_slice(d),
                        _ => return Err(bad("join of incompatible items")),
                    }
                }
                Node::Str { major, data }
            }
            (Node::Array(jv), Node::Array(_)) => {
                let mut out = Vec::new();
                for (i, &e) in items.iter().enumerate() {
                    if i > 0 {
                        out.extend_from_slice(jv);
                    }
                    match self.a.get(e)? {
                        Node::Array(v) => out.extend_from_slice(v),
                        _ => return Err(bad("join of incompatible items")),
                    }
                }
                Node::Array(out)
            }
            (Node::Map(jm), Node::Map(_)) => {
                let mut acc: Vec<(Id, Id)> = Vec::new();
                for (i, &e) in items.iter().enumerate() {
                    if i > 0 {
                        acc = self.merge_maps(&acc, jm)?;
                    }
                    let m = match self.a.get(e)? {
                        Node::Map(m) => m.clone(),
                        _ => return Err(bad("join of incompatible items")),
                    };
                    acc = self.merge_maps(&acc, &m)?;
                }
                Node::Map(acc)
            }
            _ => return Err(bad("join of incompatible items")),
        };
        self.a.add(n)
    }

    /// record(keys, values): a map; missing and undefined values leave their key out.
    fn record_fn(&mut self, keys: Id, values: Id) -> Result<Id> {
        let (k, v) = (self.array_items(keys)?, self.array_items(values)?);
        if v.len() > k.len() {
            return Err(bad("record function has more values than keys"));
        }
        let mut entries = Vec::new();
        for (k, v) in k.into_iter().zip(v) {
            if !matches!(self.a.get(v)?, Node::Scalar(raw) if raw.as_slice() == [UNDEFINED]) {
                entries.push((k, v));
            }
        }
        self.a.add(Node::Map(entries))
    }

    /// Plain encoding of a resolved node (for key comparison in map concatenation).
    fn bytes_of(&self, id: Id) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        serialize(&self.a, id, &mut out, usize::MAX)?;
        Ok(out)
    }
}

/// Writes plain CBOR for a resolved node, failing past `limit` bytes.
fn serialize(a: &Arena, root: Id, out: &mut Vec<u8>, limit: usize) -> Result<()> {
    let mut todo: Vec<Id> = vec![root];
    while let Some(id) = todo.pop() {
        if out.len() > limit {
            return Err(Error::new(ErrorKind::ExpansionLimit, NO_OFFSET));
        }
        match a.get(id)? {
            Node::Scalar(raw) => out.extend_from_slice(raw),
            Node::Str { major, data } => {
                write_head(out, *major, data.len() as u64)?;
                out.extend_from_slice(data);
            }
            Node::Array(items) => {
                write_head(out, MAJOR_ARRAY, items.len() as u64)?;
                todo.extend(items.iter().rev());
            }
            Node::Map(entries) => {
                write_head(out, MAJOR_MAP, entries.len() as u64)?;
                for &(k, v) in entries.iter().rev() {
                    todo.push(v);
                    todo.push(k);
                }
            }
            Node::Tag(t, c) => {
                write_head(out, MAJOR_TAG, *t)?;
                todo.push(*c);
            }
        }
    }
    Ok(())
}

/// Unpacks packed input into plain CBOR (not yet normalized).
pub fn unpack(input: &[u8], ctx: &mut impl Ctx) -> Result<Vec<u8>> {
    let limits = *ctx.limits();
    let max_out = limits.max_body(input.len());
    // Nodes cost several times their encoded size; allow a fixed multiple of the output limit.
    let mut a = Arena { nodes: Vec::new(), budget: max_out.saturating_mul(4).max(1 << 16) };
    // Packing wrappers (tag 113 + its array, record tags) add levels; the unpacked value is depth-checked again
    // when it is normalized.
    let root = decode(input, &mut a, limits.max_depth.saturating_add(8))?;
    let mut r = Resolver {
        a,
        scopes: vec![Scope::default()],
        records: vec![Vec::new()],
        tasks: Vec::new(),
        values: Vec::new(),
        ctx,
        steps: 0,
    };
    let resolved = r.run(root)?;
    let mut out = Vec::new();
    serialize(&r.a, resolved, &mut out, max_out)?;
    Ok(out)
}

// ---- packing (output) ----

/// Output packing formats for `cbor_pack`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PackFormat {
    /// IETF Packed CBOR: tag 113 with a shared item table of repeated strings.
    Ietf,
    /// cbor-x records: maps with repeated text-key sets become record definitions and references.
    CborX,
}

use crate::out::OutBuf;
use crate::value::{Kind, Value, end_of};

/// Shortest-first reference to shared item `i` (simple values, then zigzag tag 6).
fn write_shared_ref(out: &mut impl OutBuf, i: usize) -> Result<()> {
    if i < 16 {
        return out.write_byte(0xe0 | i as u8);
    }
    let k = (i - 16) as u64;
    write_head(out, MAJOR_TAG, 6)?;
    if k.is_multiple_of(2) { write_head(out, MAJOR_UNSIGNED, k / 2) } else { write_head(out, MAJOR_NEGATIVE, k / 2) }
}

/// Encoded strings repeated in `v`, most valuable first, deterministic.
/// Empty when `v` holds plain simple values 0–15 or tags 6 and 128–143: inside tag 113 they would read as
/// references, and Packed CBOR has no way to write them literally there.
fn repeated_strings(v: &Value) -> Result<Vec<Vec<u8>>> {
    let mut counts: std::collections::BTreeMap<&[u8], u64> = std::collections::BTreeMap::new();
    let mut c = Cursor::at(v.d.body, v.pos);
    let end = v.end()?;
    while c.pos() < end {
        let at = c.pos();
        let h = c.head()?;
        let lookalike = (h.major == MAJOR_SIMPLE && h.info < 16) || (h.major == MAJOR_TAG && (h.arg == 6 || (128..=143).contains(&h.arg)));
        if lookalike {
            return Ok(Vec::new());
        }
        if matches!(h.major, MAJOR_BYTES | MAJOR_TEXT) {
            c.take(h.arg)?;
            // References cost 1–3 bytes; shorter strings never pay off.
            if h.arg >= 3 {
                *counts.entry(v.d.body.get(at..c.pos()).ok_or(bad("corrupt value"))?).or_default() += 1;
            }
        }
    }
    let mut cands: Vec<(&[u8], u64)> = counts.into_iter().filter(|&(_, n)| n >= 2).collect();
    cands.sort_by(|a, b| (b.1 * b.0.len() as u64).cmp(&(a.1 * a.0.len() as u64)).then(a.0.cmp(b.0)));
    Ok(cands.into_iter().map(|(s, _)| s.to_vec()).collect())
}

/// Text keys of a map, when every key is text.
fn record_keys(v: &Value) -> Result<Option<Vec<u8>>> {
    let mut keys = Vec::new();
    for e in v.map_iter()? {
        let (k, _) = e?;
        if !matches!(k.kind()?, Kind::Text(_)) {
            return Ok(None);
        }
        keys.extend_from_slice(k.bytes()?);
    }
    Ok(Some(keys))
}

enum PFrame {
    /// Items left to copy in an ordinary container or tag.
    Copy(u64),
    /// Values left in a map written as a record (keys are skipped).
    Record(u64),
}

/// Writes a packed encoding of `v`; unpacking it gives `v` back.
pub fn pack(v: &Value, fmt: PackFormat, out: &mut impl OutBuf) -> Result<()> {
    // IETF: table of repeated strings; cbor-x: record structures defined at first use.
    let table: std::collections::BTreeMap<Vec<u8>, usize> = match fmt {
        PackFormat::Ietf => repeated_strings(v)?.into_iter().enumerate().map(|(i, s)| (s, i)).collect(),
        PackFormat::CborX => Default::default(),
    };
    // cbor-x: text-key set → record id, and the key set held by each id slot (ids are reused round-robin when full).
    let mut records: std::collections::BTreeMap<Vec<u8>, u64> = std::collections::BTreeMap::new();
    let mut slots: Vec<Vec<u8>> = Vec::new();
    let mut next_reuse = 0usize;
    let capacity = (RECORD_LAST_WRITTEN - RECORD_FIRST + 1) as usize;
    if fmt == PackFormat::Ietf && !table.is_empty() {
        let mut ordered: Vec<(&Vec<u8>, &usize)> = table.iter().collect();
        ordered.sort_by_key(|&(_, i)| *i);
        out.write_bytes(&[0xd8, 113, 0x82])?;
        write_head(out, MAJOR_ARRAY, ordered.len() as u64)?;
        for (s, _) in ordered {
            out.write_bytes(s)?;
        }
    }
    let body = v.d.body;
    let mut c = Cursor::at(body, v.pos);
    let mut stack: Vec<PFrame> = Vec::new();
    let mut first = true;
    loop {
        // Account for the item about to be written; skip record keys.
        if !first {
            loop {
                match stack.last_mut() {
                    None => return Ok(()),
                    Some(PFrame::Copy(0)) | Some(PFrame::Record(0)) => {
                        stack.pop();
                    }
                    Some(PFrame::Copy(n)) => {
                        *n -= 1;
                        break;
                    }
                    Some(PFrame::Record(n)) => {
                        *n -= 1;
                        let key_end = end_of(v.d, c.pos())?;
                        c = Cursor::at(body, key_end);
                        break;
                    }
                }
            }
        }
        first = false;
        let at = c.pos();
        let item = Value { d: v.d, pos: at };
        let h = c.head()?;
        match h.major {
            MAJOR_BYTES | MAJOR_TEXT => {
                c.take(h.arg)?;
                let enc = body.get(at..c.pos()).ok_or(bad("corrupt value"))?;
                match table.get(enc) {
                    Some(&i) => write_shared_ref(out, i)?,
                    None => out.write_bytes(enc)?,
                }
            }
            MAJOR_ARRAY => {
                write_head(out, MAJOR_ARRAY, h.arg)?;
                try_push(&mut stack, PFrame::Copy(h.arg))?;
            }
            MAJOR_MAP => {
                // Every text-keyed map, empty ones included, becomes a record: a cbor-x decoder with records on
                // reads a plain map as a JS Map, not an object.
                let keys = if fmt == PackFormat::CborX { record_keys(&item)? } else { None };
                match keys {
                    Some(k) if records.contains_key(&k) => {
                        write_head(out, MAJOR_TAG, records[&k])?;
                        write_head(out, MAJOR_ARRAY, h.arg)?;
                        try_push(&mut stack, PFrame::Record(h.arg))?;
                    }
                    Some(k) => {
                        // A new shape: a free id, or (as cbor-x does when full) the oldest one, redefined.
                        let slot = if slots.len() < capacity {
                            slots.push(k.clone());
                            slots.len() - 1
                        } else {
                            let slot = next_reuse % capacity;
                            next_reuse += 1;
                            records.remove(&slots[slot]);
                            slots[slot] = k.clone();
                            slot
                        };
                        let id = RECORD_FIRST + slot as u64;
                        write_head(out, MAJOR_TAG, RECORD_INLINE)?;
                        write_head(out, MAJOR_ARRAY, h.arg + 2)?;
                        write_head(out, MAJOR_UNSIGNED, id)?;
                        write_head(out, MAJOR_ARRAY, h.arg)?;
                        out.write_bytes(&k)?;
                        records.insert(k, id);
                        try_push(&mut stack, PFrame::Record(h.arg))?;
                    }
                    None => {
                        write_head(out, MAJOR_MAP, h.arg)?;
                        try_push(&mut stack, PFrame::Copy(h.arg * 2))?;
                    }
                }
            }
            MAJOR_TAG => {
                write_head(out, MAJOR_TAG, h.arg)?;
                try_push(&mut stack, PFrame::Copy(1))?;
            }
            _ => out.write_bytes(body.get(at..c.pos()).ok_or(bad("corrupt value"))?)?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::PlainCtx;
    use crate::datum::Datum;
    use crate::edn;
    use crate::value::Value;

    /// EDN text → (possibly packed) datum → EDN text of the unpacked value.
    fn up(text: &str) -> std::result::Result<String, ErrorKind> {
        let mut d = Vec::new();
        edn::to_datum(text.as_bytes(), &mut d, &mut PlainCtx::default()).map_err(|e| e.kind)?;
        let mut out = Vec::new();
        edn::print(&Value::root(&Datum::parse(&d).unwrap()), &mut out, Default::default()).unwrap();
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn shared_item_references() {
        assert_eq!(up(r#"113([["a", "b"], [simple(0), simple(1), simple(0)]])"#).unwrap(), r#"["a", "b", "a"]"#);
        let table: Vec<String> = (0..20).map(|i| format!("\"s{i}\"")).collect();
        let input = format!("113([[{}], [6(0), 6(-1), 6(1)]])", table.join(", "));
        assert_eq!(up(&input).unwrap(), r#"["s16", "s17", "s18"]"#);
        // Outside a table setup tag, simple values and tag 6 are plain values.
        assert_eq!(up("[simple(0), 6(1)]").unwrap(), "[simple(0), 6(1)]");
    }

    #[test]
    fn argument_references_and_functions_follow_the_draft_examples() {
        assert_eq!(up(r#"113([["foobar", h'666f6f62', "fo"], [128("t"), 129("art"), 130("obart")]])"#).unwrap(), r#"["foobart", "foobart", "foobart"]"#);
        assert_eq!(
            up(r#"113([[106("packed.example")], [128(["https://", "/foo.html"]), 128(["mailto:support@", ""])]])"#).unwrap(),
            r#"["https://packed.example/foo.html", "mailto:support@packed.example"]"#
        );
        assert_eq!(
            up(r#"113([["packed.example"], [136(105(["https://", "/foo.html"])), 136("mailto:support@")]])"#).unwrap(),
            r#"["https://packed.example/foo.html", "mailto:support@packed.example"]"#
        );
        assert_eq!(
            up(r#"113([[114(["key1", "key2", "key0"])], [128(["value 1", 2, false]), 128(["", 0])]])"#).unwrap(),
            r#"[{"key0": false, "key1": "value 1", "key2": 2}, {"key1": "", "key2": 0}]"#
        );
        // Map concatenation: right wins, undefined removes.
        assert_eq!(up(r#"113([[{"a": 1, "b": 2}], 128({"b": undefined, "c": 3})])"#).unwrap(), r#"{"a": 1, "c": 3}"#);
    }

    #[test]
    fn cbor_x_formats() {
        // Tag 51 packing as cbor-x writes it: [values, prefixes, suffixes, rump].
        assert_eq!(up(r#"51([["repeated"], [], [], [simple(0), simple(0)]])"#).unwrap(), r#"["repeated", "repeated"]"#);
        // Records: inline definition, then references.
        assert_eq!(
            up(r#"[57343([57344, ["name", "age"], "ann", 7]), 57344(["bob", 9])]"#).unwrap(),
            r#"[{"age": 7, "name": "ann"}, {"age": 9, "name": "bob"}]"#
        );
        assert_eq!(up(r#"57342([57344, ["x", "y"], [57344([1, 2]), 57344([3, 4])]])"#).unwrap(), r#"[{"x": 1, "y": 2}, {"x": 3, "y": 4}]"#);
        assert!(up("57344([1])").is_err());
        // A redefined id replaces the earlier structure from there on.
        assert_eq!(up(r#"[57343([57344, ["a"], 1]), 57343([57344, ["b"], 2]), 57344([3])]"#).unwrap(), r#"[{"a": 1}, {"b": 2}, {"b": 3}]"#);
        // Ids beyond the 256 of the cbor-records spec, as cbor-x (and rel) write them.
        assert_eq!(up(r#"[57343([65000, ["a"], 1]), 65000([2])]"#).unwrap(), r#"[{"a": 1}, {"a": 2}]"#);
    }

    fn roundtrip(text: &str, fmt: PackFormat) -> Vec<u8> {
        let mut d = Vec::new();
        edn::to_datum(text.as_bytes(), &mut d, &mut PlainCtx::default()).unwrap();
        let mut packed = Vec::new();
        pack(&Value::root(&Datum::parse(&d).unwrap()), fmt, &mut packed).unwrap();
        let mut back = Vec::new();
        crate::datum::from_cbor(&packed, &mut back, &mut PlainCtx::default()).unwrap();
        assert_eq!(back, d, "{fmt:?} round trip of {text}");
        packed
    }

    #[test]
    fn packing_round_trips_and_shrinks_repetitive_values() {
        let rows: Vec<String> = (0..20).map(|i| format!(r#"{{"name": "user number {i}", "status": "active", "role": "administrator"}}"#)).collect();
        let text = format!("[{}]", rows.join(", "));
        let mut plain = Vec::new();
        edn::to_datum(text.as_bytes(), &mut plain, &mut PlainCtx::default()).unwrap();
        for fmt in [PackFormat::Ietf, PackFormat::CborX] {
            let packed = roundtrip(&text, fmt);
            assert!(packed.len() < plain.len() * 3 / 4, "{fmt:?}: {} vs {}", packed.len(), plain.len());
        }
        // Plain simple values and tag 6 would turn into references inside tag 113: those values stay unpacked.
        assert_eq!(roundtrip(r#"["abc", "abc", simple(3), 6(1)]"#, PackFormat::Ietf)[0], 0x84);
        for t in ["1", "[]", "{}", r#"{1: "abc", 2: "abc"}"#, r#"[{"a": 1}, {"a": {"a": 2}}, {"b": 3}]"#, r#"1({"x": ["abc", "abc"]})"#] {
            roundtrip(t, PackFormat::Ietf);
            roundtrip(t, PackFormat::CborX);
        }
    }

    #[test]
    fn loops_and_blowups_are_stopped() {
        assert_eq!(up("113([[simple(0)], simple(0)])").unwrap_err(), ErrorKind::ExpansionLimit);
        // Each level doubles the previous one: 2^40 items if unbounded.
        let mut table = vec!["\"x\"".to_string()];
        for i in 0..40 {
            let r = if i < 16 { format!("simple({i})") } else { format!("6({})", if (i - 16) % 2 == 0 { (i - 16) / 2 } else { -((i - 16) / 2) - 1 }) };
            table.push(format!("[{r}, {r}]"));
        }
        table.reverse();
        let input = format!("113([[{}], simple(0)])", table.join(", "));
        assert!(matches!(up(&input).unwrap_err(), ErrorKind::ExpansionLimit | ErrorKind::InvalidArgument(_)));
    }
}
