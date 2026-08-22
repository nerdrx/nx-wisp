//! **F16 — the tool registry**, and SPEC §3.7's consent, enforced.
//!
//! > `Ambient` tools and senses may run unprompted. `Explicit` require the
//! > operator to have enabled them. `Invasive` (mic, clipboard, screen)
//! > additionally require the visible tell of §0.3 while active. Defaults ship
//! > as: ambient on, explicit off, invasive off.
//!
//! ## Three gates, in order
//!
//! 1. **Does it exist.** A name the registry does not know is refused, and the
//!    refusal is recorded like any other call.
//! 2. **Is it consented to.** `Explicit` and `Invasive` must have been switched
//!    on by the operator; `Invasive` additionally refuses unless a visible tell
//!    is wired, because SPEC §0.3 is not satisfied by intent.
//! 3. **Are the arguments the shape the tool declared.**
//!
//! The third gate is the interesting one: **the validator is the grammar.** A
//! tool is registered by compiling its JSON Schema to GBNF
//! ([`crate::grammar`]), and arguments are checked by asking that grammar
//! whether it would accept them. So the thing the decoder is constrained to and
//! the thing the registry accepts are, by construction, the same language —
//! there is no second implementation of the schema to drift out of step. A tool
//! whose schema cannot be compiled cannot be registered at all, because offering
//! it would silently mean offering it *unconstrained*.
//!
//! Every call, refusal included, becomes an [`EventKind::ToolCall`]. SPEC §0.4:
//! "why did you do that?" is answerable from data, and "why did you *not*" is a
//! question with the same claim on the truth.
//!
//! Nothing here speaks. A tool returns a [`ToolOutcome`] with a one-line summary
//! and `wisp-mind`'s caller decides whether that becomes an
//! [`wisp_proto::Utterance`] — which `wisp-attn` then decides about in turn
//! (SPEC §3.4).

pub mod builtin;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use wisp_proto::{Consent, EventKind};

// The descriptor type is `wisp-fleet`'s, not a copy of it. See the crate docs'
// note on the SPEC §2 dependency edge that costs.
pub use wisp_fleet::tools::{ToolDescriptor, ToolFn, ToolFuture, ToolInvocation, ToolOutcome};

use crate::error::{MindError, Result};
use crate::events::EventSink;
use crate::grammar::{tool_grammar, Grammar, GrammarOptions};

/// A tool plus the compiled grammar for its arguments.
pub struct Registered {
    pub descriptor: ToolDescriptor,
    /// The language of this tool's argument objects. Used both to constrain the
    /// decoder and to validate what comes back.
    grammar: Grammar,
}

impl std::fmt::Debug for Registered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registered")
            .field("name", &self.descriptor.name)
            .field("consent", &self.descriptor.consent)
            .finish()
    }
}

/// Shown on the character herself while an `Invasive` tool is running
/// (SPEC §0.3). `wisp-mind` cannot draw, so this is a callback the binary wires
/// to the rig.
pub type Tell = Arc<dyn Fn(&str, bool) + Send + Sync>;

#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Registered>,
    enabled: BTreeSet<String>,
    events: EventSink,
    state_path: Option<PathBuf>,
    tell: Option<Tell>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .field("enabled", &self.enabled)
            .field("tell", &self.tell.is_some())
            .finish()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        ToolRegistry::default()
    }

    pub fn with_events(mut self, events: EventSink) -> Self {
        self.events = events;
        self
    }

    /// Where the operator's Explicit-tool choices are remembered. Under
    /// `NX_WISP_CONFIG_DIR` by default, so a test can never turn something on
    /// in the operator's real profile (SPEC §4).
    pub fn with_state_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.state_path = Some(path.into());
        self
    }

    pub fn default_state_file() -> PathBuf {
        crate::dirs::config_dir().join("mind").join("tools.json")
    }

    /// Wire the visible tell. Until this is set, no `Invasive` tool will run,
    /// whatever the operator has enabled.
    pub fn with_tell(mut self, tell: Tell) -> Self {
        self.tell = Some(tell);
        self
    }

    /// Add a tool.
    ///
    /// Fails if its schema cannot be compiled to a grammar. That is deliberate
    /// and strict: a tool that cannot constrain its own decoder is a tool that
    /// would be reliable only by luck, and F14 exists precisely to not depend
    /// on luck.
    pub fn register(&mut self, descriptor: ToolDescriptor) -> Result<()> {
        if self.tools.contains_key(descriptor.name) {
            return Err(MindError::BadArguments {
                name: descriptor.name.to_string(),
                why: "a tool with this name is already registered".into(),
            });
        }
        let src = tool_grammar(std::iter::once(&descriptor), &GrammarOptions::TOOL_ONLY)?;
        let whole = Grammar::parse(&src)?;
        // The rule for *this* tool's arguments, so validation does not have to
        // wrap the value in a fake call.
        let args_rule = format!("{}-args", ident(descriptor.name));
        if whole.rule(&args_rule).is_none() {
            return Err(MindError::Grammar(format!(
                "{}: the argument rule went missing from its own grammar",
                descriptor.name
            )));
        }
        self.tools.insert(
            descriptor.name.to_string(),
            Registered {
                descriptor,
                grammar: whole,
            },
        );
        Ok(())
    }

    pub fn register_all(
        &mut self,
        tools: impl IntoIterator<Item = ToolDescriptor>,
    ) -> Vec<(String, Result<()>)> {
        tools
            .into_iter()
            .map(|d| {
                let name = d.name.to_string();
                (name, self.register(d))
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }
    pub fn get(&self, name: &str) -> Option<&ToolDescriptor> {
        self.tools.get(name).map(|r| &r.descriptor)
    }
    pub fn all(&self) -> impl Iterator<Item = &ToolDescriptor> {
        self.tools.values().map(|r| &r.descriptor)
    }

    /// Only the ones she may actually call right now. This is what goes into
    /// the prompt and into the grammar: a tool she may not use is never
    /// mentioned to her, so she cannot want it and cannot be refused in front
    /// of the operator.
    pub fn available(&self) -> Vec<&ToolDescriptor> {
        self.tools
            .values()
            .map(|r| &r.descriptor)
            .filter(|d| self.consent_ok(d).is_ok())
            .collect()
    }

    pub fn is_enabled(&self, name: &str) -> bool {
        self.enabled.contains(name)
    }

    /// Switch an `Explicit` or `Invasive` tool on or off. Ambient tools are
    /// always on and cannot be "enabled"; asking to is not an error, it is a
    /// no-op with a `false` return.
    pub fn enable(&mut self, name: &str, on: bool) -> Result<bool> {
        let d = self
            .tools
            .get(name)
            .map(|r| &r.descriptor)
            .ok_or_else(|| MindError::NoSuchTool(name.to_string()))?;
        if d.consent == Consent::Ambient {
            return Ok(false);
        }
        let changed = if on {
            self.enabled.insert(name.to_string())
        } else {
            self.enabled.remove(name)
        };
        if changed {
            self.save();
        }
        Ok(changed)
    }

    /// Everything the operator has switched on, for the consent panel.
    pub fn enabled_names(&self) -> Vec<String> {
        self.enabled.iter().cloned().collect()
    }

    /// SPEC §3.7, as one function.
    pub fn consent_ok(&self, d: &ToolDescriptor) -> Result<()> {
        match d.consent {
            Consent::Ambient => Ok(()),
            Consent::Explicit | Consent::Invasive => {
                if !self.enabled.contains(d.name) {
                    return Err(MindError::ConsentRequired {
                        name: d.name.to_string(),
                        consent: d.consent,
                    });
                }
                // SPEC §0.3: an invasive tool with no visible tell is not
                // permitted, however enthusiastically it was enabled. Intent is
                // not a tell.
                if d.consent == Consent::Invasive && self.tell.is_none() {
                    return Err(MindError::ConsentRequired {
                        name: d.name.to_string(),
                        consent: d.consent,
                    });
                }
                Ok(())
            }
        }
    }

    /// Would the constrained decoder have been able to produce these arguments?
    pub fn validate(&self, name: &str, args: &Value) -> Result<()> {
        let r = self
            .tools
            .get(name)
            .ok_or_else(|| MindError::NoSuchTool(name.to_string()))?;
        // `serde_json::Map` is a `BTreeMap`, so this is canonical key order —
        // the same order the grammar emits properties in.
        let text = serde_json::to_string(args)?;
        let rule = format!("{}-args", ident(name));
        if r.grammar.accepts_rule(&rule, &text) {
            return Ok(());
        }
        Err(MindError::BadArguments {
            name: name.to_string(),
            why: format!("{text} is not the shape this tool declared"),
        })
    }

    /// The GBNF for everything she may currently call.
    pub fn grammar(&self, opts: &GrammarOptions) -> Result<String> {
        tool_grammar(self.available(), opts)
    }

    /// Call a tool. All three gates, then the tool, then the record.
    pub async fn invoke(&self, name: &str, args: Value) -> Result<ToolOutcome> {
        let d = match self.tools.get(name) {
            Some(r) => r.descriptor.clone(),
            None => {
                self.record(name, &args, false);
                return Err(MindError::NoSuchTool(name.to_string()));
            }
        };
        if let Err(e) = self.consent_ok(&d) {
            self.record(name, &args, false);
            return Err(e);
        }
        if let Err(e) = self.validate(name, &args) {
            self.record(name, &args, false);
            return Err(e);
        }

        // SPEC §0.3: the tell goes up *before* anything invasive runs and comes
        // down after, whatever happened in between.
        let invasive = d.consent == Consent::Invasive;
        if invasive {
            if let Some(t) = &self.tell {
                t(d.name, true);
            }
            self.events.emit(EventKind::InvasiveActive {
                sense: wisp_proto::SenseId::Screen,
                active: true,
            });
        }
        let outcome = (d.invoke)(args.clone()).await;
        if invasive {
            if let Some(t) = &self.tell {
                t(d.name, false);
            }
            self.events.emit(EventKind::InvasiveActive {
                sense: wisp_proto::SenseId::Screen,
                active: false,
            });
        }

        self.record(name, &args, outcome.ok);
        Ok(outcome)
    }

    /// The same, but a refusal comes back as a [`ToolOutcome`] she could
    /// actually say rather than as an error to be formatted somewhere else.
    pub async fn invoke_or_excuse(&self, name: &str, args: Value) -> ToolOutcome {
        match self.invoke(name, args).await {
            Ok(o) => o,
            Err(e) => ToolOutcome {
                ok: false,
                // A tool that exists but is switched off is not broken; it is
                // simply not available to her, which is the same shape of
                // outcome as `nx` not being installed.
                unavailable: matches!(e, MindError::ConsentRequired { .. }),
                summary: e.to_string(),
                json: None,
                exit_code: None,
            },
        }
    }

    fn record(&self, name: &str, args: &Value, ok: bool) {
        self.events.emit(EventKind::ToolCall {
            name: name.to_string(),
            args: args.to_string(),
            ok,
        });
    }

    // --- persistence -------------------------------------------------------

    /// Read the operator's choices back. Unknown names are kept: a tool may be
    /// registered later in the same run, and silently dropping the operator's
    /// consent would be worse than carrying a name that means nothing yet.
    pub fn load(&mut self) -> Result<()> {
        let Some(path) = self.state_path.clone() else {
            return Ok(());
        };
        if !path.exists() {
            return Ok(());
        }
        let text = std::fs::read_to_string(&path).map_err(|e| MindError::io(&path, e))?;
        let v: Value = serde_json::from_str(&text)?;
        if let Some(list) = v.get("enabled").and_then(Value::as_array) {
            self.enabled = list
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
        }
        Ok(())
    }

    fn save(&self) {
        let Some(path) = &self.state_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let body = serde_json::json!({ "enabled": self.enabled });
        // Best effort. Losing a preference is a nuisance; failing a tool call
        // because a preference could not be written would be worse.
        if let Err(e) = write_atomic(path, body.to_string().as_bytes()) {
            tracing::warn!(path = %path.display(), error = %e, "could not save tool consent");
        }
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Same transformation [`crate::grammar`] uses for rule names.
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

/// Build a [`ToolDescriptor`] from a synchronous closure — which is what almost
/// every local tool actually is.
pub fn sync_tool<F>(
    name: &'static str,
    description: &'static str,
    consent: Consent,
    parameters: Value,
    f: F,
) -> ToolDescriptor
where
    F: Fn(Value) -> ToolOutcome + Send + Sync + 'static,
{
    let f = Arc::new(f);
    ToolDescriptor {
        name,
        description,
        consent,
        parameters,
        invoke: Arc::new(move |args| {
            let f = Arc::clone(&f);
            Box::pin(async move { f(args) })
        }),
    }
}

/// The ordinary "it worked, here is a sentence" outcome.
pub fn ok(summary: impl Into<String>) -> ToolOutcome {
    ToolOutcome {
        ok: true,
        unavailable: false,
        summary: summary.into(),
        json: None,
        exit_code: None,
    }
}

pub fn ok_with(summary: impl Into<String>, json: Value) -> ToolOutcome {
    ToolOutcome {
        json: Some(json),
        ..ok(summary)
    }
}

pub fn failed(summary: impl Into<String>) -> ToolOutcome {
    ToolOutcome {
        ok: false,
        unavailable: false,
        summary: summary.into(),
        json: None,
        exit_code: None,
    }
}

/// Not an error: the thing this tool talks to is not on this machine.
pub fn unavailable(summary: impl Into<String>) -> ToolOutcome {
    ToolOutcome {
        ok: false,
        unavailable: true,
        summary: summary.into(),
        json: None,
        exit_code: None,
    }
}
