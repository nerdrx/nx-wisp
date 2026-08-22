//! A parser and matcher for the GBNF subset [`super::emit`] emits.
//!
//! Its job is to make the F14 tests mean something. A test that compares
//! generated grammar text to an expected string tests the emitter's formatting;
//! a test that feeds the grammar a JSON document and asks "would the decoder
//! have been able to produce this?" tests the thing that actually matters.
//!
//! The matcher is a memoised backtracking recogniser in continuation-passing
//! style. Our grammars are small, right-branching and effectively LL(1), so
//! backtracking is cheap; a step budget and a depth cap are in place anyway,
//! because a grammar bug should surface as a failed match rather than as a
//! hung test.

use std::collections::HashMap;

use crate::error::{MindError, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// A literal string. May be empty — `""` is how an optional tail bottoms
    /// out.
    Lit(String),
    Ref(String),
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
    Seq(Vec<Node>),
    Alt(Vec<Node>),
    Rep {
        node: Box<Node>,
        min: u32,
        /// `None` is unbounded.
        max: Option<u32>,
    },
}

impl Node {
    fn seq(mut v: Vec<Node>) -> Node {
        if v.len() == 1 {
            v.pop().expect("len 1")
        } else {
            Node::Seq(v)
        }
    }
    fn alt(mut v: Vec<Node>) -> Node {
        if v.len() == 1 {
            v.pop().expect("len 1")
        } else {
            Node::Alt(v)
        }
    }
}

/// A parsed grammar, rooted at the rule named `root`.
#[derive(Debug, Clone)]
pub struct Grammar {
    rules: HashMap<String, Node>,
    order: Vec<String>,
    root: String,
    src: String,
}

/// How much work a single [`Grammar::accepts`] may do before giving up. Set far
/// above anything a JSON tool call needs; it exists so a pathological grammar
/// fails a test instead of wedging CI.
const STEP_BUDGET: u64 = 2_000_000;
const MAX_DEPTH: u32 = 256;

impl Grammar {
    pub fn parse(src: &str) -> Result<Grammar> {
        Grammar::parse_rooted(src, "root")
    }

    pub fn parse_rooted(src: &str, root: &str) -> Result<Grammar> {
        let mut rules: HashMap<String, Node> = HashMap::new();
        let mut order = Vec::new();

        // Rules are one per line as we emit them, but a hand-written grammar
        // may wrap; a line without `::=` continues the previous rule.
        let mut pending: Option<(String, String)> = None;
        let flush = |pending: &mut Option<(String, String)>,
                         rules: &mut HashMap<String, Node>,
                         order: &mut Vec<String>|
         -> Result<()> {
            if let Some((name, body)) = pending.take() {
                let node = Parser::new(&body).parse_alt()?;
                if rules.insert(name.clone(), node).is_none() {
                    order.push(name);
                }
            }
            Ok(())
        };

        for raw in src.lines() {
            let line = strip_comment(raw);
            if line.trim().is_empty() {
                continue;
            }
            match split_rule(line) {
                Some((name, body)) => {
                    flush(&mut pending, &mut rules, &mut order)?;
                    pending = Some((name.to_string(), body.to_string()));
                }
                None => match pending.as_mut() {
                    Some((_, body)) => {
                        body.push(' ');
                        body.push_str(line.trim());
                    }
                    None => {
                        return Err(MindError::Grammar(format!(
                            "line before any rule: {}",
                            line.trim()
                        )))
                    }
                },
            }
        }
        flush(&mut pending, &mut rules, &mut order)?;

        if !rules.contains_key(root) {
            return Err(MindError::Grammar(format!("no `{root}` rule")));
        }
        // Every reference must resolve, or the matcher would fail confusingly
        // deep inside a document instead of at load.
        for (name, node) in &rules {
            let mut missing = Vec::new();
            collect_refs(node, &rules, &mut missing);
            if let Some(m) = missing.first() {
                return Err(MindError::Grammar(format!(
                    "rule `{name}` references undefined `{m}`"
                )));
            }
        }
        Ok(Grammar {
            rules,
            order,
            root: root.to_string(),
            src: src.to_string(),
        })
    }

    pub fn source(&self) -> &str {
        &self.src
    }
    pub fn rule_names(&self) -> &[String] {
        &self.order
    }
    pub fn rule(&self, name: &str) -> Option<&Node> {
        self.rules.get(name)
    }

    /// Would a decoder constrained by this grammar have been able to emit `s`?
    pub fn accepts(&self, s: &str) -> bool {
        self.accepts_rule(&self.root, s)
    }

    pub fn accepts_rule(&self, rule: &str, s: &str) -> bool {
        self.run(rule, s, false)
    }

    /// A prefix check: could `s` still grow into something the grammar accepts?
    /// This is the question a constrained decoder asks itself at every token,
    /// and the reason a constrained model cannot paint itself into a corner.
    pub fn accepts_prefix(&self, s: &str) -> bool {
        self.run(&self.root, s, true)
    }

    fn run(&self, rule: &str, s: &str, prefix: bool) -> bool {
        let chars: Vec<char> = s.chars().collect();
        let Some(node) = self.rules.get(rule) else {
            return false;
        };
        let m = Matcher {
            g: self,
            chars: &chars,
            prefix,
            steps: std::cell::Cell::new(0),
            overrun: std::cell::Cell::new(false),
        };
        let end = chars.len();
        let ok = m.node(node, 0, 0, &mut |pos| pos == end);
        // An overrun is never reported as "accepted": a blown budget is an
        // unknown answer, and answering "yes" to an unknown would let a broken
        // grammar pass a test.
        ok && !m.overrun.get()
    }

    /// The shortest string this grammar accepts, or `None` if every derivation
    /// is infinite (which would be an emitter bug).
    ///
    /// [`crate::backend::mock::MockBackend`] uses this to answer a
    /// grammar-constrained request without a model: the reply is guaranteed to
    /// be something the real constrained decoder could also have produced.
    pub fn shortest(&self) -> Option<String> {
        self.shortest_rule(&self.root)
    }

    pub fn shortest_rule(&self, rule: &str) -> Option<String> {
        let mut best: HashMap<String, String> = HashMap::new();
        // Fixpoint. Each pass can only shorten, and there are finitely many
        // rules, so it terminates; the bound is belt and braces.
        for _ in 0..self.rules.len().max(1) * 2 + 4 {
            let mut changed = false;
            for name in &self.order {
                let node = &self.rules[name];
                if let Some(s) = shortest_node(node, &best) {
                    match best.get(name) {
                        Some(old) if old.len() <= s.len() => {}
                        _ => {
                            best.insert(name.clone(), s);
                            changed = true;
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }
        best.get(rule).cloned()
    }
}

fn shortest_node(node: &Node, best: &HashMap<String, String>) -> Option<String> {
    match node {
        Node::Lit(s) => Some(s.clone()),
        Node::Ref(r) => best.get(r).cloned(),
        Node::Class { negated, ranges } => Some(first_char_of(*negated, ranges)?.to_string()),
        Node::Seq(v) => {
            let mut out = String::new();
            for n in v {
                out.push_str(&shortest_node(n, best)?);
            }
            Some(out)
        }
        Node::Alt(v) => v
            .iter()
            .filter_map(|n| shortest_node(n, best))
            .min_by_key(String::len),
        Node::Rep { node, min, .. } => {
            if *min == 0 {
                return Some(String::new());
            }
            let one = shortest_node(node, best)?;
            Some(one.repeat(*min as usize))
        }
    }
}

/// A representative character for a class. For a negated class we pick a
/// printable ASCII letter that is not excluded, because "the shortest string"
/// containing a control character would be technically correct and useless.
fn first_char_of(negated: bool, ranges: &[(char, char)]) -> Option<char> {
    if !negated {
        return ranges.first().map(|(a, _)| *a);
    }
    "abcdefghijklmnopqrstuvwxyz0123456789 "
        .chars()
        .find(|c| !ranges.iter().any(|(a, b)| c >= a && c <= b))
}

fn collect_refs(node: &Node, rules: &HashMap<String, Node>, missing: &mut Vec<String>) {
    match node {
        Node::Ref(r) => {
            if !rules.contains_key(r) {
                missing.push(r.clone());
            }
        }
        Node::Seq(v) | Node::Alt(v) => {
            for n in v {
                collect_refs(n, rules, missing);
            }
        }
        Node::Rep { node, .. } => collect_refs(node, rules, missing),
        Node::Lit(_) | Node::Class { .. } => {}
    }
}

fn strip_comment(line: &str) -> &str {
    // `#` only starts a comment outside a literal, and our emitter never puts
    // one inside one, but a hand-edited grammar might.
    let mut in_str = false;
    let mut esc = false;
    for (i, c) in line.char_indices() {
        if esc {
            esc = false;
            continue;
        }
        match c {
            '\\' if in_str => esc = true,
            '"' => in_str = !in_str,
            '#' if !in_str => return &line[..i],
            _ => {}
        }
    }
    line
}

fn split_rule(line: &str) -> Option<(&str, &str)> {
    let i = line.find("::=")?;
    let name = line[..i].trim();
    if name.is_empty() || !name.chars().all(is_name_char) {
        return None;
    }
    Some((name, &line[i + 3..]))
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

// ---------------------------------------------------------------------------
// Parsing one rule body
// ---------------------------------------------------------------------------

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    src: &'a str,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Parser {
            s: src.as_bytes(),
            i: 0,
            src,
        }
    }

    fn err(&self, why: &str) -> MindError {
        MindError::Grammar(format!("{why} at byte {} of `{}`", self.i, self.src.trim()))
    }

    fn ws(&mut self) {
        while self.i < self.s.len() && (self.s[self.i] as char).is_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.s.get(self.i).copied()
    }

    fn parse_alt(&mut self) -> Result<Node> {
        let mut branches = vec![self.parse_seq()?];
        while self.peek() == Some(b'|') {
            self.i += 1;
            branches.push(self.parse_seq()?);
        }
        self.ws();
        if self.i < self.s.len() {
            return Err(self.err("trailing input"));
        }
        Ok(Node::alt(branches))
    }

    fn parse_seq(&mut self) -> Result<Node> {
        let mut items = Vec::new();
        loop {
            match self.peek() {
                None | Some(b'|') | Some(b')') => break,
                _ => items.push(self.parse_postfix()?),
            }
        }
        if items.is_empty() {
            // `a ::= | b` and `a ::= ()` both mean "matches nothing extra".
            return Ok(Node::Lit(String::new()));
        }
        Ok(Node::seq(items))
    }

    fn parse_postfix(&mut self) -> Result<Node> {
        let mut node = self.parse_atom()?;
        loop {
            match self.s.get(self.i).copied() {
                Some(b'?') => {
                    self.i += 1;
                    node = Node::Rep {
                        node: Box::new(node),
                        min: 0,
                        max: Some(1),
                    };
                }
                Some(b'*') => {
                    self.i += 1;
                    node = Node::Rep {
                        node: Box::new(node),
                        min: 0,
                        max: None,
                    };
                }
                Some(b'+') => {
                    self.i += 1;
                    node = Node::Rep {
                        node: Box::new(node),
                        min: 1,
                        max: None,
                    };
                }
                _ => break,
            }
        }
        Ok(node)
    }

    fn parse_atom(&mut self) -> Result<Node> {
        match self.peek() {
            None => Err(self.err("unexpected end")),
            Some(b'(') => {
                self.i += 1;
                let inner = {
                    let mut branches = vec![self.parse_seq()?];
                    while self.peek() == Some(b'|') {
                        self.i += 1;
                        branches.push(self.parse_seq()?);
                    }
                    Node::alt(branches)
                };
                if self.peek() != Some(b')') {
                    return Err(self.err("unclosed ("));
                }
                self.i += 1;
                Ok(inner)
            }
            Some(b'"') => self.parse_literal(),
            Some(b'[') => self.parse_class(),
            Some(c) if is_name_char(c as char) => {
                let start = self.i;
                while self
                    .s
                    .get(self.i)
                    .is_some_and(|c| is_name_char(*c as char))
                {
                    self.i += 1;
                }
                Ok(Node::Ref(self.src[start..self.i].to_string()))
            }
            Some(c) => Err(self.err(&format!("unexpected `{}`", c as char))),
        }
    }

    fn parse_literal(&mut self) -> Result<Node> {
        debug_assert_eq!(self.s[self.i], b'"');
        self.i += 1;
        let mut out = String::new();
        loop {
            let Some(c) = self.s.get(self.i).copied() else {
                return Err(self.err("unterminated literal"));
            };
            self.i += 1;
            match c {
                b'"' => return Ok(Node::Lit(out)),
                b'\\' => {
                    let Some(e) = self.s.get(self.i).copied() else {
                        return Err(self.err("unterminated escape"));
                    };
                    self.i += 1;
                    out.push(self.unescape(e)?);
                }
                _ => {
                    // Re-decode from the original str so multi-byte UTF-8 in a
                    // literal survives.
                    let rest = &self.src[self.i - 1..];
                    let ch = rest.chars().next().expect("non-empty");
                    self.i = self.i - 1 + ch.len_utf8();
                    out.push(ch);
                }
            }
        }
    }

    fn unescape(&mut self, e: u8) -> Result<char> {
        Ok(match e {
            b'n' => '\n',
            b't' => '\t',
            b'r' => '\r',
            b'"' => '"',
            b'\'' => '\'',
            b'\\' => '\\',
            b'/' => '/',
            b']' => ']',
            b'[' => '[',
            b'-' => '-',
            b'x' => {
                let hex = self
                    .src
                    .get(self.i..self.i + 2)
                    .ok_or_else(|| self.err("short \\x escape"))?;
                self.i += 2;
                let v = u32::from_str_radix(hex, 16).map_err(|_| self.err("bad \\x escape"))?;
                char::from_u32(v).ok_or_else(|| self.err("bad \\x escape"))?
            }
            b'u' => {
                let hex = self
                    .src
                    .get(self.i..self.i + 4)
                    .ok_or_else(|| self.err("short \\u escape"))?;
                self.i += 4;
                let v = u32::from_str_radix(hex, 16).map_err(|_| self.err("bad \\u escape"))?;
                char::from_u32(v).ok_or_else(|| self.err("bad \\u escape"))?
            }
            other => return Err(self.err(&format!("unknown escape \\{}", other as char))),
        })
    }

    fn parse_class(&mut self) -> Result<Node> {
        debug_assert_eq!(self.s[self.i], b'[');
        self.i += 1;
        let negated = if self.s.get(self.i) == Some(&b'^') {
            self.i += 1;
            true
        } else {
            false
        };
        let mut ranges = Vec::new();
        loop {
            let Some(c) = self.s.get(self.i).copied() else {
                return Err(self.err("unterminated class"));
            };
            if c == b']' {
                self.i += 1;
                break;
            }
            let lo = self.class_char()?;
            // `-` is a range only between two characters, never at the end.
            let hi = if self.s.get(self.i) == Some(&b'-') && self.s.get(self.i + 1) != Some(&b']') {
                self.i += 1;
                self.class_char()?
            } else {
                lo
            };
            ranges.push((lo, hi));
        }
        if ranges.is_empty() {
            return Err(self.err("empty class"));
        }
        Ok(Node::Class { negated, ranges })
    }

    fn class_char(&mut self) -> Result<char> {
        let Some(c) = self.s.get(self.i).copied() else {
            return Err(self.err("unterminated class"));
        };
        self.i += 1;
        if c == b'\\' {
            let Some(e) = self.s.get(self.i).copied() else {
                return Err(self.err("unterminated escape"));
            };
            self.i += 1;
            return self.unescape(e);
        }
        let rest = &self.src[self.i - 1..];
        let ch = rest.chars().next().expect("non-empty");
        self.i = self.i - 1 + ch.len_utf8();
        Ok(ch)
    }
}


// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

/// The recogniser.
///
/// Everything takes `&self` and the mutable bookkeeping lives in `Cell`s. That
/// is not a style choice: continuation-passing over `&mut self` needs the
/// continuation to name the matcher's lifetime, and the resulting signatures
/// are unreadable. With shared borrows a continuation is just
/// `&mut dyn FnMut(usize) -> bool` that closes over `self`.
struct Matcher<'a> {
    g: &'a Grammar,
    chars: &'a [char],
    /// In prefix mode, running out of input mid-derivation is success rather
    /// than failure.
    prefix: bool,
    steps: std::cell::Cell<u64>,
    overrun: std::cell::Cell<bool>,
}

type Cont<'c> = dyn FnMut(usize) -> bool + 'c;

impl<'a> Matcher<'a> {
    fn tick(&self, depth: u32) -> bool {
        let n = self.steps.get() + 1;
        self.steps.set(n);
        if n > STEP_BUDGET || depth > MAX_DEPTH {
            self.overrun.set(true);
            return false;
        }
        true
    }

    fn node(&self, node: &'a Node, pos: usize, depth: u32, k: &mut Cont<'_>) -> bool {
        if !self.tick(depth) {
            return false;
        }
        match node {
            Node::Lit(lit) => {
                let want: Vec<char> = lit.chars().collect();
                let have = self.chars.len().saturating_sub(pos);
                if want.len() > have {
                    // Ran out of input part-way through a literal: in prefix
                    // mode that is exactly what "still could" looks like.
                    return self.prefix && self.chars[pos..] == want[..have];
                }
                if self.chars[pos..pos + want.len()] != want[..] {
                    return false;
                }
                k(pos + want.len())
            }
            Node::Class { negated, ranges } => {
                let Some(c) = self.chars.get(pos).copied() else {
                    return self.prefix;
                };
                let inside = ranges.iter().any(|(a, b)| c >= *a && c <= *b);
                if inside == *negated {
                    return false;
                }
                k(pos + 1)
            }
            Node::Ref(name) => match self.g.rules.get(name) {
                Some(sub) => self.node(sub, pos, depth + 1, k),
                None => false,
            },
            Node::Alt(branches) => branches.iter().any(|b| self.node(b, pos, depth + 1, k)),
            Node::Seq(items) => self.seq(items, pos, depth + 1, k),
            Node::Rep { node, min, max } => self.rep(node, *min, *max, 0, pos, depth + 1, k),
        }
    }

    fn seq(&self, items: &'a [Node], pos: usize, depth: u32, k: &mut Cont<'_>) -> bool {
        match items.split_first() {
            None => k(pos),
            Some((head, tail)) => self.node(head, pos, depth, &mut |p| self.seq(tail, p, depth, k)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn rep(
        &self,
        node: &'a Node,
        min: u32,
        max: Option<u32>,
        done: u32,
        pos: usize,
        depth: u32,
        k: &mut Cont<'_>,
    ) -> bool {
        if done >= min && k(pos) {
            return true;
        }
        if max.is_some_and(|m| done >= m) {
            return false;
        }
        self.node(node, pos, depth, &mut |p| {
            // A repetition that consumed nothing would loop forever.
            if p == pos {
                return false;
            }
            self.rep(node, min, max, done + 1, p, depth, k)
        })
    }
}
