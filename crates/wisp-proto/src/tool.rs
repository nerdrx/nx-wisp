//! SPEC §3.7 — the shape of a tool.
//!
//! These types started life in `wisp-fleet` because the `nx` CLI wrappers were
//! the first tools. Then `wisp-mind` needed them for its registry, and SPEC §2
//! gives the mind proto and gov only — so the mind carried a whole fleet
//! dependency for four type definitions. The *shape* of a tool is contract,
//! not fleet behaviour; it lives here now, beside the `Consent` it enforces.
//! `wisp-fleet` keeps the actual `nx` tools and re-exports these for
//! compatibility.

use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::sense::Consent;

pub type ToolFuture = Pin<Box<dyn std::future::Future<Output = ToolOutcome> + Send>>;
pub type ToolFn = Arc<dyn Fn(Value) -> ToolFuture + Send + Sync>;

/// Everything `wisp-mind` needs to offer a tool to the model.
#[derive(Clone)]
pub struct ToolDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    /// SPEC §3.7. `Ambient` may run unprompted; `Explicit` must be enabled by
    /// the operator first. An `Invasive` tool additionally requires the
    /// visible tell while it runs.
    pub consent: Consent,
    /// JSON Schema for the arguments object.
    pub parameters: Value,
    pub invoke: ToolFn,
}

impl std::fmt::Debug for ToolDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolDescriptor")
            .field("name", &self.name)
            .field("consent", &self.consent)
            .field("parameters", &self.parameters)
            .finish_non_exhaustive()
    }
}

/// What came back. `unavailable` is deliberately distinct from failure: "NX
/// Hub is not installed" means nothing is wrong, there is just no fleet here —
/// and she should say so rather than apologise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutcome {
    pub ok: bool,
    pub unavailable: bool,
    /// One line she can actually say.
    pub summary: String,
    /// Structured output, when the tool produced any.
    pub json: Option<Value>,
    pub exit_code: Option<i32>,
}

impl ToolOutcome {
    pub fn success(summary: impl Into<String>, json: Option<Value>) -> Self {
        Self { ok: true, unavailable: false, summary: summary.into(), json, exit_code: Some(0) }
    }
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self { ok: false, unavailable: true, summary: reason.into(), json: None, exit_code: None }
    }
    pub fn failed(summary: impl Into<String>, exit_code: Option<i32>) -> Self {
        Self { ok: false, unavailable: false, summary: summary.into(), json: None, exit_code }
    }
}

/// One line of the flight recorder's tool trace. The binary turns these into
/// `EventKind::ToolCall`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolInvocation {
    pub name: String,
    /// The exact argv handed to the tool — not a paraphrase of it.
    pub argv: Vec<String>,
    pub ok: bool,
    pub unavailable: bool,
    pub detail: String,
}
