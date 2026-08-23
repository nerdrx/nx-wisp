//! The whole crate, wired together.
//!
//! [`Mind`] is where F12–F19 meet: the two-tier manager, the KV cache, the tool
//! registry, memory, the mood FSM, the escalation ladder and the deferred queue.
//! It owns the turn — what happens between "the operator asked something" and
//! "there is an [`Utterance`] waiting for `wisp-attn`".
//!
//! ## Two rules it exists to enforce
//!
//! **SPEC §3.4 — she does not speak.** Everything she wants to say comes out of
//! [`Mind::take_outbox`] as an [`Utterance`]. There is no path from this crate
//! to a terminal, a notification, or a pixel; `tests/no_speaking.rs` checks that
//! mechanically, on the source.
//!
//! **SPEC §3.5 — at T3/T4 work is queued, not run.** Every entry point that
//! would start cognition checks [`Tier::may_think`] *first*, and the queue is
//! replayed oldest-first with staleness filtering when the tier comes back.

use std::sync::{Arc, Mutex};

use serde_json::Value;
use wisp_gov::{DeviceChoice, VramBudget};
use wisp_proto::{Cost, EventKind, Governed, Millis, Observation, Tier, TierReason, Urgency, Utterance};

use crate::backend::{Backend, GenRequest, Generated, Role, Sampling, SlotId};
use crate::defer::{DeferQueue, Job, JobKind, Pushed, Replayed};
use crate::error::{MindError, Result};
use crate::escalate::{Ask, Available, Ladder, Rung, SelfAssessment};
use crate::events::EventSink;
use crate::grammar::{GrammarOptions, ToolCall};
use crate::kv::{ConversationId, KvCache};
use crate::manager::{lock_backend, ModelManager, ModelSettings};
use crate::memory::embed::{Embedder, HashEmbedder, ModelEmbedder};
use crate::memory::{Memory, MemoryKind, NewMemory, WallClock};
use crate::models::ModelRegistry;
use crate::mood::{Mood, MoodFsm, MoodReason};
use crate::prompt::{ChatTemplate, Message, Persona, PromptBuilder, State};
use crate::tools::builtin::{Builtins, MemoryHandle};
use crate::tools::{ToolOutcome, ToolRegistry};

/// What a turn produced.
#[derive(Debug, Clone, PartialEq)]
pub enum Thought {
    /// She wants to say this. A *proposal*: `wisp-attn` decides.
    Proposed(Utterance),
    /// She called a tool, and what it said.
    Tool {
        call: ToolCall,
        outcome: ToolOutcome,
        then: Option<Utterance>,
    },
    /// SPEC §3.5: the tier said no, so this is queued instead.
    Deferred { id: u64, depth: usize },
    /// The queue was full and this was the least urgent thing in it.
    Discarded { why: String },
    /// She got to the top of the ladder and stopped rather than guessing.
    OutOfDepth(Utterance),
    Nothing,
}

impl Thought {
    pub fn utterance(&self) -> Option<&Utterance> {
        match self {
            Thought::Proposed(u) | Thought::OutOfDepth(u) => Some(u),
            Thought::Tool { then, .. } => then.as_ref(),
            _ => None,
        }
    }
}

/// How chatty and how expensive a turn is allowed to be.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnConfig {
    pub max_tokens: u32,
    pub temperature: f32,
    /// How many memories to put in front of her.
    pub recall_k: usize,
    /// How many tool hops one turn may take before she has to say something.
    pub max_tool_hops: u32,
    /// Ask the reflex model whether it is out of its depth before answering
    /// (F17). Costs one constrained token.
    pub self_assess: bool,
}

impl Default for TurnConfig {
    fn default() -> Self {
        TurnConfig {
            max_tokens: 256,
            temperature: 0.7,
            recall_k: 4,
            max_tool_hops: 2,
            self_assess: true,
        }
    }
}

pub struct Mind {
    manager: ModelManager,
    memory: MemoryHandle,
    tools: ToolRegistry,
    builtins: Builtins,
    mood: MoodFsm,
    kv: KvCache,
    prompts: PromptBuilder,
    defer: DeferQueue,
    ladder: Ladder,
    turn: TurnConfig,
    events: EventSink,
    tier: Tier,
    outbox: Vec<Utterance>,
    /// Whatever the senses last said that is worth putting in the prompt.
    context: Vec<String>,
}

impl std::fmt::Debug for Mind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mind")
            .field("tier", &self.tier)
            .field("mood", &self.mood.mood())
            .field("tools", &self.tools.names())
            .field("deferred", &self.defer.len())
            .field("outbox", &self.outbox.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

/// Building a [`Mind`] takes half a dozen decisions that all have sensible
/// answers, so they are defaulted and overridable rather than positional.
pub struct MindBuilder {
    backend: Box<dyn Backend>,
    registry: ModelRegistry,
    settings: ModelSettings,
    memory: Option<Memory>,
    embedder: Box<dyn Embedder>,
    clock: WallClock,
    persona: Persona,
    template: ChatTemplate,
    events: EventSink,
    ladder: Ladder,
    turn: TurnConfig,
    conversations: usize,
    defer_cap: usize,
    tool_state: Option<std::path::PathBuf>,
}

impl MindBuilder {
    pub fn new(backend: Box<dyn Backend>) -> Self {
        MindBuilder {
            backend,
            registry: ModelRegistry::builtin(),
            settings: ModelSettings::default(),
            memory: None,
            embedder: Box::new(HashEmbedder::default()),
            clock: WallClock::system(),
            persona: Persona::default(),
            template: ChatTemplate::ChatMl,
            events: EventSink::silent(),
            ladder: Ladder::default(),
            turn: TurnConfig::default(),
            conversations: 2,
            defer_cap: 32,
            tool_state: None,
        }
    }

    pub fn registry(mut self, r: ModelRegistry) -> Self {
        self.registry = r;
        self
    }
    pub fn settings(mut self, s: ModelSettings) -> Self {
        self.settings = s;
        self
    }
    pub fn memory(mut self, m: Memory) -> Self {
        self.memory = Some(m);
        self
    }
    pub fn embedder(mut self, e: Box<dyn Embedder>) -> Self {
        self.embedder = e;
        self
    }
    pub fn clock(mut self, c: WallClock) -> Self {
        self.clock = c;
        self
    }
    pub fn persona(mut self, p: Persona) -> Self {
        self.persona = p;
        self
    }
    pub fn template(mut self, t: ChatTemplate) -> Self {
        self.template = t;
        self
    }
    pub fn events(mut self, e: EventSink) -> Self {
        self.events = e;
        self
    }
    pub fn ladder(mut self, l: Ladder) -> Self {
        self.ladder = l;
        self
    }
    pub fn turn(mut self, t: TurnConfig) -> Self {
        self.turn = t;
        self
    }
    pub fn tool_state_file(mut self, p: impl Into<std::path::PathBuf>) -> Self {
        self.tool_state = Some(p.into());
        self
    }

    pub fn build(self) -> Result<Mind> {
        let memory = match self.memory {
            Some(m) => m,
            None => Memory::open(crate::dirs::memory_db())?,
        };
        let handle = MemoryHandle::new(
            Arc::new(Mutex::new(memory)),
            self.embedder,
            self.clock.clone(),
        );
        let builtins = Builtins::new(handle.clone());
        let manager = ModelManager::shared(
            Arc::new(Mutex::new(self.backend)),
            self.registry,
            self.settings,
        )
        .with_events(self.events.clone());

        let mut tools = ToolRegistry::new().with_events(self.events.clone());
        if let Some(p) = self.tool_state {
            tools = tools.with_state_file(p);
        }
        for (name, r) in tools.register_all(builtins.descriptors()) {
            if let Err(e) = r {
                // A built-in that will not register is a bug in this crate, not
                // a runtime condition. Loud, and then carry on without it: she
                // is more useful with five tools than with none.
                tracing::error!(tool = %name, error = %e, "a built-in tool would not register");
            }
        }
        tools.register(self.ladder.cli.descriptor())?;
        tools.load()?;

        Ok(Mind {
            manager,
            memory: handle,
            tools,
            builtins,
            mood: MoodFsm::default(),
            kv: KvCache::new(self.conversations),
            prompts: PromptBuilder::new(self.persona, self.template),
            defer: DeferQueue::new(self.defer_cap).with_events(self.events.clone()),
            ladder: self.ladder,
            turn: self.turn,
            events: self.events,
            tier: Tier::Full,
            outbox: Vec::new(),
            context: Vec::new(),
        })
    }
}

// ---------------------------------------------------------------------------

impl Mind {
    pub fn builder(backend: Box<dyn Backend>) -> MindBuilder {
        MindBuilder::new(backend)
    }

    pub fn manager(&self) -> &ModelManager {
        &self.manager
    }
    pub fn manager_mut(&mut self) -> &mut ModelManager {
        &mut self.manager
    }
    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }
    pub fn tools_mut(&mut self) -> &mut ToolRegistry {
        &mut self.tools
    }
    pub fn builtins(&self) -> &Builtins {
        &self.builtins
    }
    pub fn memory(&self) -> &MemoryHandle {
        &self.memory
    }
    pub fn mood(&self) -> Mood {
        self.mood.mood()
    }
    pub fn mood_fsm(&mut self) -> &mut MoodFsm {
        &mut self.mood
    }
    pub fn kv(&self) -> &KvCache {
        &self.kv
    }
    pub fn deferred(&self) -> &DeferQueue {
        &self.defer
    }
    pub fn ladder(&self) -> &Ladder {
        &self.ladder
    }
    pub fn tier(&self) -> Tier {
        self.tier
    }

    /// Register more tools — `wisp-fleet`'s, and whatever the binary adds.
    pub fn add_tools(&mut self, tools: impl IntoIterator<Item = crate::tools::ToolDescriptor>) {
        for (name, r) in self.tools.register_all(tools) {
            if let Err(e) = r {
                tracing::warn!(tool = %name, error = %e, "tool refused registration");
            }
        }
    }

    /// The governor's device and budget decisions (F13/F61).
    pub fn apply_governor(&mut self, device: DeviceChoice, budget: VramBudget) {
        self.manager.apply(device, budget);
    }

    // --- speaking, which she does not do -----------------------------------

    /// Queue something for `wisp-attn`. This is the *only* way anything leaves
    /// this crate (SPEC §3.4).
    pub fn propose(&mut self, u: Utterance) {
        self.events.emit(EventKind::Proposed(u.clone()));
        self.outbox.push(u);
    }

    /// Everything she wants to say, handed over. The caller submits it to
    /// `wisp-attn`, which decides whether any of it is said.
    pub fn take_outbox(&mut self) -> Vec<Utterance> {
        std::mem::take(&mut self.outbox)
    }

    pub fn outbox_len(&self) -> usize {
        self.outbox.len()
    }

    // --- the senses --------------------------------------------------------

    /// A sense saw something. Moves the mood, and remembers it if it is worth
    /// remembering.
    pub fn observe(&mut self, obs: &Observation, now: Millis) {
        self.mood.observe(obs, now);
        if let Some((text, salience)) = worth_remembering(obs) {
            // Writing a memory is cheap, but not free, and at T3/T4 "cheap" is
            // still more than she is allowed. Queue it.
            if !self.tier.may_think() {
                self.defer.push(
                    Job::new(JobKind::Remember, text, now)
                        .payload(serde_json::json!({ "salience": salience }))
                        .urgency(Urgency::Whim),
                );
                return;
            }
            if let Err(e) = self.memory.remember(
                NewMemory::episodic(&text)
                    .salience(salience)
                    .from(format!("{:?}", obs.sense())),
            ) {
                tracing::warn!(error = %e, "could not write an episodic memory");
            }
        }
        if let Some((mood, why)) = self.mood.tick(now) {
            self.on_mood_change(mood, why);
        }
    }

    /// One pass of the clock: mood relaxation and timers coming due.
    pub fn tick(&mut self, now: Millis) {
        if let Some((mood, why)) = self.mood.tick(now) {
            self.on_mood_change(mood, why);
        }
        for t in self.builtins.due_timers(self.memory.clock().now()) {
            // A timer the operator set is something they asked for, so it is an
            // `Answer` and does not pay the attention budget.
            self.propose(Utterance {
                expression: Some(self.mood.expression().to_string()),
                ..Utterance::new(format!("{} — that's your timer.", t.label), Urgency::Answer)
            });
        }
    }

    fn on_mood_change(&mut self, mood: Mood, why: MoodReason) {
        tracing::debug!(?mood, ?why, "mood");
    }

    /// `wisp-attn` refused or dropped something she proposed. She notices.
    pub fn snubbed(&mut self, now: Millis) {
        self.mood.snubbed(now);
    }
    /// It was said.
    pub fn heard(&mut self, now: Millis) {
        self.mood.heard(now);
    }
    /// Somebody picked her up.
    pub fn petted(&mut self, now: Millis) {
        self.mood.petted(now);
    }

    /// What the senses last saw, for the prompt's state block.
    pub fn set_context(&mut self, lines: Vec<String>) {
        self.context = lines;
    }

    // --- thinking ----------------------------------------------------------

    /// One turn.
    ///
    /// At T3/T4 this does not run: it queues (SPEC §3.5) and returns
    /// [`Thought::Deferred`].
    pub async fn think(
        &mut self,
        ask: Ask,
        conversation: ConversationId,
        now: Millis,
    ) -> Result<Thought> {
        if !self.tier.may_think() {
            return Ok(self.defer_ask(&ask, now));
        }

        let reflex = self.manager.ensure(Role::Reflex);
        // "Available" means *could actually be loaded* — the file is on disk —
        // not "is named in the registry". A rung that would fail is not a rung,
        // and pretending otherwise is how she ends up answering a hard question
        // with the small model and sounding confident about it.
        let available = self
            .ladder
            .available(reflex.is_ok(), self.manager.can_load(Role::Deliberate));
        let Ok(handle) = reflex else {
            // Nothing is loaded and nothing can be. That is an honest "I can't",
            // not a crash and not a guess.
            let v = self.ladder.give_up(&[], available);
            let u = v.utterance();
            self.propose(u.clone());
            return Ok(Thought::OutOfDepth(u));
        };

        let (mut rung, triage) = self.ladder.start_at(&ask, available)?;
        tracing::debug!(?rung, complexity = triage.complexity, why = triage.why, "triage");

        // F17: ask the small model whether it is out of its depth. It is much
        // better at recognising a hard question than at answering one.
        if self.turn.self_assess && rung == Rung::Reflex && ask.from_operator {
            if let Ok(SelfAssessment::Escalate) | Ok(SelfAssessment::Unsure) =
                self.self_assess(handle, &ask)
            {
                match available.resolve(Rung::Deliberate) {
                    Some(up) if up != Rung::Reflex => rung = up,
                    // It said it could not, and there is nothing above it. This
                    // is the moment F17 exists for: she stops, rather than
                    // answering anyway with the model that has just told her it
                    // is out of its depth.
                    _ => {
                        let v = self.ladder.give_up(&[Rung::Reflex], available);
                        let u = v.utterance();
                        self.propose(u.clone());
                        return Ok(Thought::OutOfDepth(u));
                    }
                }
            }
        }

        let handle = match rung {
            Rung::Reflex => handle,
            Rung::Deliberate => match self.manager.ensure(Role::Deliberate) {
                Ok(h) => h,
                Err(e) if e.is_tier_refusal() => return Ok(self.defer_ask(&ask, now)),
                // The big model is not on disk, or would not load. Fall back
                // rather than fail: a smaller answer beats no answer, and the
                // self-assessment already ran.
                Err(e) => {
                    tracing::info!(error = %e, "falling back to the reflex model");
                    rung = Rung::Reflex;
                    handle
                }
            },
            Rung::BigBrain => {
                return self.ask_the_big_brain(&ask).await;
            }
        };

        self.turn_with(handle, rung, ask, conversation, now).await
    }

    /// The reflex model's opinion of its own depth. One constrained token.
    fn self_assess(
        &mut self,
        handle: crate::backend::ModelHandle,
        ask: &Ask,
    ) -> Result<SelfAssessment> {
        let grammar = crate::escalate::self_assessment_grammar()?;
        let req = GenRequest::new(crate::escalate::self_assessment_prompt(ask))
            .grammar(grammar)
            .max_tokens(8)
            .slot(SlotId(1))
            .sampling(Sampling::DETERMINISTIC);
        let out = self.generate(handle, &req)?;
        SelfAssessment::parse(&out.text).ok_or_else(|| {
            MindError::Inference(format!("self-assessment produced {:?}", out.text))
        })
    }

    async fn turn_with(
        &mut self,
        handle: crate::backend::ModelHandle,
        rung: Rung,
        ask: Ask,
        conversation: ConversationId,
        now: Millis,
    ) -> Result<Thought> {
        let recalled = self.recall_for(&ask);
        let mut messages = vec![Message::operator(&ask.text)];

        for hop in 0..=self.turn.max_tool_hops {
            let state = State {
                mood: self.mood.mood(),
                tier: self.tier,
                time_of_day: None,
                context: self.context.clone(),
                recalled: recalled.clone(),
                tools: if ask.allow_tools {
                    self.tools
                        .available()
                        .iter()
                        .map(|d| d.name.to_string())
                        .collect()
                } else {
                    Vec::new()
                },
            };
            let opts = GrammarOptions {
                allow_say: true,
                allow_thought: false,
                max_string_chars: None,
            };
            // The last hop offers no tools: she has to say something.
            let grammar = if ask.allow_tools && hop < self.turn.max_tool_hops {
                self.tools.grammar(&opts)?
            } else {
                crate::grammar::reply_grammar(&opts)?
            };

            let rendered = self.prompts.render(&state, &messages);
            let plan = self.plan_kv(handle, conversation, &rendered.text, now)?;
            let req = GenRequest::new(&rendered.text)
                .grammar(grammar)
                .max_tokens(self.turn.max_tokens)
                .slot(plan.slot)
                .sampling(Sampling {
                    temperature: self.turn.temperature,
                    ..Sampling::default()
                });
            let out = self.generate(handle, &req)?;
            self.commit_kv(handle, &plan, &rendered.text, &out.text, now)?;

            match parse_reply(&out.text) {
                Reply::Say(text) => {
                    let u = Utterance {
                        expression: Some(self.mood.expression().to_string()),
                        ..Utterance::new(text, urgency_for(&ask))
                    };
                    self.remember_turn(&ask, u.text.as_str(), rung);
                    self.propose(u.clone());
                    return Ok(Thought::Proposed(u));
                }
                Reply::Call(call) => {
                    let outcome = self
                        .tools
                        .invoke_or_excuse(&call.name, call.arguments.clone())
                        .await;
                    if outcome.ok {
                        // She predicted a tool would help and it did.
                        self.mood.vindicated(now);
                    }
                    messages.push(Message::wisp(format!(
                        "calling {} with {}",
                        call.name, call.arguments
                    )));
                    messages.push(Message::tool(&call.name, &outcome.summary));
                    if hop == self.turn.max_tool_hops {
                        let u = Utterance::new(outcome.summary.clone(), urgency_for(&ask));
                        self.propose(u.clone());
                        return Ok(Thought::Tool {
                            call,
                            outcome,
                            then: Some(u),
                        });
                    }
                    // Round again, with the result in front of her.
                    continue;
                }
                Reply::Unparseable(raw) => {
                    // With a constrained decoder this is unreachable; if the
                    // backend ignored the grammar it is a bug worth saying so
                    // about rather than papering over.
                    return Err(MindError::Inference(format!(
                        "the backend ignored the grammar and produced {raw:?}"
                    )));
                }
            }
        }
        Ok(Thought::Nothing)
    }

    async fn ask_the_big_brain(&mut self, ask: &Ask) -> Result<Thought> {
        let outcome = self
            .tools
            .invoke_or_excuse(
                "big_brain",
                serde_json::json!({ "question": ask.text.clone() }),
            )
            .await;
        if outcome.ok {
            let u = Utterance::new(outcome.summary.clone(), Urgency::Answer);
            self.propose(u.clone());
            return Ok(Thought::Tool {
                call: ToolCall {
                    name: "big_brain".into(),
                    arguments: serde_json::json!({ "question": ask.text.clone() }),
                },
                outcome,
                then: Some(u),
            });
        }
        // Absent, switched off, or it failed. Either way she does not know, and
        // says so (F17).
        // Everything was nominally available; it just did not produce an answer.
        let v = self.ladder.give_up(
            &[Rung::Reflex, Rung::Deliberate, Rung::BigBrain],
            Available { reflex: true, deliberate: true, big_brain: true },
        );
        let u = v.utterance();
        self.propose(u.clone());
        Ok(Thought::OutOfDepth(u))
    }

    fn defer_ask(&mut self, ask: &Ask, now: Millis) -> Thought {
        let job = Job::new(JobKind::Reply, ask.text.clone(), now)
            .payload(serde_json::json!({
                "from_operator": ask.from_operator,
                "allow_tools": ask.allow_tools,
            }))
            .urgency(if ask.from_operator {
                Urgency::Answer
            } else {
                Urgency::Whim
            });
        match self.defer.push(job) {
            Pushed::Queued { id, depth } => Thought::Deferred { id, depth },
            Pushed::Displaced { id, depth, dropped } => {
                tracing::debug!(dropped = %dropped.what, "displaced a deferred thought");
                Thought::Deferred { id, depth }
            }
            Pushed::Refused { job } => Thought::Discarded {
                why: format!("there was no room to remember to answer \"{}\"", job.what),
            },
        }
    }

    /// SPEC §3.5's replay. Called when the tier comes back up.
    pub fn replay_deferred(&mut self, now: Millis) -> Replayed {
        if !self.tier.may_think() {
            return Replayed::default();
        }
        self.defer.replay(now)
    }

    // --- the pieces --------------------------------------------------------

    fn generate(
        &self,
        handle: crate::backend::ModelHandle,
        req: &GenRequest,
    ) -> Result<Generated> {
        let backend = self.manager.backend();
        let mut b = lock_backend(&backend);
        b.generate(handle, req, &mut |_| crate::backend::Flow::Continue)
    }

    fn plan_kv(
        &mut self,
        handle: crate::backend::ModelHandle,
        conversation: ConversationId,
        text: &str,
        now: Millis,
    ) -> Result<crate::kv::Plan> {
        let backend = self.manager.backend();
        let b = lock_backend(&backend);
        // F15: the persona prefix is installed once and is the same bytes every
        // turn, so this is a no-op after the first call.
        let persona = b.tokenize(handle, &self.prompts.prefix(), true)?;
        let prompt = b.tokenize(handle, text, true)?;
        drop(b);
        self.kv.set_persona(persona);
        Ok(self.kv.plan_for(conversation, &prompt, now))
    }

    fn commit_kv(
        &mut self,
        handle: crate::backend::ModelHandle,
        plan: &crate::kv::Plan,
        prompt: &str,
        generated: &str,
        now: Millis,
    ) -> Result<()> {
        let backend = self.manager.backend();
        let b = lock_backend(&backend);
        let mut tokens = b.tokenize(handle, prompt, true)?;
        tokens.extend(b.tokenize(handle, generated, false)?);
        drop(b);
        self.kv.commit(plan, tokens, now);
        Ok(())
    }

    fn recall_for(&self, ask: &Ask) -> Vec<String> {
        if self.turn.recall_k == 0 {
            return Vec::new();
        }
        match self.memory.recall(&ask.text, self.turn.recall_k) {
            Ok(rs) => rs.into_iter().map(|r| r.memo.text).collect(),
            Err(e) => {
                tracing::debug!(error = %e, "recall failed; carrying on without memory");
                Vec::new()
            }
        }
    }

    fn remember_turn(&mut self, ask: &Ask, said: &str, rung: Rung) {
        if !ask.from_operator {
            return;
        }
        let text = format!("They asked: {}. I said: {}", ask.text.trim(), said.trim());
        if let Err(e) = self.memory.remember(
            NewMemory {
                kind: MemoryKind::Episodic,
                text,
                // A conversation is more memorable than a window changing, and
                // one that needed the big model is more memorable still.
                salience: match rung {
                    Rung::Reflex => 0.35,
                    Rung::Deliberate => 0.5,
                    Rung::BigBrain => 0.6,
                },
                source: Some("conversation".into()),
                detail: None,
            },
        ) {
            tracing::warn!(error = %e, "could not remember the turn");
        }
    }

    /// Upgrade memory to the real embedding model once it is loaded. Returns the
    /// embedder id it replaced.
    pub fn use_model_embeddings(&mut self) -> Result<String> {
        let handle = self.manager.ensure(Role::Embed)?;
        let entry = self.manager.entry_for(Role::Embed)?.clone();
        let dim = if entry.embedding_dim > 0 {
            entry.embedding_dim as usize
        } else {
            1024
        };
        let e = ModelEmbedder::new(self.manager.backend(), handle, dim, entry.name.clone());
        Ok(self.memory.set_embedder(Box::new(e)))
    }

    /// F18's nightly pass. Refuses above T0 (the store enforces it), which is
    /// why the caller queues it rather than calling it on a timer.
    pub fn consolidate(&mut self) -> Result<crate::memory::Consolidation> {
        let handle = self.manager.ensure(Role::Deliberate)?;
        let now = self.memory.clock().now();
        let backend = self.manager.backend();
        let mem = self.memory.memory();
        let mut m = mem.lock().unwrap_or_else(|e| e.into_inner());
        let mut b = lock_backend(&backend);
        let mut e: Box<dyn Embedder> = Box::new(HashEmbedder::default());
        // Use whatever the store is actually using, so the summary is
        // comparable with everything else in it.
        let id = self.memory.embedder_id();
        if id.starts_with("model:") {
            e = Box::new(ModelEmbedder::new(
                self.manager.backend(),
                handle,
                dim_from_id(&id),
                id.clone(),
            ));
        }
        m.consolidate(self.tier, b.as_mut(), handle, e.as_mut(), now)
    }

    /// Delete what has faded (F18). Safe at any tier: it is a `DELETE`, not a
    /// thought.
    pub fn forget_faded(&mut self) -> Result<usize> {
        let now = self.memory.clock().now();
        let mem = self.memory.memory();
        let mut m = mem.lock().unwrap_or_else(|e| e.into_inner());
        let gone = m.forget(now)?;
        for g in &gone {
            self.events.emit(EventKind::Dropped {
                text: g.text.clone(),
                why: "it faded".to_string(),
            });
        }
        Ok(gone.len())
    }
}

fn dim_from_id(id: &str) -> usize {
    id.rsplit(':')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024)
}

fn urgency_for(ask: &Ask) -> Urgency {
    if ask.from_operator {
        Urgency::Answer
    } else {
        Urgency::Notable
    }
}

/// What the constrained decoder produced.
#[derive(Debug, Clone, PartialEq)]
enum Reply {
    Say(String),
    Call(ToolCall),
    Unparseable(String),
}

fn parse_reply(text: &str) -> Reply {
    let Ok(v) = serde_json::from_str::<Value>(text.trim()) else {
        return Reply::Unparseable(text.to_string());
    };
    if let Some(s) = v.get("say").and_then(Value::as_str) {
        return Reply::Say(s.to_string());
    }
    match serde_json::from_value::<ToolCall>(v) {
        Ok(c) if !c.name.is_empty() => Reply::Call(c),
        _ => Reply::Unparseable(text.to_string()),
    }
}

/// Which observations are worth an episodic row, and how much they matter.
///
/// Most are not. A store that remembers every focus change is a store whose
/// recall is noise, and F18's decay would be doing nothing but deleting.
fn worth_remembering(obs: &Observation) -> Option<(String, f32)> {
    match obs {
        Observation::Notification { app, summary, .. } if !summary.trim().is_empty() => {
            Some((format!("{app} said: {summary}"), 0.25))
        }
        Observation::Media {
            title,
            artist,
            playing: true,
            ..
        } if !title.is_empty() => Some((format!("they were listening to {title} by {artist}"), 0.1)),
        Observation::Vitals { temp_c, .. } if *temp_c >= 90 => {
            Some((format!("the GPU hit {temp_c}°C"), 0.7))
        }
        Observation::Files { path, dirty: true } => {
            Some((format!("{path} changed"), 0.15))
        }
        Observation::Speech {
            text,
            final_: true,
        } if !text.trim().is_empty() => Some((format!("they said: {text}"), 0.4)),
        // Focus, windows, idle, workspaces, audio levels, clipboard, fleet
        // chatter: seen, reacted to, not written down. The mood FSM is where
        // those leave their mark.
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// SPEC §3.1
// ---------------------------------------------------------------------------

impl Governed for Mind {
    fn set_tier(&mut self, tier: Tier, reason: &TierReason) {
        let from = self.tier;
        self.tier = tier;
        self.manager.set_tier(tier, reason);
        self.mood.set_tier(tier, reason);

        if !tier.may_hold_model() {
            // The contexts went with the models. The persona prefix is text and
            // survives, so coming back does not cost a re-prefill of it.
            self.kv.clear_conversations();
        }
        if tier == Tier::Dormant {
            // T4 is not "later" — it is being switched off. Everything queued is
            // dropped and recorded (SPEC §3.5).
            self.defer.silence();
            self.outbox.clear();
        }
        tracing::debug!(?from, to = ?tier, ?reason, "mind tier");
    }

    fn cost_at(tier: Tier) -> Cost {
        // The models dominate everything else by three orders of magnitude; the
        // rest is the memory database's page cache and a few structs.
        ModelManager::cost_at(tier)
            + Cost {
                ram_mib: 24,
                vram_mib: 0,
                cpu_centi_pct: 10,
            }
    }
}
