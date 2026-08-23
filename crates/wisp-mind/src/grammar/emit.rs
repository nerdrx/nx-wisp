//! JSON Schema in, GBNF out.
//!
//! The supported subset is exactly what a tool manifest needs and nothing more:
//! `type` (including a list of types), `properties` / `required` /
//! `additionalProperties`, `items` / `minItems`, `enum`, `const`, `anyOf` /
//! `oneOf`, a shallow `allOf`, and `$ref` into `$defs` or `definitions`.
//! Anything else is a hard error rather than a silently loose grammar — a tool
//! whose arguments the decoder cannot be constrained to is a tool that should
//! not be offered to a 1.7B model at all.
//!
//! ## Optional properties
//!
//! The awkward part of schema-to-grammar, and the reason this is not a
//! twenty-line function. `{"a": 1}` and `{"b": 2}` and `{"a": 1, "b": 2}` must
//! all be reachable but `{,"b": 2}` must not, so the comma has to belong to the
//! *pair that follows it* and the first pair present has to be chosen
//! separately.
//!
//! It comes out as two chains of rules over the properties in schema order —
//! `head-k` ("the first pair present is at index ≥ k", no leading comma) and
//! `tail-k` ("everything from k on, each with its comma"). A required property
//! simply omits the skip branch in both, which means `required` is enforced
//! structurally: there is no derivation that reaches the closing brace without
//! it. Two rules per property rather than the factorial a free key order would
//! cost.

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use wisp_proto::ToolDescriptor;

use crate::error::{MindError, Result};

/// What shape of reply the model is being constrained to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrammarOptions {
    /// Also allow `{"say": "..."}` — a plain spoken reply — as an alternative
    /// to a tool call. SPEC §3.4: even her *speech* comes back as structured
    /// output, so `wisp-mind` never has free text to accidentally print.
    pub allow_say: bool,
    /// Allow `{"thought": "..."}` before the answer. Off by default: a reflex
    /// model's "thinking" is tokens the operator waits for and rarely value.
    pub allow_thought: bool,
    /// Cap on how many characters of string content the grammar permits, so a
    /// stuck decoder cannot emit a megabyte of `aaaa`. `None` is unbounded.
    pub max_string_chars: Option<u32>,
}

impl Default for GrammarOptions {
    fn default() -> Self {
        GrammarOptions {
            allow_say: true,
            allow_thought: false,
            max_string_chars: None,
        }
    }
}

impl GrammarOptions {
    /// Tool calls only — nothing else is a valid completion.
    pub const TOOL_ONLY: GrammarOptions = GrammarOptions {
        allow_say: false,
        allow_thought: false,
        max_string_chars: None,
    };
}

/// A tool call as the grammar shapes it. Parsing this back can never fail on
/// output that came from a constrained decoder, which is the entire point of
/// F14; it can still fail on output from somewhere else, so it returns a
/// `Result`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// The whole tool registry as one grammar.
///
/// The tool name and its argument schema are correlated by construction: each
/// alternative fixes the name to a literal *and* uses that tool's own argument
/// rule, so `{"name": "note_write", "arguments": {"stack": "vr"}}` is not
/// merely unlikely, it is not in the language.
pub fn tool_grammar<'a>(
    tools: impl IntoIterator<Item = &'a ToolDescriptor>,
    opts: &GrammarOptions,
) -> Result<String> {
    let mut b = Builder::new(opts.clone());
    let mut calls = Vec::new();

    for t in tools {
        let args = b.rule_for(&t.parameters, &Value::Null, &format!("{}-args", ident(t.name)))?;
        let name = format!("call-{}", ident(t.name));
        let body = format!(
            "\"{{\" ws \"\\\"name\\\"\" ws \":\" ws \"\\\"{}\\\"\" ws \",\" ws \"\\\"arguments\\\"\" ws \":\" ws {} ws \"}}\"",
            t.name, args
        );
        b.define(&name, &body);
        calls.push(name);
    }

    if calls.is_empty() && !opts.allow_say {
        return Err(MindError::Grammar(
            "a tool-only grammar with no tools would have no valid completion".into(),
        ));
    }

    let mut roots = Vec::new();
    if opts.allow_thought {
        b.need_string();
        b.define(
            "thought",
            "\"{\" ws \"\\\"thought\\\"\" ws \":\" ws string ws \"}\"",
        );
        roots.push("thought".to_string());
    }
    if opts.allow_say {
        b.need_string();
        b.define("say", "\"{\" ws \"\\\"say\\\"\" ws \":\" ws string ws \"}\"");
        roots.push("say".to_string());
    }
    if !calls.is_empty() {
        b.define("tool-call", &calls.join(" | "));
        roots.push("tool-call".to_string());
    }
    b.define("root", &roots.join(" | "));
    Ok(b.finish("root"))
}

/// Just `{"say": "..."}` — for a turn where no tool is on offer, either because
/// none are enabled or because the governor says she is only allowed a
/// one-liner.
pub fn reply_grammar(opts: &GrammarOptions) -> Result<String> {
    let none: [&ToolDescriptor; 0] = [];
    tool_grammar(
        none,
        &GrammarOptions {
            allow_say: true,
            ..opts.clone()
        },
    )
}

/// One JSON Schema as a complete grammar rooted at `root`.
pub fn schema_grammar(schema: &Value) -> Result<String> {
    let mut b = Builder::new(GrammarOptions::TOOL_ONLY);
    let r = b.rule_for(schema, schema, "value-0")?;
    b.define("root", &r);
    Ok(b.finish("root"))
}

/// The rule text for one schema, with the primitive rules it needs, for callers
/// composing something larger.
pub fn schema_rule(schema: &Value, hint: &str) -> Result<(String, String)> {
    let mut b = Builder::new(GrammarOptions::TOOL_ONLY);
    let r = b.rule_for(schema, schema, hint)?;
    let src = b.finish_without_root();
    Ok((r, src))
}

/// A closed set of quoted labels — the shape the escalation ladder's
/// self-assessment uses (F17). A model constrained to this cannot answer
/// "maybe".
pub fn enum_grammar(labels: &[&str]) -> Result<String> {
    if labels.is_empty() {
        return Err(MindError::Grammar("an enum grammar needs labels".into()));
    }
    let alts = labels
        .iter()
        .map(|l| format!("\"\\\"{}\\\"\"", escape_literal(l)))
        .collect::<Vec<_>>()
        .join(" | ");
    Ok(format!("root ::= {alts}\n"))
}

// ---------------------------------------------------------------------------
// The builder
// ---------------------------------------------------------------------------

struct Builder {
    rules: Vec<(String, String)>,
    index: BTreeMap<String, usize>,
    /// `$ref` path to the rule name it became, so a recursive schema produces a
    /// recursive grammar instead of an infinite one.
    refs: BTreeMap<String, String>,
    counter: u32,
    depth: u32,
    opts: GrammarOptions,
}

const MAX_SCHEMA_DEPTH: u32 = 24;

/// One property of an object schema, already reduced to the GBNF that matches
/// its `"key": value` pair.
struct Field {
    kv: String,
    required: bool,
}

impl Builder {
    fn new(opts: GrammarOptions) -> Self {
        let mut b = Builder {
            rules: Vec::new(),
            index: BTreeMap::new(),
            refs: BTreeMap::new(),
            counter: 0,
            depth: 0,
            opts,
        };
        b.define("ws", "[ \\t\\n\\r]*");
        b
    }

    fn define(&mut self, name: &str, body: &str) {
        match self.index.get(name) {
            Some(&i) => self.rules[i].1 = body.to_string(),
            None => {
                self.index.insert(name.to_string(), self.rules.len());
                self.rules.push((name.to_string(), body.to_string()));
            }
        }
    }

    fn has(&self, name: &str) -> bool {
        self.index.contains_key(name)
    }

    fn fresh(&mut self, hint: &str) -> String {
        let base = ident(hint);
        if !self.has(&base) {
            return base;
        }
        loop {
            self.counter += 1;
            let n = format!("{base}-{}", self.counter);
            if !self.has(&n) {
                return n;
            }
        }
    }

    // --- primitives, emitted only when something asks for them -------------

    fn need_string(&mut self) {
        if self.has("string") {
            return;
        }
        self.define("hex", "[0-9a-fA-F]");
        self.define(
            "char",
            "[^\"\\\\] | \"\\\\\" ([\"\\\\/bfnrt] | \"u\" hex hex hex hex)",
        );
        let body = match self.opts.max_string_chars {
            // GBNF has no `{m,n}`, so a cap is expressed as a chain of
            // optionals. Kept modest deliberately: this is a guard rail, not a
            // length policy.
            Some(n) if n <= 64 => {
                let mut s = String::from("\"\\\"\"");
                for _ in 0..n {
                    s.push_str(" char?");
                }
                s.push_str(" \"\\\"\"");
                s
            }
            _ => "\"\\\"\" char* \"\\\"\"".to_string(),
        };
        self.define("string", &body);
    }

    fn need_number(&mut self) {
        if self.has("number") {
            return;
        }
        self.need_integer();
        self.define(
            "number",
            "integer (\".\" [0-9]+)? ([eE] [-+]? [0-9]+)?",
        );
    }

    fn need_integer(&mut self) {
        if self.has("integer") {
            return;
        }
        self.define("integer", "\"-\"? (\"0\" | [1-9] [0-9]*)");
    }

    fn need_boolean(&mut self) {
        if !self.has("boolean") {
            self.define("boolean", "\"true\" | \"false\"");
        }
    }

    fn need_null(&mut self) {
        if !self.has("null") {
            self.define("null", "\"null\"");
        }
    }

    /// Any JSON value. Only reached by `additionalProperties: true` and by a
    /// schema that declares no type at all.
    fn need_value(&mut self) {
        if self.has("value") {
            return;
        }
        // Defined before recursing so the self-reference resolves.
        self.define("value", "object | array | string | number | boolean | null");
        self.need_string();
        self.need_number();
        self.need_boolean();
        self.need_null();
        self.define(
            "object",
            "\"{\" ws ( string ws \":\" ws value ( ws \",\" ws string ws \":\" ws value )* )? ws \"}\"",
        );
        self.define(
            "array",
            "\"[\" ws ( value ( ws \",\" ws value )* )? ws \"]\"",
        );
    }

    // --- the conversion ----------------------------------------------------

    /// Returns the *name* of a rule matching `schema`. Always a name, never an
    /// inline expression, so callers never have to think about precedence.
    fn rule_for(&mut self, schema: &Value, root: &Value, hint: &str) -> Result<String> {
        self.depth += 1;
        if self.depth > MAX_SCHEMA_DEPTH {
            self.depth -= 1;
            return Err(MindError::Schema {
                at: hint.to_string(),
                why: format!("nested more than {MAX_SCHEMA_DEPTH} deep"),
            });
        }
        let out = self.rule_for_inner(schema, root, hint);
        self.depth -= 1;
        out
    }

    fn rule_for_inner(&mut self, schema: &Value, root: &Value, hint: &str) -> Result<String> {
        // `true` / `false` schemas.
        match schema {
            Value::Bool(true) => {
                self.need_value();
                return Ok("value".to_string());
            }
            Value::Bool(false) => {
                return Err(MindError::Schema {
                    at: hint.to_string(),
                    why: "a `false` schema accepts nothing, so nothing could be generated".into(),
                })
            }
            Value::Object(_) => {}
            _ => {
                return Err(MindError::Schema {
                    at: hint.to_string(),
                    why: format!("expected an object or a boolean, got {schema}"),
                })
            }
        }
        let obj = schema.as_object().expect("checked above");

        if let Some(r) = obj.get("$ref").and_then(Value::as_str) {
            return self.resolve_ref(r, root, hint);
        }
        if let Some(c) = obj.get("const") {
            let name = self.fresh(hint);
            let body = format!("\"{}\"", escape_literal(&json_literal(c)));
            self.define(&name, &body);
            return Ok(name);
        }
        if let Some(Value::Array(vals)) = obj.get("enum") {
            if vals.is_empty() {
                return Err(MindError::Schema {
                    at: hint.to_string(),
                    why: "an empty enum accepts nothing".into(),
                });
            }
            let name = self.fresh(hint);
            let body = vals
                .iter()
                .map(|v| format!("\"{}\"", escape_literal(&json_literal(v))))
                .collect::<Vec<_>>()
                .join(" | ");
            self.define(&name, &body);
            return Ok(name);
        }
        for key in ["anyOf", "oneOf"] {
            if let Some(Value::Array(alts)) = obj.get(key) {
                if alts.is_empty() {
                    return Err(MindError::Schema {
                        at: hint.to_string(),
                        why: format!("an empty {key} accepts nothing"),
                    });
                }
                let mut names = Vec::new();
                for (i, a) in alts.iter().enumerate() {
                    names.push(self.rule_for(a, root, &format!("{hint}-{i}"))?);
                }
                let name = self.fresh(hint);
                self.define(&name, &names.join(" | "));
                return Ok(name);
            }
        }
        if let Some(Value::Array(parts)) = obj.get("allOf") {
            let merged = merge_all_of(parts, hint)?;
            return self.rule_for(&Value::Object(merged), root, hint);
        }

        // A `type` list is alternation over the same schema with one type each.
        if let Some(Value::Array(types)) = obj.get("type") {
            let mut names = Vec::new();
            for (i, t) in types.iter().enumerate() {
                let mut one = obj.clone();
                one.insert("type".into(), t.clone());
                names.push(self.rule_for(&Value::Object(one), root, &format!("{hint}-{i}"))?);
            }
            let name = self.fresh(hint);
            self.define(&name, &names.join(" | "));
            return Ok(name);
        }

        let ty = obj.get("type").and_then(Value::as_str);
        let looks_like_object = obj.contains_key("properties") || obj.contains_key("required");
        let looks_like_array = obj.contains_key("items");

        match ty {
            Some("object") => self.object_rule(obj, root, hint),
            Some("array") => self.array_rule(obj, root, hint),
            Some("string") => {
                self.need_string();
                Ok("string".to_string())
            }
            Some("integer") => {
                self.need_integer();
                Ok("integer".to_string())
            }
            Some("number") => {
                self.need_number();
                Ok("number".to_string())
            }
            Some("boolean") => {
                self.need_boolean();
                Ok("boolean".to_string())
            }
            Some("null") => {
                self.need_null();
                Ok("null".to_string())
            }
            Some(other) => Err(MindError::Schema {
                at: hint.to_string(),
                why: format!("unsupported type `{other}`"),
            }),
            None if looks_like_object => self.object_rule(obj, root, hint),
            None if looks_like_array => self.array_rule(obj, root, hint),
            None => {
                self.need_value();
                Ok("value".to_string())
            }
        }
    }

    fn resolve_ref(&mut self, r: &str, root: &Value, hint: &str) -> Result<String> {
        if let Some(existing) = self.refs.get(r) {
            return Ok(existing.clone());
        }
        let rest = r
            .strip_prefix("#/")
            .ok_or_else(|| MindError::Schema {
                at: hint.to_string(),
                why: format!("only local refs are supported, got `{r}`"),
            })?;
        let mut cur = root;
        for seg in rest.split('/') {
            let seg = seg.replace("~1", "/").replace("~0", "~");
            cur = cur.get(&seg).ok_or_else(|| MindError::Schema {
                at: hint.to_string(),
                why: format!("`{r}` does not resolve"),
            })?;
        }
        let target = cur.clone();
        let name = self.fresh(rest.rsplit('/').next().unwrap_or(hint));
        // Claim the name *before* recursing so a self-referential schema
        // terminates.
        self.refs.insert(r.to_string(), name.clone());
        self.define(&name, "\"\"");
        let inner = self.rule_for(&target, root, &format!("{name}-body"))?;
        self.define(&name, &inner);
        Ok(name)
    }

    fn array_rule(&mut self, obj: &Map<String, Value>, root: &Value, hint: &str) -> Result<String> {
        let items = obj.get("items").cloned().unwrap_or(Value::Bool(true));
        let item = self.rule_for(&items, root, &format!("{hint}-item"))?;
        let min = obj.get("minItems").and_then(Value::as_u64).unwrap_or(0);
        let name = self.fresh(hint);
        let inner = format!("{item} ( ws \",\" ws {item} )*");
        let body = if min == 0 {
            format!("\"[\" ws ( {inner} )? ws \"]\"")
        } else {
            format!("\"[\" ws {inner} ws \"]\"")
        };
        self.define(&name, &body);
        Ok(name)
    }

    fn object_rule(&mut self, obj: &Map<String, Value>, root: &Value, hint: &str) -> Result<String> {
        let empty = Map::new();
        let props = obj
            .get("properties")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let required: Vec<&str> = obj
            .get("required")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        // Properties keep **schema order**, required or not. The ordering is
        // fixed rather than free — permitting every permutation is factorial in
        // the number of keys and no model needs it — and it is schema order
        // specifically so that anything `serde_json` serialises from the same
        // schema is directly in the language. A grammar that rejected the
        // output of our own serialiser would be a trap.
        let mut fields: Vec<Field> = Vec::new();
        for (k, v) in props {
            let rule = self.rule_for(v, root, &format!("{hint}-{}", ident(k)))?;
            fields.push(Field {
                kv: format!("\"\\\"{}\\\"\" ws \":\" ws {}", escape_literal(k), rule),
                required: required.contains(&k.as_str()),
            });
        }
        for r in &required {
            if !props.contains_key(*r) {
                return Err(MindError::Schema {
                    at: hint.to_string(),
                    why: format!("`{r}` is required but has no schema"),
                });
            }
        }

        // `additionalProperties` defaults to false for a tool manifest: a tool
        // that accepts keys it never documented is a tool whose grammar cannot
        // help it.
        let additional = match obj.get("additionalProperties") {
            None | Some(Value::Bool(false)) => None,
            Some(Value::Bool(true)) => {
                self.need_value();
                Some("value".to_string())
            }
            Some(s) => Some(self.rule_for(s, root, &format!("{hint}-additional"))?),
        };
        if additional.is_some() {
            self.need_string();
        }

        let name = self.fresh(hint);
        let n = fields.len();

        // Two chains, both linear in the number of properties.
        //
        //   head-k  — "the first pair present is at index ≥ k", so no comma.
        //   tail-k  — "everything from index k on, each preceded by a comma."
        //
        // A required field's rules have no skip branch, which is the entire
        // enforcement of `required`: there is simply no derivation that reaches
        // the closing brace without it.
        //
        //   head-k ::= kv-k tail-(k+1)                      k required
        //   head-k ::= kv-k tail-(k+1) | head-(k+1)         k optional
        //   tail-k ::= ws "," ws kv-k tail-(k+1)            k required
        //   tail-k ::= ( ws "," ws kv-k tail-(k+1) ) | tail-(k+1)   k optional
        //
        // and the two ends of the chain carry `additionalProperties`, which is
        // where the "extra pairs come last" simplification lives.
        let (head_end, tail_end) = match &additional {
            None => (String::from("\"\""), String::from("\"\"")),
            Some(a) => (
                format!(
                    "( string ws \":\" ws {a} ( ws \",\" ws string ws \":\" ws {a} )* )?"
                ),
                format!("( ws \",\" ws string ws \":\" ws {a} )*"),
            ),
        };
        self.define(&format!("{name}-head{n}"), &head_end);
        self.define(&format!("{name}-tail{n}"), &tail_end);

        for k in (0..n).rev() {
            let f = &fields[k];
            let next_tail = format!("{name}-tail{}", k + 1);
            let next_head = format!("{name}-head{}", k + 1);
            let (head, tail) = if f.required {
                (
                    format!("{} {next_tail}", f.kv),
                    format!("ws \",\" ws {} {next_tail}", f.kv),
                )
            } else {
                (
                    format!("{} {next_tail} | {next_head}", f.kv),
                    format!("( ws \",\" ws {} {next_tail} ) | {next_tail}", f.kv),
                )
            };
            self.define(&format!("{name}-head{k}"), &head);
            self.define(&format!("{name}-tail{k}"), &tail);
        }

        self.define(&name, &format!("\"{{\" ws {name}-head0 ws \"}}\""));
        Ok(name)
    }

    fn finish(&self, root: &str) -> String {
        let mut out = String::new();
        // The root first, so a human reading the grammar starts where the
        // decoder does.
        if let Some(&i) = self.index.get(root) {
            out.push_str(&format!("{} ::= {}\n", self.rules[i].0, self.rules[i].1));
        }
        for (n, b) in &self.rules {
            if n == root {
                continue;
            }
            out.push_str(&format!("{n} ::= {b}\n"));
        }
        out
    }

    fn finish_without_root(&self) -> String {
        self.rules
            .iter()
            .map(|(n, b)| format!("{n} ::= {b}\n"))
            .collect()
    }
}

fn merge_all_of(parts: &[Value], hint: &str) -> Result<Map<String, Value>> {
    let mut props = Map::new();
    let mut required: Vec<Value> = Vec::new();
    for p in parts {
        let o = p.as_object().ok_or_else(|| MindError::Schema {
            at: hint.to_string(),
            why: "allOf members must be objects".into(),
        })?;
        if let Some(Value::Object(ps)) = o.get("properties") {
            for (k, v) in ps {
                props.insert(k.clone(), v.clone());
            }
        }
        if let Some(Value::Array(rs)) = o.get("required") {
            for r in rs {
                if !required.contains(r) {
                    required.push(r.clone());
                }
            }
        }
    }
    let mut merged = Map::new();
    merged.insert("type".into(), Value::String("object".into()));
    merged.insert("properties".into(), Value::Object(props));
    merged.insert("required".into(), Value::Array(required));
    Ok(merged)
}

/// A JSON value as the exact text a decoder would have to emit.
fn json_literal(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".to_string())
}

/// Escape a string for the inside of a GBNF `"..."` literal.
fn escape_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

/// A schema key or tool name as a legal GBNF rule name.
fn ident(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert_str(0, "r-");
    }
    out
}
