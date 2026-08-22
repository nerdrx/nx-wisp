//! **F14 — grammar-constrained tool calls.**
//!
//! > *The reflex model is small, so it is never asked to "please output JSON" —
//! > the decoder is constrained to a grammar generated from the tool registry.*
//!
//! The distinction matters. Asking a 1.7B model for JSON and parsing hopefully
//! gives you a retry loop and a failure mode the operator sees. Constraining
//! the decoder means a malformed tool call is not unlikely, it is *unreachable*:
//! at every step the sampler's logits are masked to the tokens the grammar can
//! still accept.
//!
//! This module builds the GBNF. It contains three things:
//!
//! * [`schema_rule`] / [`tool_grammar`] / [`reply_grammar`] — JSON Schema in,
//!   GBNF out.
//! * [`Grammar`] — a parser and matcher for the subset of GBNF we emit. This is
//!   what makes the unit tests real: instead of asserting on grammar *text*,
//!   the tests assert that the grammar **accepts** every valid instance of the
//!   schema and **rejects** the malformed ones.
//! * [`Grammar::shortest`] — the smallest string the grammar accepts, which is
//!   how [`crate::backend::mock::MockBackend`] emits grammar-valid output
//!   without a model. A mock that could emit something the real constrained
//!   decoder never could would be a mock that hides bugs.
//!
//! ## The subset
//!
//! We emit, and therefore parse: string literals, rule references, alternation,
//! grouping, `?`/`*`/`+`, and character classes with ranges and negation. No
//! `{m,n}` repetition, no lookahead, no `.`. Everything llama.cpp's own
//! `json.gbnf` uses, and nothing else.

mod emit;
mod parse;

pub use emit::{
    enum_grammar, reply_grammar, schema_grammar, schema_rule, tool_grammar, GrammarOptions,
    ToolCall,
};
pub use parse::{Grammar, Node};
