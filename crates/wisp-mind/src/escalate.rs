//! **F17 — the escalation ladder.**
//!
//! > *reflex → deliberate → (opt-in) Claude Code CLI as the "big brain" for hard
//! > asks. The wisp knows when it is out of its depth and says so instead of
//! > hallucinating.*
//!
//! The second sentence is the feature. The ladder is easy; **knowing when to
//! stop** is the part that decides whether she is trustworthy. SPEC §0.4 says
//! the flight recorder holds the real trace and that everything she says is
//! checkable — a companion that answers confidently from nothing fails that on
//! the first interesting question.
//!
//! So there is no rung below "I don't know". [`Verdict::OutOfDepth`] is a
//! first-class outcome, it carries an [`Utterance`] she can actually submit, and
//! the top rung being unavailable produces it rather than a fallback to
//! guessing.
//!
//! ## Two judgements, not one
//!
//! [`triage`] is a pure, cheap heuristic over the text: long, multi-clause,
//! code-shaped, "why"-shaped asks start higher. It costs nothing and it is
//! deterministic, so "why did she use the big model?" is answerable.
//!
//! [`self_assessment_grammar`] is the model's own opinion, taken with a
//! grammar-constrained single token (`"answer"` / `"escalate"` / `"unsure"`).
//! A 1.7B model is not much good at answering a hard question but it is
//! surprisingly good at *recognising* one, and constraining the decoder to three
//! labels means the answer cannot be a paragraph explaining that it will now
//! try its best.
//!
//! ## The CLI hop
//!
//! `Consent::Explicit`, off by default, and **silent when absent**. `claude` not
//! being installed is the ordinary case, not an error path — the same shape as
//! `wisp-fleet` treating a missing `nx`. It is also the only thing in this crate
//! besides model downloads that can reach the network, which is why it is behind
//! consent: SPEC §0.2c allows "tools the operator explicitly enabled", and this
//! is one.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use wisp_proto::{Consent, Urgency, Utterance};

use crate::error::{MindError, Result};
use crate::grammar::enum_grammar;
use crate::tools::{failed, ok_with, unavailable, ToolDescriptor, ToolOutcome};

/// Where a question can be answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rung {
    /// The resident 1.7B. Routing, one-liners, "what time is it".
    Reflex,
    /// The 30B MoE. Real conversation, and anything that needs a second thought.
    Deliberate,
    /// The Claude Code CLI. Opt-in, absent by default, and never assumed.
    BigBrain,
}

impl Rung {
    pub const ALL: [Rung; 3] = [Rung::Reflex, Rung::Deliberate, Rung::BigBrain];

    pub fn as_str(self) -> &'static str {
        match self {
            Rung::Reflex => "reflex",
            Rung::Deliberate => "deliberate",
            Rung::BigBrain => "the big brain",
        }
    }

    pub fn next(self) -> Option<Rung> {
        match self {
            Rung::Reflex => Some(Rung::Deliberate),
            Rung::Deliberate => Some(Rung::BigBrain),
            Rung::BigBrain => None,
        }
    }
}

/// What she was asked.
#[derive(Debug, Clone, PartialEq)]
pub struct Ask {
    pub text: String,
    /// The operator asked, as opposed to her wondering something at herself.
    /// Only an operator's question is ever worth the big brain.
    pub from_operator: bool,
    /// Whether tools are on the table for this turn.
    pub allow_tools: bool,
}

impl Ask {
    pub fn from_operator(text: impl Into<String>) -> Self {
        Ask {
            text: text.into(),
            from_operator: true,
            allow_tools: true,
        }
    }
    pub fn her_own(text: impl Into<String>) -> Self {
        Ask {
            text: text.into(),
            from_operator: false,
            allow_tools: true,
        }
    }
}

/// [`triage`]'s verdict, with the reason kept so it can be recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct Triage {
    pub rung: Rung,
    pub complexity: f32,
    pub why: &'static str,
}

/// Markers that a question wants reasoning rather than recall.
const HARD_WORDS: [&str; 14] = [
    "why", "how come", "explain", "design", "compare", "trade-off", "tradeoff", "debug",
    "refactor", "prove", "derive", "architecture", "step by step", "root cause",
];

/// Markers that something technical is being pasted in.
const CODE_MARKERS: [&str; 8] = [
    "```", "panicked at", "stack trace", "backtrace", "error[e", "segfault",
    "undefined reference", "traceback",
];

/// A pure, cheap first opinion. No model, no clock, no allocation worth
/// mentioning — it runs before she has decided whether to wake anything up.
pub fn triage(ask: &Ask) -> Triage {
    let text = ask.text.trim();
    let lower = text.to_lowercase();
    let words = text.split_whitespace().count();

    let mut score = 0.0f32;
    let mut why = "short and ordinary";

    // Length. A long question is usually a hard one, but with diminishing
    // returns — somebody venting is not asking for the 30B.
    score += (words as f32 / 60.0).min(0.35);
    if words > 25 {
        why = "a long question";
    }

    if CODE_MARKERS.iter().any(|m| lower.contains(m)) {
        score += 0.45;
        why = "there is code or an error in it";
    }
    if HARD_WORDS.iter().any(|m| lower.contains(m)) {
        score += 0.3;
        if !lower.contains("```") {
            why = "it asks for reasoning, not a fact";
        }
    }
    // Several questions at once, or several clauses.
    let questions = text.matches('?').count();
    if questions > 1 {
        score += 0.15;
        why = "there is more than one question in it";
    }
    if text.matches(['.', ';']).count() > 3 {
        score += 0.1;
    }
    // Her own idle wondering never earns the expensive rungs, however
    // elaborately she phrases it to herself.
    if !ask.from_operator {
        score = score.min(0.3);
        why = "she is only thinking to herself";
    }

    let complexity = score.clamp(0.0, 1.0);
    let rung = if complexity < 0.35 {
        Rung::Reflex
    } else if complexity < 0.75 {
        Rung::Deliberate
    } else {
        Rung::BigBrain
    };
    Triage {
        rung,
        complexity,
        why,
    }
}

/// The three things the reflex model is allowed to say about its own ability.
pub const SELF_ASSESSMENT: [&str; 3] = ["answer", "escalate", "unsure"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelfAssessment {
    /// It believes it can answer.
    Answer,
    /// It believes something bigger should.
    Escalate,
    /// It does not know whether it knows. Treated as `Escalate`, because an
    /// uncertain small model is exactly the situation that produces confident
    /// nonsense.
    Unsure,
}

impl SelfAssessment {
    pub fn parse(s: &str) -> Option<SelfAssessment> {
        match s.trim().trim_matches('"') {
            "answer" => Some(SelfAssessment::Answer),
            "escalate" => Some(SelfAssessment::Escalate),
            "unsure" => Some(SelfAssessment::Unsure),
            _ => None,
        }
    }
    pub fn wants_escalation(self) -> bool {
        !matches!(self, SelfAssessment::Answer)
    }
}

/// The grammar for the self-assessment turn. Three labels, nothing else in the
/// language, so "I'll do my best!" is not a reachable output.
pub fn self_assessment_grammar() -> Result<String> {
    enum_grammar(&SELF_ASSESSMENT)
}

/// The prompt for it. Deliberately not the persona prompt: this is a
/// classification, and the persona would only make it chattier.
pub fn self_assessment_prompt(ask: &Ask) -> String {
    format!(
        "Decide whether you can answer this yourself.\n\n\
         Reply \"answer\" only if you are confident you know, from what you have \
         been told or from a tool you can call. Reply \"escalate\" if it needs \
         more thought than you can give it. Reply \"unsure\" if you cannot tell.\n\n\
         Question: {}\n\nDecision: ",
        ask.text.trim()
    )
}

/// The end of a turn.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Answered {
        rung: Rung,
        text: String,
    },
    /// She got to the top of what she has and it was not enough. This is a
    /// *success*: SPEC §0.4's honesty is worth more than an answer.
    OutOfDepth {
        tried: Vec<Rung>,
        why: String,
    },
}

impl Verdict {
    /// What she would propose saying. Still only a proposal — SPEC §3.4 gives
    /// the decision to `wisp-attn`.
    pub fn utterance(&self) -> Utterance {
        match self {
            Verdict::Answered { text, .. } => Utterance::new(text.clone(), Urgency::Answer),
            Verdict::OutOfDepth { why, .. } => Utterance::new(why.clone(), Urgency::Answer),
        }
    }
}

/// The sentence she says when she has run out of ladder. One of a small fixed
/// set, chosen by what was actually available, and never padded with a guess.
pub fn out_of_depth(tried: &[Rung], big_brain_available: bool) -> Verdict {
    let why = if tried.contains(&Rung::BigBrain) {
        "I asked the big brain and I still do not have an answer for that."
    } else if !big_brain_available {
        "That is past what I can work out here. I could ask Claude Code, but it \
         is not switched on — or not installed."
    } else if tried.contains(&Rung::Deliberate) {
        "I have thought about that properly and I do not know."
    } else {
        "I do not know."
    };
    Verdict::OutOfDepth {
        tried: tried.to_vec(),
        why: why.to_string(),
    }
}

// ---------------------------------------------------------------------------
// The big brain
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ClaudeCli {
    path: PathBuf,
    timeout: Duration,
}

impl Default for ClaudeCli {
    fn default() -> Self {
        ClaudeCli::new(default_claude_path())
    }
}

/// `$NX_WISP_CLAUDE_BIN`, else `~/.local/bin/claude`, else bare `claude` for
/// whatever is on `PATH`. Never searched for beyond that: a "big brain" that
/// found itself somewhere unexpected would be a surprising thing to have
/// enabled.
pub fn default_claude_path() -> PathBuf {
    if let Some(p) = std::env::var_os("NX_WISP_CLAUDE_BIN") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let local = crate::dirs::home().join(".local").join("bin").join("claude");
    if local.is_file() {
        return local;
    }
    PathBuf::from("claude")
}

impl ClaudeCli {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        ClaudeCli {
            path: path.into(),
            timeout: Duration::from_secs(120),
        }
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    pub fn binary(&self) -> &Path {
        &self.path
    }

    /// Is it there? Re-checked on every call, because it may be installed while
    /// she is running.
    pub fn available(&self) -> bool {
        if self.path.is_absolute() || self.path.components().count() > 1 {
            return self.path.is_file();
        }
        // A bare name: look along PATH ourselves rather than discovering the
        // answer by spawning and failing.
        std::env::var_os("PATH")
            .map(|paths| {
                std::env::split_paths(&paths).any(|d| d.join(&self.path).is_file())
            })
            .unwrap_or(false)
    }

    /// Ask it. Never errors on absence — that is an outcome, not a failure.
    pub async fn ask(&self, prompt: &str) -> ToolOutcome {
        if !self.available() {
            return unavailable(
                "Claude Code is not installed here, so there is nothing bigger for me to ask."
                    .to_string(),
            );
        }
        let mut cmd = tokio::process::Command::new(&self.path);
        cmd.arg("-p")
            .arg(prompt)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env("NO_COLOR", "1")
            .kill_on_drop(true);

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return unavailable(
                    "Claude Code is not installed here, so there is nothing bigger for me to ask."
                        .to_string(),
                )
            }
            Err(e) => return failed(format!("I could not start Claude Code: {e}")),
        };

        let output = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return failed(format!("Claude Code failed: {e}")),
            Err(_) => return failed("Claude Code took too long and I gave up on it.".to_string()),
        };
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let line = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            return failed(if line.is_empty() {
                format!("Claude Code exited {}.", output.status.code().unwrap_or(-1))
            } else {
                line.to_string()
            });
        }
        if stdout.is_empty() {
            return failed("Claude Code had nothing to say.".to_string());
        }
        ok_with(stdout.clone(), json!({ "answer": stdout }))
    }

    /// As a registrable tool, so the operator's consent panel has one row for
    /// it and the flight recorder records every hop.
    pub fn descriptor(&self) -> ToolDescriptor {
        let me = self.clone();
        ToolDescriptor {
            name: "big_brain",
            description: "Ask Claude Code — a much larger model, running locally as a CLI — \
                          a question you cannot answer yourself. Slow and not always \
                          available. Only for things you have genuinely tried and failed.",
            // SPEC §3.7 and §0.2c: this leaves the machine, so the operator has
            // to have said yes.
            consent: Consent::Explicit,
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string"}
                },
                "required": ["question"],
                "additionalProperties": false
            }),
            invoke: std::sync::Arc::new(move |args| {
                let me = me.clone();
                Box::pin(async move {
                    let q = args
                        .get("question")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    if q.is_empty() {
                        return failed("what should I ask it?");
                    }
                    me.ask(&q).await
                })
            }),
        }
    }
}

/// Which rungs exist right now, given what is loaded and what is consented to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Available {
    pub reflex: bool,
    pub deliberate: bool,
    pub big_brain: bool,
}

impl Available {
    pub fn has(&self, r: Rung) -> bool {
        match r {
            Rung::Reflex => self.reflex,
            Rung::Deliberate => self.deliberate,
            Rung::BigBrain => self.big_brain,
        }
    }
    /// The highest rung at or above `want` that actually exists; failing that,
    /// the highest that exists at all.
    pub fn resolve(&self, want: Rung) -> Option<Rung> {
        let mut r = Some(want);
        while let Some(x) = r {
            if self.has(x) {
                return Some(x);
            }
            r = x.next();
        }
        // Nothing above; fall back to anything below.
        Rung::ALL.iter().rev().copied().find(|x| self.has(*x))
    }
    pub fn none() -> Self {
        Available {
            reflex: false,
            deliberate: false,
            big_brain: false,
        }
    }
}

/// Everything F17 needs that is not a model call, in one place.
#[derive(Debug, Clone)]
pub struct Ladder {
    pub cli: ClaudeCli,
    /// Mirrors the registry's consent for `big_brain`. Off by default.
    pub big_brain_enabled: bool,
}

// Written out rather than derived on purpose: SPEC §3.7 says `Explicit` ships
// disabled, and that should read as a decision somebody made rather than as a
// property of `bool`.
#[allow(clippy::derivable_impls)]
impl Default for Ladder {
    fn default() -> Self {
        Ladder {
            cli: ClaudeCli::default(),
            big_brain_enabled: false,
        }
    }
}

impl Ladder {
    pub fn available(&self, reflex: bool, deliberate: bool) -> Available {
        Available {
            reflex,
            deliberate,
            big_brain: self.big_brain_enabled && self.cli.available(),
        }
    }

    /// Where to start, given the ask and what exists. Pure.
    pub fn start_at(&self, ask: &Ask, available: Available) -> Result<(Rung, Triage)> {
        let t = triage(ask);
        match available.resolve(t.rung) {
            Some(r) => Ok((r, t)),
            None => Err(MindError::NotLoaded(crate::backend::Role::Reflex)),
        }
    }

    /// She has run out of ladder.
    pub fn give_up(&self, tried: &[Rung]) -> Verdict {
        out_of_depth(tried, self.big_brain_enabled && self.cli.available())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_one_liner_stays_on_the_small_model() {
        for q in [
            "what time is it",
            "set a timer for ten minutes",
            "is the build done",
        ] {
            let t = triage(&Ask::from_operator(q));
            assert_eq!(t.rung, Rung::Reflex, "{q} -> {t:?}");
        }
    }

    #[test]
    fn a_pasted_panic_goes_straight_past_the_reflex_model() {
        let t = triage(&Ask::from_operator(
            "why does this happen\n```\nthread 'main' panicked at src/lib.rs:12\n```",
        ));
        assert!(t.rung >= Rung::Deliberate, "{t:?}");
        assert!(t.complexity > 0.5, "{t:?}");
    }

    #[test]
    fn her_own_idle_wondering_never_earns_the_big_brain() {
        let elaborate = "why does the shader cache keep recompiling, and how would I \
                         even debug that, and is it the driver? Explain the root cause \
                         step by step. ```log``` panicked at";
        let operator = triage(&Ask::from_operator(elaborate));
        let herself = triage(&Ask::her_own(elaborate));
        assert_eq!(operator.rung, Rung::BigBrain);
        assert_eq!(herself.rung, Rung::Reflex);
    }

    #[test]
    fn the_grammar_leaves_no_room_to_hedge() {
        let g = crate::grammar::Grammar::parse(&self_assessment_grammar().expect("grammar"))
            .expect("parses");
        assert!(g.accepts("\"escalate\""));
        assert!(g.accepts("\"unsure\""));
        assert!(!g.accepts("\"I will try my best\""));
        assert_eq!(
            SelfAssessment::parse("\"unsure\""),
            Some(SelfAssessment::Unsure)
        );
        assert!(SelfAssessment::Unsure.wants_escalation());
    }

    #[test]
    fn a_missing_cli_degrades_silently_and_honestly() {
        let l = Ladder {
            cli: ClaudeCli::new("/nonexistent/claude"),
            big_brain_enabled: true,
        };
        assert!(!l.cli.available());
        let a = l.available(true, true);
        assert!(!a.big_brain);
        // The ask wanted the big brain; it resolves down to what exists.
        let hard = Ask::from_operator(
            "explain the root cause step by step; ```panicked at``` why? how come? \
             compare the trade-off",
        );
        let (rung, t) = l.start_at(&hard, a).expect("something exists");
        assert_eq!(t.rung, Rung::BigBrain);
        assert_eq!(rung, Rung::Deliberate);

        match l.give_up(&[Rung::Reflex, Rung::Deliberate]) {
            Verdict::OutOfDepth { why, .. } => {
                assert!(why.contains("not switched on"), "{why}");
                // Never a guess dressed as an answer.
                assert!(!why.contains("probably"));
            }
            other => panic!("expected out of depth, got {other:?}"),
        }
    }

    #[test]
    fn the_big_brain_needs_the_operators_say_so() {
        assert_eq!(
            ClaudeCli::new("/nonexistent/claude").descriptor().consent,
            Consent::Explicit
        );
        assert!(!Ladder::default().big_brain_enabled);
    }

    #[tokio::test]
    async fn asking_a_cli_that_is_not_there_is_an_outcome_not_an_error() {
        let cli = ClaudeCli::new("/nonexistent/claude");
        let o = cli.ask("anything").await;
        assert!(!o.ok);
        assert!(o.unavailable, "absence is not failure");
        assert!(o.summary.contains("not installed"), "{}", o.summary);
    }

    #[test]
    fn with_nothing_loaded_at_all_there_is_no_rung_to_start_on() {
        let l = Ladder::default();
        let err = l
            .start_at(&Ask::from_operator("hello"), Available::none())
            .unwrap_err();
        assert!(matches!(err, MindError::NotLoaded(_)));
    }
}
