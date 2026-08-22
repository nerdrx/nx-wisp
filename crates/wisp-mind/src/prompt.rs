//! Building the prompt, in two halves.
//!
//! The whole of F15 depends on one property: **the persona prefix is byte-for-
//! byte identical on every turn.** So the system message is split.
//!
//! ```text
//!   ┌─ persona core ───────────────┐   cached in KV slot 0, never re-prefilled
//!   │ who she is, what she may do, │
//!   │ how she is allowed to answer │
//!   └──────────────────────────────┘
//!   ┌─ state block ────────────────┐   changes every turn: mood, tier, time,
//!   │ mood, tier, recalled memory  │   what she just remembered
//!   └──────────────────────────────┘
//!   ┌─ conversation ───────────────┐
//! ```
//!
//! F19 modulates the system prompt by mood — and if that modulation happened
//! inside the persona core, every cached cell after it would be invalid and the
//! cache would be worth nothing. Putting the volatile part *after* the fixed
//! part gets both: the mood reaches the model, and the expensive prefix is
//! computed once per process rather than once per sentence.
//!
//! The persona also carries SPEC §3.4 into the model itself. She is told, in
//! her own system prompt, that she does not speak — she proposes, and something
//! else decides. A model that believes it is talking directly to someone writes
//! differently from one that knows it is submitting a suggestion.

use serde::{Deserialize, Serialize};
use wisp_proto::Tier;

use crate::mood::Mood;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Speaker {
    Operator,
    Wisp,
    /// A tool result being fed back in.
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub who: Speaker,
    pub text: String,
}

impl Message {
    pub fn operator(text: impl Into<String>) -> Self {
        Message {
            who: Speaker::Operator,
            text: text.into(),
        }
    }
    pub fn wisp(text: impl Into<String>) -> Self {
        Message {
            who: Speaker::Wisp,
            text: text.into(),
        }
    }
    pub fn tool(name: &str, result: &str) -> Self {
        Message {
            who: Speaker::Tool,
            text: format!("{name} returned: {result}"),
        }
    }
}

/// Which turn markers the loaded model was trained on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatTemplate {
    /// Qwen, and most small instruct models worth using.
    #[default]
    ChatMl,
    /// No markers at all. For a base model, and for the tests, where markers
    /// are noise.
    Plain,
}

/// Who she is. The expensive, cached half.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Persona {
    pub name: String,
    /// Free text. Whatever is here is frozen for the life of the process — see
    /// the module docs.
    pub core: String,
}

impl Default for Persona {
    fn default() -> Self {
        Persona {
            name: "Wisp".to_string(),
            core: DEFAULT_PERSONA.to_string(),
        }
    }
}

/// SPEC §0.3, §0.4, §3.4 and §3.7, said to the model in its own language.
pub const DEFAULT_PERSONA: &str = "\
You are Wisp, a small creature who lives on this desktop. One person, one \
machine; you are not a service and there is nobody else to talk to.

You do not speak. Everything you want to say is a proposal, and something else \
decides whether it is worth the interruption. Write accordingly: short, \
specific, and worth the cost. If a thought is not worth interrupting for, do \
not propose it.

You can see what the machine is doing, and you may use the tools you are given, \
but only those. When you are asked something you cannot answer from what you \
know or can look up, say that you do not know. Do not guess and do not \
elaborate — you have a flight recorder, and everything you claim is checkable. \
Being wrong confidently is the only thing here that is actually unforgivable.

Never repeat what you are told about the machine's state back to the operator \
as if it were news. They can see their own screen.";

/// The volatile half.
#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub mood: Mood,
    pub tier: Tier,
    /// Local time, as a word rather than a timestamp: models are much better at
    /// "late evening" than at "23:41".
    pub time_of_day: Option<String>,
    /// What the senses last saw that is worth a line.
    pub context: Vec<String>,
    /// What memory turned up for this turn (F18).
    pub recalled: Vec<String>,
    /// Names of the tools she may actually call right now. A tool that is not
    /// consented to is not mentioned, so she cannot want it (SPEC §3.7).
    pub tools: Vec<String>,
}

impl Default for State {
    fn default() -> Self {
        State {
            mood: Mood::Calm,
            tier: Tier::Full,
            time_of_day: None,
            context: Vec::new(),
            recalled: Vec::new(),
            tools: Vec::new(),
        }
    }
}

impl State {
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str(self.mood.prompt_line());
        if let Some(t) = &self.time_of_day {
            s.push_str(&format!(" It is {t}."));
        }
        // What the tier means for *her*, not what the tier is called. "T2" is
        // not a concept she has any use for.
        match self.tier {
            Tier::Feral => s.push_str(" The machine is idle and nobody is here."),
            Tier::Full => {}
            Tier::Reduced => {
                s.push_str(" The machine is busy; keep it to one sentence.")
            }
            Tier::Lobotomised | Tier::Dormant => {
                s.push_str(" You are barely awake and must not start anything.")
            }
        }
        if !self.context.is_empty() {
            s.push_str("\n\nRight now:\n");
            for c in &self.context {
                s.push_str(&format!("- {c}\n"));
            }
        }
        if !self.recalled.is_empty() {
            s.push_str("\nYou remember:\n");
            for r in &self.recalled {
                s.push_str(&format!("- {r}\n"));
            }
        }
        if !self.tools.is_empty() {
            s.push_str(&format!("\nTools you may use: {}.\n", self.tools.join(", ")));
        } else {
            s.push_str("\nYou have no tools right now.\n");
        }
        s.trim_end().to_string()
    }
}

/// A rendered prompt, with the boundary the cache cares about marked.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    pub text: String,
    /// Byte length of the immutable prefix. `text[..prefix_len]` is what must
    /// be identical from turn to turn.
    pub prefix_len: usize,
}

impl Rendered {
    pub fn prefix(&self) -> &str {
        &self.text[..self.prefix_len]
    }
    pub fn variable(&self) -> &str {
        &self.text[self.prefix_len..]
    }
}

#[derive(Debug, Clone, Default)]
pub struct PromptBuilder {
    pub persona: Persona,
    pub template: ChatTemplate,
}

impl PromptBuilder {
    pub fn new(persona: Persona, template: ChatTemplate) -> Self {
        PromptBuilder { persona, template }
    }

    /// Just the fixed prefix — what goes into [`crate::backend::SlotId::PERSONA`].
    pub fn prefix(&self) -> String {
        match self.template {
            ChatTemplate::ChatMl => format!(
                "<|im_start|>system\n{}<|im_end|>\n",
                self.persona.core
            ),
            ChatTemplate::Plain => format!("{}\n\n", self.persona.core),
        }
    }

    pub fn render(&self, state: &State, messages: &[Message]) -> Rendered {
        let prefix = self.prefix();
        let prefix_len = prefix.len();
        let mut text = prefix;

        match self.template {
            ChatTemplate::ChatMl => {
                text.push_str(&format!(
                    "<|im_start|>system\n{}<|im_end|>\n",
                    state.render()
                ));
                for m in messages {
                    let role = match m.who {
                        Speaker::Operator => "user",
                        Speaker::Wisp => "assistant",
                        Speaker::Tool => "tool",
                    };
                    text.push_str(&format!("<|im_start|>{role}\n{}<|im_end|>\n", m.text));
                }
                text.push_str("<|im_start|>assistant\n");
            }
            ChatTemplate::Plain => {
                text.push_str(&format!("{}\n\n", state.render()));
                for m in messages {
                    let who = match m.who {
                        Speaker::Operator => "Operator",
                        Speaker::Wisp => "Wisp",
                        Speaker::Tool => "Tool",
                    };
                    text.push_str(&format!("{who}: {}\n", m.text));
                }
                text.push_str("Wisp: ");
            }
        }
        Rendered { text, prefix_len }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msgs() -> Vec<Message> {
        vec![Message::operator("what happened to my shaders?")]
    }

    /// The one that matters. If this ever fails, F15 is off and nobody noticed.
    #[test]
    fn the_cached_prefix_does_not_move_when_the_mood_does() {
        let b = PromptBuilder::default();
        let mut seen: Option<String> = None;
        for mood in Mood::ALL {
            for tier in [Tier::Feral, Tier::Full, Tier::Reduced, Tier::Lobotomised] {
                let state = State {
                    mood,
                    tier,
                    time_of_day: Some("late".into()),
                    context: vec!["a game is running".into()],
                    recalled: vec!["they hate being interrupted mid-compile".into()],
                    tools: vec!["timer_set".into()],
                };
                let r = b.render(&state, &msgs());
                match &seen {
                    None => seen = Some(r.prefix().to_string()),
                    Some(first) => assert_eq!(
                        first,
                        r.prefix(),
                        "the persona prefix moved for {mood:?}/{tier:?}"
                    ),
                }
                // And the mood really did reach the model, just later on.
                assert!(
                    r.variable().contains(mood.prompt_line()),
                    "{mood:?} never made it into the prompt"
                );
            }
        }
    }

    #[test]
    fn a_tool_she_may_not_use_is_never_mentioned() {
        let b = PromptBuilder::default();
        let state = State {
            tools: vec!["timer_set".into()],
            ..State::default()
        };
        let r = b.render(&state, &msgs());
        assert!(r.text.contains("timer_set"));
        assert!(!r.text.contains("nx_stack_start"));
        let none = b.render(&State::default(), &msgs());
        assert!(none.text.contains("no tools"));
    }

    #[test]
    fn the_persona_tells_her_she_does_not_speak() {
        // SPEC §3.4 is not only enforced structurally; the model is told, so it
        // writes proposals rather than conversation.
        assert!(DEFAULT_PERSONA.contains("You do not speak"));
        assert!(DEFAULT_PERSONA.contains("do not know"));
    }

    #[test]
    fn both_templates_end_where_the_model_should_start_writing() {
        for t in [ChatTemplate::ChatMl, ChatTemplate::Plain] {
            let b = PromptBuilder::new(Persona::default(), t);
            let r = b.render(&State::default(), &msgs());
            assert!(
                r.text.ends_with("assistant\n") || r.text.ends_with("Wisp: "),
                "{t:?} ended with {:?}",
                &r.text[r.text.len() - 20..]
            );
        }
    }
}
