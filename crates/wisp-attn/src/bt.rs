//! F50 — declarative behaviour trees. **Data only**, per SPEC §3.6: a skin can
//! never contain executable code, so every antic she has is a tree of enum
//! nodes that serialise to and from a skin file with no `eval` anywhere.
//!
//! Node kinds: [`Node::Sequence`], [`Node::Selector`], [`Node::Parallel`],
//! [`Node::Condition`], [`Node::Action`], [`Node::Cooldown`],
//! [`Node::Random`] (weighted choice) and [`Node::Invert`].
//!
//! Two rules keep the trees honest:
//!
//! 1. **She cannot speak from a tree.** An [`Action::Say`] becomes
//!    [`Effect::Propose`], which the caller must hand to the interruption
//!    budget. SPEC §3.4 has exactly one door to the operator and this is not
//!    it. [`crate::Attention`] is the thing that wires the two together.
//! 2. **Randomness is seeded.** Same seed plus same trace equals same antics,
//!    which is what makes "she did something weird" a reproducible bug report.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use wisp_proto::{Millis, Tier, Urgency, Utterance};

use crate::flow::{Moment, Opportunity};
use crate::relationship::Relationship;
use crate::rng::Rng;
use crate::world::World;
use crate::{Chattiness, Mood};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Success,
    Failure,
    /// She is mid-antic. Nothing else in this tree runs until she is done.
    Running,
}

impl Status {
    pub fn is_success(self) -> bool {
        self == Status::Success
    }
}

// ---------------------------------------------------------------------------
// The tree
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Node {
    /// All children in order; stops at the first failure.
    Sequence(Vec<Node>),
    /// Children in order until one succeeds.
    Selector(Vec<Node>),
    /// Every child, every tick.
    Parallel { policy: ParallelPolicy, children: Vec<Node> },
    /// A question about the world or about her.
    Condition(Condition),
    /// Something she does.
    Action(Action),
    /// Fails outright until `ms` after the child last *succeeded*. Keys are
    /// author-chosen so several nodes can share one budget ("she has poked a
    /// window recently" is one fact, however many trees wanted to).
    Cooldown { key: String, ms: Millis, child: Box<Node> },
    /// Weighted choice. Zero-weight entries are never picked; an empty list
    /// fails rather than picking anything.
    Random { choices: Vec<Weighted> },
    /// Success becomes failure and back. `Running` passes through.
    Invert(Box<Node>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Weighted {
    pub weight: u32,
    pub node: Node,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParallelPolicy {
    /// Fails if any child fails.
    RequireAll,
    /// Succeeds if any child succeeds.
    RequireOne,
}

// ---------------------------------------------------------------------------
// Conditions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Condition {
    Always,
    Never,
    // --- the operator, from the observation stream
    IdleAtLeast { ms: Millis },
    Active,
    FocusIs { app_id: String },
    FocusContains { needle: String },
    FocusHeldAtLeast { ms: Millis },
    WorkspaceIs { name: String },
    WindowsAtLeast { n: u32 },
    MediaPlaying,
    MicLive,
    NotificationWithin { ms: Millis },
    SpokeToHerWithin { ms: Millis },
    FilesDirty,
    FleetSaid { app: String, field: String, value: String },
    // --- the machine
    CpuAbove { pct: u8 },
    GpuAbove { pct: u8 },
    TempAbove { c: u8 },
    OnBattery,
    TierAtMost { tier: Tier },
    // --- her own read of the moment
    FlowAbove { conf: f32 },
    FlowBelow { conf: f32 },
    OpeningWithin { ms: Millis },
    /// A *particular* kind of opening just happened — they came back, a build
    /// finished, the music stopped. This is what makes a tree able to react.
    OpeningIs { kind: Opportunity, ms: Millis },
    Silenced,
    // --- her
    MoodIs { mood: Mood },
    EnergyAbove { v: f32 },
    EnergyBelow { v: f32 },
    ChattinessAtLeast { level: Chattiness },
    HourBetween { from: u8, to: u8 },
    FavouriteHour,
    DaysOwnedAtLeast { days: u32 },
    PettedAtLeast { times: u64 },
    ThrownAtLeast { times: u64 },
    // --- her own memory of what she has been doing
    SinceMark { key: String, ms: Millis },
    NoMark { key: String },
    SessionIs { state: SessionState },
    SessionElapsedAtLeast { ms: Millis },
    SessionOverdue,
    // --- combinators, so a skin can express "not" without new variants
    Chance { permille: u16 },
    Not(Box<Condition>),
    All(Vec<Condition>),
    Any(Vec<Condition>),
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Action {
    /// Propose speech. It goes to the interruption budget, not to the operator.
    Say {
        text: String,
        urgency: Urgency,
        expression: Option<String>,
        stale_after_ms: Option<Millis>,
        defer_ms: Option<Millis>,
    },
    /// Propose one line, picked by the seeded RNG. Empty list fails.
    SayOneOf {
        lines: Vec<String>,
        urgency: Urgency,
        expression: Option<String>,
        stale_after_ms: Option<Millis>,
    },
    /// Ask the rig for a clip. `hold_ms` is how long she is busy with it.
    Clip { name: String, loop_: bool, hold_ms: Millis },
    /// Go somewhere.
    Move { to: MoveTarget, hold_ms: Millis },
    /// Shove at a window. `wisp-shell` decides what that means.
    Poke { target: PokeTarget },
    SetMood { mood: Mood },
    /// Remember that this happened, for `SinceMark`/`NoMark`.
    Mark { key: String },
    /// A named hook for the host, so a skin can drive host features that this
    /// crate knows nothing about.
    Tag { name: String },
    /// Occupy her for a while without doing anything else.
    Wait { ms: Millis },
    // --- the focus warden (F40)
    StartSession { minutes: u32 },
    StartBreak { minutes: u32 },
    EndSession,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MoveTarget {
    /// Amble somewhere nearby.
    Wander,
    /// Chase the pointer.
    Cursor,
    /// Sit on the top edge of the focused window.
    ActiveWindowEdge,
    /// The biggest window on screen.
    LargestWindowEdge,
    /// Her corner.
    Home,
    /// Off the edge of the screen (fleet hop, sulking).
    OffScreen,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PokeTarget {
    Active,
    Largest,
    Id(u64),
}

/// What a tree tick produced. Facts about what she *wants*; the host performs
/// them, and speech goes through the budget first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Effect {
    /// Candidate speech. Must be submitted to [`crate::Budget`].
    Propose(Utterance),
    Clip { name: String, loop_: bool },
    Move(MoveTarget),
    Poke { window: Option<u64> },
    Mood(Mood),
    Tag(String),
    Session(SessionEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionEvent {
    Started { minutes: u32 },
    BreakStarted { minutes: u32 },
    Ended,
}

// ---------------------------------------------------------------------------
// The focus warden's state (F40)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SessionState {
    #[default]
    Off,
    Work,
    Break,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Session {
    pub state: SessionState,
    pub started: Millis,
    pub ends: Millis,
    /// Completed work sessions, this run.
    pub completed: u32,
}

impl Session {
    pub fn elapsed(&self, now: Millis) -> Millis {
        now.saturating_sub(self.started)
    }
    pub fn overdue(&self, now: Millis) -> bool {
        self.state != SessionState::Off && now >= self.ends
    }
}

// ---------------------------------------------------------------------------
// Evaluation context
// ---------------------------------------------------------------------------

/// Everything a condition may read. Borrowed, so a tick allocates nothing but
/// its effects.
#[derive(Debug, Clone, Copy)]
pub struct BtCtx<'a> {
    pub now: Millis,
    pub world: &'a World,
    pub moment: &'a Moment,
    pub relationship: &'a Relationship,
    pub mood: Mood,
    /// 0..=1. Low means sleepy; high means she is bouncing off the walls.
    pub energy: f32,
    pub chattiness: Chattiness,
    /// Local hour, supplied by the host. This crate never reads a clock.
    pub hour: u8,
    pub tier: Tier,
}

impl<'a> BtCtx<'a> {
    pub fn new(
        now: Millis,
        world: &'a World,
        moment: &'a Moment,
        relationship: &'a Relationship,
    ) -> Self {
        BtCtx {
            now,
            world,
            moment,
            relationship,
            mood: Mood::Calm,
            energy: 0.5,
            chattiness: Chattiness::default(),
            hour: 12,
            tier: Tier::Full,
        }
    }
    pub fn with_mood(mut self, mood: Mood) -> Self {
        self.mood = mood;
        self
    }
    pub fn with_energy(mut self, energy: f32) -> Self {
        self.energy = energy;
        self
    }
    pub fn with_chattiness(mut self, dial: Chattiness) -> Self {
        self.chattiness = dial;
        self
    }
    pub fn with_hour(mut self, hour: u8) -> Self {
        self.hour = hour.min(23);
        self
    }
    pub fn with_tier(mut self, tier: Tier) -> Self {
        self.tier = tier;
        self
    }
}

// ---------------------------------------------------------------------------
// A named tree, and a set of them
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Behaviour {
    pub name: String,
    /// Evaluated even while she is busy with an antic, and clears the antic if
    /// it does anything. This is the priority-inversion guard for behaviour:
    /// the focus warden must not be stuck behind a nap.
    #[serde(default)]
    pub preempt: bool,
    /// Skip this tree at or below this tier. `Lobotomised` still runs trees —
    /// canned behaviour is the cheap thing she keeps (SPEC §3.1) — but a skin
    /// can mark expensive antics as needing headroom.
    #[serde(default)]
    pub min_tier: Option<Tier>,
    pub root: Node,
}

impl Behaviour {
    pub fn new(name: impl Into<String>, root: Node) -> Self {
        Behaviour { name: name.into(), preempt: false, min_tier: None, root }
    }
    pub fn preempting(mut self) -> Self {
        self.preempt = true;
        self
    }
    pub fn needs_tier(mut self, tier: Tier) -> Self {
        self.min_tier = Some(tier);
        self
    }
}

/// An ordered set of trees — the whole of her scripted personality.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct BehaviourSet {
    pub trees: Vec<Behaviour>,
}

impl BehaviourSet {
    pub fn get(&self, name: &str) -> Option<&Behaviour> {
        self.trees.iter().find(|t| t.name == name)
    }
}

// ---------------------------------------------------------------------------
// The runner
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BtRunner {
    rng: Rng,
    /// key -> the time the cooldown lifts.
    cooldowns: BTreeMap<String, Millis>,
    /// key -> when it was last marked.
    marks: BTreeMap<String, Millis>,
    busy_until: Millis,
    pub session: Session,
    pub ticks: u64,
}

impl Default for BtRunner {
    fn default() -> Self {
        BtRunner::new(Rng::default())
    }
}

impl BtRunner {
    pub fn new(rng: Rng) -> Self {
        BtRunner {
            rng,
            cooldowns: BTreeMap::new(),
            marks: BTreeMap::new(),
            busy_until: 0,
            session: Session::default(),
            ticks: 0,
        }
    }

    pub fn seeded(seed: u64) -> Self {
        BtRunner::new(Rng::new(seed))
    }

    pub fn busy(&self, now: Millis) -> bool {
        now < self.busy_until
    }

    pub fn busy_until(&self) -> Millis {
        self.busy_until
    }

    /// Stop whatever antic she is in the middle of.
    pub fn interrupt(&mut self) {
        self.busy_until = 0;
    }

    pub fn cooldown_ready_at(&self, key: &str) -> Option<Millis> {
        self.cooldowns.get(key).copied()
    }

    pub fn mark(&mut self, key: &str, now: Millis) {
        self.marks.insert(key.to_string(), now);
    }

    pub fn marked_at(&self, key: &str) -> Option<Millis> {
        self.marks.get(key).copied()
    }

    /// Run one tree.
    pub fn tick(&mut self, root: &Node, ctx: &BtCtx, out: &mut Vec<Effect>) -> Status {
        self.ticks += 1;
        self.eval(root, ctx, out)
    }

    /// Run a whole set, in order. Preempting trees go first and may cut short
    /// an antic in progress; the rest are skipped while she is busy.
    pub fn run(&mut self, set: &BehaviourSet, ctx: &BtCtx) -> Vec<Effect> {
        let mut out = Vec::new();
        let mut preempted = false;
        for tree in set.trees.iter().filter(|t| t.preempt) {
            if !self.tier_allows(tree, ctx) {
                continue;
            }
            let before = out.len();
            let status = self.tick(&tree.root, ctx, &mut out);
            if status != Status::Failure && out.len() > before {
                preempted = true;
            }
        }
        if preempted {
            // Something urgent happened: she stops what she was doing, and
            // nothing leisurely starts on the same tick.
            self.busy_until = self.busy_until.min(ctx.now);
            return out;
        }
        if self.busy(ctx.now) {
            return out;
        }
        for tree in set.trees.iter().filter(|t| !t.preempt) {
            if !self.tier_allows(tree, ctx) {
                continue;
            }
            let status = self.tick(&tree.root, ctx, &mut out);
            if status == Status::Running || self.busy(ctx.now) {
                break;
            }
        }
        out
    }

    fn tier_allows(&self, tree: &Behaviour, ctx: &BtCtx) -> bool {
        match tree.min_tier {
            // `Tier` orders most-capable first, so "at least this tier" is <=.
            Some(min) => ctx.tier <= min,
            None => true,
        }
    }

    fn eval(&mut self, node: &Node, ctx: &BtCtx, out: &mut Vec<Effect>) -> Status {
        match node {
            Node::Sequence(children) => {
                for c in children {
                    match self.eval(c, ctx, out) {
                        Status::Success => {}
                        other => return other,
                    }
                }
                Status::Success
            }
            Node::Selector(children) => {
                for c in children {
                    match self.eval(c, ctx, out) {
                        Status::Failure => {}
                        other => return other,
                    }
                }
                Status::Failure
            }
            Node::Parallel { policy, children } => {
                let mut any_success = false;
                let mut any_running = false;
                let mut any_failure = false;
                for c in children {
                    match self.eval(c, ctx, out) {
                        Status::Success => any_success = true,
                        Status::Running => any_running = true,
                        Status::Failure => any_failure = true,
                    }
                }
                match policy {
                    ParallelPolicy::RequireAll => {
                        if any_failure {
                            Status::Failure
                        } else if any_running {
                            Status::Running
                        } else {
                            Status::Success
                        }
                    }
                    ParallelPolicy::RequireOne => {
                        if any_success {
                            Status::Success
                        } else if any_running {
                            Status::Running
                        } else {
                            Status::Failure
                        }
                    }
                }
            }
            Node::Condition(c) => {
                if self.check(c, ctx) {
                    Status::Success
                } else {
                    Status::Failure
                }
            }
            Node::Action(a) => self.act(a, ctx, out),
            Node::Cooldown { key, ms, child } => {
                if self.cooldowns.get(key).is_some_and(|ready| ctx.now < *ready) {
                    return Status::Failure;
                }
                let status = self.eval(child, ctx, out);
                if status == Status::Success {
                    self.cooldowns.insert(key.clone(), ctx.now.saturating_add(*ms));
                }
                status
            }
            Node::Random { choices } => {
                if choices.is_empty() {
                    return Status::Failure;
                }
                let weights: Vec<u32> = choices.iter().map(|c| c.weight).collect();
                match self.rng.weighted(&weights) {
                    Some(i) => self.eval(&choices[i].node, ctx, out),
                    None => Status::Failure,
                }
            }
            Node::Invert(child) => match self.eval(child, ctx, out) {
                Status::Success => Status::Failure,
                Status::Failure => Status::Success,
                Status::Running => Status::Running,
            },
        }
    }

    fn check(&mut self, c: &Condition, ctx: &BtCtx) -> bool {
        let w = ctx.world;
        match c {
            Condition::Always => true,
            Condition::Never => false,
            Condition::IdleAtLeast { ms } => w.idle && w.idle_ms(ctx.now) >= *ms,
            Condition::Active => !w.idle,
            Condition::FocusIs { app_id } => w.app == *app_id,
            Condition::FocusContains { needle } => {
                let n = needle.to_lowercase();
                w.app.to_lowercase().contains(&n) || w.title.to_lowercase().contains(&n)
            }
            Condition::FocusHeldAtLeast { ms } => w.focus_held(ctx.now) >= *ms,
            Condition::WorkspaceIs { name } => w.workspace == *name,
            Condition::WindowsAtLeast { n } => w.windows.len() as u32 >= *n,
            Condition::MediaPlaying => w.media_playing,
            Condition::MicLive => w.mic_live,
            Condition::NotificationWithin { ms } => {
                matches!(&w.last_notification, Some((at, _, _)) if ctx.now.saturating_sub(*at) <= *ms)
            }
            Condition::SpokeToHerWithin { ms } => {
                matches!(&w.last_speech, Some((at, _)) if ctx.now.saturating_sub(*at) <= *ms)
            }
            Condition::FilesDirty => !w.dirty_paths.is_empty(),
            Condition::FleetSaid { app, field, value } => {
                match w.fleet.get(&(app.clone(), field.clone())) {
                    Some(v) => value.is_empty() || v == value,
                    None => false,
                }
            }
            Condition::CpuAbove { pct } => w.cpu_pct > *pct,
            Condition::GpuAbove { pct } => w.gpu_pct > *pct,
            Condition::TempAbove { c } => w.temp_c > *c,
            Condition::OnBattery => w.on_battery,
            Condition::TierAtMost { tier } => ctx.tier >= *tier,
            Condition::FlowAbove { conf } => ctx.moment.flow > *conf,
            Condition::FlowBelow { conf } => ctx.moment.flow < *conf,
            Condition::OpeningWithin { ms } => ctx.moment.opening_within(ctx.now, *ms).is_some(),
            Condition::OpeningIs { kind, ms } => {
                ctx.moment.opening_within(ctx.now, *ms) == Some(*kind)
            }
            Condition::Silenced => ctx.moment.silenced,
            Condition::MoodIs { mood } => ctx.mood == *mood,
            Condition::EnergyAbove { v } => ctx.energy > *v,
            Condition::EnergyBelow { v } => ctx.energy < *v,
            Condition::ChattinessAtLeast { level } => ctx.chattiness >= *level,
            Condition::HourBetween { from, to } => hour_between(ctx.hour, *from, *to),
            Condition::FavouriteHour => ctx.relationship.favourite_hour() == Some(ctx.hour),
            Condition::DaysOwnedAtLeast { days } => ctx.relationship.days_owned >= *days,
            Condition::PettedAtLeast { times } => ctx.relationship.pets >= *times,
            Condition::ThrownAtLeast { times } => ctx.relationship.throws >= *times,
            Condition::SinceMark { key, ms } => {
                matches!(self.marks.get(key), Some(at) if ctx.now.saturating_sub(*at) >= *ms)
            }
            Condition::NoMark { key } => !self.marks.contains_key(key),
            Condition::SessionIs { state } => self.session.state == *state,
            Condition::SessionElapsedAtLeast { ms } => {
                self.session.state != SessionState::Off && self.session.elapsed(ctx.now) >= *ms
            }
            Condition::SessionOverdue => self.session.overdue(ctx.now),
            Condition::Chance { permille } => self.rng.chance_permille(*permille),
            Condition::Not(inner) => !self.check(inner, ctx),
            // `All`/`Any` evaluate every member instead of short-circuiting: a
            // `Chance` buried in one of them must draw the same number of times
            // whatever its neighbours answered, or the RNG stream stops being
            // reproducible.
            Condition::All(list) => {
                let mut all = true;
                for c in list {
                    if !self.check(c, ctx) {
                        all = false;
                    }
                }
                all
            }
            Condition::Any(list) => {
                let mut any = false;
                for c in list {
                    if self.check(c, ctx) {
                        any = true;
                    }
                }
                any
            }
        }
    }

    fn act(&mut self, a: &Action, ctx: &BtCtx, out: &mut Vec<Effect>) -> Status {
        match a {
            Action::Say { text, urgency, expression, stale_after_ms, defer_ms } => {
                let mut u = Utterance::new(text.clone(), *urgency);
                u.expression = expression.clone();
                u.stale_after = stale_after_ms.map(|d| ctx.now.saturating_add(d));
                u.defer_until = defer_ms.map(|d| ctx.now.saturating_add(d));
                out.push(Effect::Propose(u));
                Status::Success
            }
            Action::SayOneOf { lines, urgency, expression, stale_after_ms } => {
                if lines.is_empty() {
                    return Status::Failure;
                }
                let i = self.rng.below(lines.len() as u64) as usize;
                let mut u = Utterance::new(lines[i].clone(), *urgency);
                u.expression = expression.clone();
                u.stale_after = stale_after_ms.map(|d| ctx.now.saturating_add(d));
                out.push(Effect::Propose(u));
                Status::Success
            }
            Action::Clip { name, loop_, hold_ms } => {
                out.push(Effect::Clip { name: name.clone(), loop_: *loop_ });
                self.occupy(ctx.now, *hold_ms);
                Status::Success
            }
            Action::Move { to, hold_ms } => {
                out.push(Effect::Move(to.clone()));
                self.occupy(ctx.now, *hold_ms);
                Status::Success
            }
            Action::Poke { target } => {
                let window = match target {
                    PokeTarget::Active => None,
                    PokeTarget::Largest => ctx.world.largest_window().map(|(id, _)| id),
                    PokeTarget::Id(id) => Some(*id),
                };
                out.push(Effect::Poke { window });
                Status::Success
            }
            Action::SetMood { mood } => {
                out.push(Effect::Mood(*mood));
                Status::Success
            }
            Action::Mark { key } => {
                self.marks.insert(key.clone(), ctx.now);
                Status::Success
            }
            Action::Tag { name } => {
                out.push(Effect::Tag(name.clone()));
                Status::Success
            }
            // The one action that stops a sequence where it stands.
            Action::Wait { ms } => {
                if self.occupy(ctx.now, *ms) {
                    Status::Running
                } else {
                    Status::Success
                }
            }
            Action::StartSession { minutes } => {
                self.session = Session {
                    state: SessionState::Work,
                    started: ctx.now,
                    ends: ctx.now.saturating_add(*minutes as Millis * 60_000),
                    completed: self.session.completed,
                };
                out.push(Effect::Session(SessionEvent::Started { minutes: *minutes }));
                Status::Success
            }
            Action::StartBreak { minutes } => {
                let completed = self.session.completed
                    + u32::from(self.session.state == SessionState::Work);
                self.session = Session {
                    state: SessionState::Break,
                    started: ctx.now,
                    ends: ctx.now.saturating_add(*minutes as Millis * 60_000),
                    completed,
                };
                out.push(Effect::Session(SessionEvent::BreakStarted { minutes: *minutes }));
                Status::Success
            }
            Action::EndSession => {
                if self.session.state == SessionState::Off {
                    return Status::Failure;
                }
                self.session.state = SessionState::Off;
                self.session.ends = ctx.now;
                out.push(Effect::Session(SessionEvent::Ended));
                Status::Success
            }
        }
    }

    /// Mark her busy with an antic for a while. Returns whether it took.
    ///
    /// Note what this does *not* do: it does not return `Running`. A clip or a
    /// move inside a sequence must not abort the rest of the sequence — "walk
    /// over, poke it, then say something" has to reach the something. The hold
    /// stops *other* trees from starting a second antic on top of this one,
    /// which is the thing that actually looks broken on screen.
    fn occupy(&mut self, now: Millis, hold_ms: Millis) -> bool {
        if hold_ms == 0 {
            return false;
        }
        self.busy_until = self.busy_until.max(now.saturating_add(hold_ms));
        true
    }
}

/// Inclusive of `from`, exclusive of `to`, and wraps around midnight so
/// `HourBetween { from: 22, to: 3 }` means what an author expects.
fn hour_between(hour: u8, from: u8, to: u8) -> bool {
    if from == to {
        return true;
    }
    if from < to {
        hour >= from && hour < to
    } else {
        hour >= from || hour < to
    }
}

// ---------------------------------------------------------------------------
// Small authoring helpers. These build data; they are not a scripting hook.
// ---------------------------------------------------------------------------

pub fn seq(children: Vec<Node>) -> Node {
    Node::Sequence(children)
}
pub fn sel(children: Vec<Node>) -> Node {
    Node::Selector(children)
}
pub fn cond(c: Condition) -> Node {
    Node::Condition(c)
}
pub fn act(a: Action) -> Node {
    Node::Action(a)
}
pub fn cooldown(key: &str, ms: Millis, child: Node) -> Node {
    Node::Cooldown { key: key.to_string(), ms, child: Box::new(child) }
}
pub fn random(choices: Vec<(u32, Node)>) -> Node {
    Node::Random {
        choices: choices.into_iter().map(|(weight, node)| Weighted { weight, node }).collect(),
    }
}
pub fn clip(name: &str, hold_ms: Millis) -> Node {
    act(Action::Clip { name: name.to_string(), loop_: false, hold_ms })
}
pub fn say(text: &str, urgency: Urgency) -> Node {
    act(Action::Say {
        text: text.to_string(),
        urgency,
        expression: None,
        stale_after_ms: None,
        defer_ms: None,
    })
}
pub fn say_one_of(lines: &[&str], urgency: Urgency, stale_after_ms: Option<Millis>) -> Node {
    act(Action::SayOneOf {
        lines: lines.iter().map(|s| s.to_string()).collect(),
        urgency,
        expression: None,
        stale_after_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::isolate;
    use wisp_proto::Observation;

    fn rel() -> Relationship {
        Relationship::default()
    }

    struct Fixture {
        world: World,
        moment: Moment,
        rel: Relationship,
    }

    impl Fixture {
        fn new() -> Self {
            Fixture { world: World::default(), moment: Moment::free(), rel: rel() }
        }
        fn ctx(&self, now: Millis) -> BtCtx<'_> {
            BtCtx::new(now, &self.world, &self.moment, &self.rel)
        }
    }

    #[test]
    fn sequence_stops_at_the_first_failure() {
        isolate();
        let f = Fixture::new();
        let mut r = BtRunner::seeded(1);
        let tree = seq(vec![
            act(Action::Tag { name: "one".into() }),
            cond(Condition::Never),
            act(Action::Tag { name: "two".into() }),
        ]);
        let mut out = Vec::new();
        assert_eq!(r.tick(&tree, &f.ctx(0), &mut out), Status::Failure);
        assert_eq!(out, vec![Effect::Tag("one".into())]);
    }

    #[test]
    fn selector_takes_the_first_success() {
        isolate();
        let f = Fixture::new();
        let mut r = BtRunner::seeded(1);
        let tree = sel(vec![
            seq(vec![cond(Condition::Never), act(Action::Tag { name: "no".into() })]),
            act(Action::Tag { name: "yes".into() }),
            act(Action::Tag { name: "unreached".into() }),
        ]);
        let mut out = Vec::new();
        assert_eq!(r.tick(&tree, &f.ctx(0), &mut out), Status::Success);
        assert_eq!(out, vec![Effect::Tag("yes".into())]);
    }

    #[test]
    fn parallel_policies() {
        isolate();
        let f = Fixture::new();
        let mut r = BtRunner::seeded(1);
        let all = Node::Parallel {
            policy: ParallelPolicy::RequireAll,
            children: vec![cond(Condition::Always), cond(Condition::Never)],
        };
        let one = Node::Parallel {
            policy: ParallelPolicy::RequireOne,
            children: vec![cond(Condition::Always), cond(Condition::Never)],
        };
        let mut out = Vec::new();
        assert_eq!(r.tick(&all, &f.ctx(0), &mut out), Status::Failure);
        assert_eq!(r.tick(&one, &f.ctx(0), &mut out), Status::Success);
    }

    #[test]
    fn invert_flips_success_and_failure_but_not_running() {
        isolate();
        let f = Fixture::new();
        let mut r = BtRunner::seeded(1);
        let mut out = Vec::new();
        assert_eq!(
            r.tick(&Node::Invert(Box::new(cond(Condition::Always))), &f.ctx(0), &mut out),
            Status::Failure
        );
        assert_eq!(
            r.tick(&Node::Invert(Box::new(cond(Condition::Never))), &f.ctx(0), &mut out),
            Status::Success
        );
        let running = Node::Invert(Box::new(act(Action::Wait { ms: 1_000 })));
        assert_eq!(r.tick(&running, &f.ctx(0), &mut out), Status::Running);
    }

    #[test]
    fn a_cooldown_blocks_until_it_lifts_and_only_arms_on_success() {
        isolate();
        let f = Fixture::new();
        let mut r = BtRunner::seeded(1);
        let tree = cooldown("poke", 10_000, act(Action::Tag { name: "poke".into() }));
        let mut out = Vec::new();
        assert_eq!(r.tick(&tree, &f.ctx(0), &mut out), Status::Success);
        assert_eq!(r.tick(&tree, &f.ctx(5_000), &mut out), Status::Failure);
        assert_eq!(r.tick(&tree, &f.ctx(9_999), &mut out), Status::Failure);
        assert_eq!(r.tick(&tree, &f.ctx(10_000), &mut out), Status::Success);
        assert_eq!(out.len(), 2);

        // A child that fails must not arm the cooldown.
        let mut r2 = BtRunner::seeded(1);
        let failing = cooldown("never", 10_000, cond(Condition::Never));
        assert_eq!(r2.tick(&failing, &f.ctx(0), &mut Vec::new()), Status::Failure);
        assert_eq!(r2.cooldown_ready_at("never"), None);
    }

    #[test]
    fn cooldown_keys_are_shared_between_trees() {
        isolate();
        let f = Fixture::new();
        let mut r = BtRunner::seeded(1);
        let a = cooldown("antic", 60_000, act(Action::Tag { name: "a".into() }));
        let b = cooldown("antic", 60_000, act(Action::Tag { name: "b".into() }));
        let mut out = Vec::new();
        assert_eq!(r.tick(&a, &f.ctx(0), &mut out), Status::Success);
        assert_eq!(r.tick(&b, &f.ctx(0), &mut out), Status::Failure, "one budget, two trees");
    }

    #[test]
    fn weighted_choice_is_deterministic_under_a_seed() {
        isolate();
        let f = Fixture::new();
        let tree = random(vec![
            (1, act(Action::Tag { name: "rare".into() })),
            (9, act(Action::Tag { name: "common".into() })),
        ]);
        let roll = |seed: u64| {
            let mut r = BtRunner::seeded(seed);
            let mut out = Vec::new();
            for i in 0..50 {
                r.tick(&tree, &f.ctx(i * 1_000), &mut out);
            }
            out
        };
        assert_eq!(roll(7), roll(7));
        assert_ne!(roll(7), roll(8));
        let rare = roll(7).iter().filter(|e| **e == Effect::Tag("rare".into())).count();
        assert!((1..=15).contains(&rare), "1:9 weighting produced {rare}/50 rare picks");
    }

    #[test]
    fn an_empty_or_zero_weighted_random_fails_instead_of_guessing() {
        isolate();
        let f = Fixture::new();
        let mut r = BtRunner::seeded(1);
        assert_eq!(r.tick(&random(vec![]), &f.ctx(0), &mut Vec::new()), Status::Failure);
        let zeroed = random(vec![(0, act(Action::Tag { name: "x".into() }))]);
        assert_eq!(r.tick(&zeroed, &f.ctx(0), &mut Vec::new()), Status::Failure);
    }

    #[test]
    fn a_tree_can_never_speak_directly() {
        isolate();
        let f = Fixture::new();
        let mut r = BtRunner::seeded(1);
        let mut out = Vec::new();
        r.tick(&say("hello", Urgency::Whim), &f.ctx(0), &mut out);
        assert!(matches!(out[0], Effect::Propose(_)), "speech must go via the budget");
    }

    #[test]
    fn actions_with_a_hold_make_her_busy_without_aborting_the_sequence() {
        isolate();
        let f = Fixture::new();
        let mut r = BtRunner::seeded(1);
        let mut out = Vec::new();
        // "walk over, poke it, then say something" must reach the something.
        let antic = seq(vec![
            act(Action::Move { to: MoveTarget::LargestWindowEdge, hold_ms: 2_000 }),
            clip("nap", 30_000),
            act(Action::Tag { name: "after".into() }),
        ]);
        assert_eq!(r.tick(&antic, &f.ctx(1_000), &mut out), Status::Success);
        assert_eq!(out.last(), Some(&Effect::Tag("after".into())));
        assert!(r.busy(20_000));
        assert!(!r.busy(31_001));
        r.interrupt();
        assert!(!r.busy(2_000));
        // `Wait` is the node that does stop a sequence where it stands.
        let mut out = Vec::new();
        let waiting = seq(vec![
            act(Action::Wait { ms: 5_000 }),
            act(Action::Tag { name: "unreached".into() }),
        ]);
        assert_eq!(r.tick(&waiting, &f.ctx(1_000), &mut out), Status::Running);
        assert!(out.is_empty());
    }

    #[test]
    fn conditions_read_the_observation_stream() {
        isolate();
        let mut f = Fixture::new();
        f.world.observe(0, &Observation::Focus { app_id: "org.kde.kate".into(), title: "x".into() });
        f.world.observe(0, &Observation::Vitals {
            cpu_pct: 91,
            gpu_pct: 10,
            vram_used_mib: 100,
            temp_c: 80,
            on_battery: true,
        });
        f.world.observe(0, &Observation::Idle { idle: true, for_ms: 120_000 });
        let mut r = BtRunner::seeded(1);
        let ctx = f.ctx(60_000);
        assert!(r.check(&Condition::FocusIs { app_id: "org.kde.kate".into() }, &ctx));
        assert!(r.check(&Condition::FocusContains { needle: "KATE".into() }, &ctx));
        assert!(r.check(&Condition::IdleAtLeast { ms: 180_000 }, &ctx));
        assert!(!r.check(&Condition::IdleAtLeast { ms: 181_000 }, &ctx));
        assert!(!r.check(&Condition::Active, &ctx));
        assert!(r.check(&Condition::CpuAbove { pct: 90 }, &ctx));
        assert!(!r.check(&Condition::CpuAbove { pct: 91 }, &ctx));
        assert!(r.check(&Condition::OnBattery, &ctx));
        assert!(r.check(&Condition::Not(Box::new(Condition::MediaPlaying)), &ctx));
        assert!(r.check(
            &Condition::All(vec![Condition::OnBattery, Condition::TempAbove { c: 70 }]),
            &ctx
        ));
        assert!(!r.check(&Condition::Any(vec![Condition::Never, Condition::MicLive]), &ctx));
    }

    #[test]
    fn tier_conditions_and_gating_use_the_ladder_the_right_way_round() {
        isolate();
        let f = Fixture::new();
        let mut r = BtRunner::seeded(1);
        let ctx = f.ctx(0).with_tier(Tier::Lobotomised);
        // "at most this much machine" — T3 is less than or equal to T3.
        assert!(r.check(&Condition::TierAtMost { tier: Tier::Reduced }, &ctx));
        assert!(!r.check(&Condition::TierAtMost { tier: Tier::Dormant }, &ctx));
        let expensive = Behaviour::new("x", cond(Condition::Always)).needs_tier(Tier::Full);
        assert!(!r.tier_allows(&expensive, &ctx));
        assert!(r.tier_allows(&expensive, &f.ctx(0).with_tier(Tier::Full)));
    }

    #[test]
    fn hour_ranges_wrap_around_midnight() {
        isolate();
        assert!(hour_between(23, 22, 3));
        assert!(hour_between(2, 22, 3));
        assert!(!hour_between(3, 22, 3));
        assert!(!hour_between(12, 22, 3));
        assert!(hour_between(10, 9, 12));
        assert!(hour_between(5, 5, 5), "from == to means always");
    }

    #[test]
    fn marks_remember_what_she_has_done() {
        isolate();
        let f = Fixture::new();
        let mut r = BtRunner::seeded(1);
        let ctx = f.ctx(0);
        assert!(r.check(&Condition::NoMark { key: "greeted".into() }, &ctx));
        r.tick(&act(Action::Mark { key: "greeted".into() }), &ctx, &mut Vec::new());
        assert!(!r.check(&Condition::NoMark { key: "greeted".into() }, &ctx));
        assert!(!r.check(&Condition::SinceMark { key: "greeted".into(), ms: 1_000 }, &ctx));
        let later = f.ctx(5_000);
        assert!(r.check(&Condition::SinceMark { key: "greeted".into(), ms: 1_000 }, &later));
    }

    #[test]
    fn the_focus_warden_keeps_its_own_clock() {
        isolate();
        let f = Fixture::new();
        let mut r = BtRunner::seeded(1);
        let mut out = Vec::new();
        r.tick(&act(Action::StartSession { minutes: 25 }), &f.ctx(0), &mut out);
        assert_eq!(out, vec![Effect::Session(SessionEvent::Started { minutes: 25 })]);
        assert_eq!(r.session.state, SessionState::Work);
        assert!(!r.check(&Condition::SessionOverdue, &f.ctx(24 * 60_000)));
        assert!(r.check(&Condition::SessionOverdue, &f.ctx(25 * 60_000)));
        assert!(r.check(&Condition::SessionElapsedAtLeast { ms: 60_000 }, &f.ctx(60_000)));
        r.tick(&act(Action::StartBreak { minutes: 5 }), &f.ctx(25 * 60_000), &mut out);
        assert_eq!(r.session.completed, 1);
        assert_eq!(r.session.state, SessionState::Break);
        r.tick(&act(Action::EndSession), &f.ctx(30 * 60_000), &mut out);
        assert_eq!(r.session.state, SessionState::Off);
        assert!(!r.check(&Condition::SessionOverdue, &f.ctx(99 * 60_000)));
        // Ending a session that never started is a failure, not a no-op.
        assert_eq!(
            r.tick(&act(Action::EndSession), &f.ctx(31 * 60_000), &mut out),
            Status::Failure
        );
    }

    #[test]
    fn a_preempting_tree_runs_even_while_she_is_busy() {
        isolate();
        let f = Fixture::new();
        let set = BehaviourSet {
            trees: vec![
                Behaviour::new("warden", act(Action::Tag { name: "nag".into() })).preempting(),
                Behaviour::new("nap", clip("nap", 60_000)),
            ],
        };
        let mut r = BtRunner::seeded(1);
        // She is mid-nap...
        r.tick(&clip("nap", 60_000), &f.ctx(0), &mut Vec::new());
        assert!(r.busy(30_000));
        // ...and the warden nags anyway, cutting the nap short.
        let out = r.run(&set, &f.ctx(30_000));
        assert_eq!(out, vec![Effect::Tag("nag".into())]);
        assert!(!r.busy(30_000), "an urgent tree interrupts an antic");

        // When the warden has nothing to say, the leisurely tree gets its turn.
        let quiet = BehaviourSet {
            trees: vec![
                Behaviour::new("warden", cond(Condition::Never)).preempting(),
                Behaviour::new("nap", clip("nap", 60_000)),
            ],
        };
        let out = r.run(&quiet, &f.ctx(31_000));
        assert_eq!(out, vec![Effect::Clip { name: "nap".into(), loop_: false }]);
    }

    #[test]
    fn non_preempting_trees_are_skipped_while_busy() {
        isolate();
        let f = Fixture::new();
        let set = BehaviourSet {
            trees: vec![
                Behaviour::new("nap", clip("nap", 60_000)),
                Behaviour::new("wander", act(Action::Move { to: MoveTarget::Wander, hold_ms: 0 })),
            ],
        };
        let mut r = BtRunner::seeded(1);
        let out = r.run(&set, &f.ctx(0));
        assert_eq!(out.len(), 1, "the nap runs and nothing else starts: {out:?}");
        assert!(r.run(&set, &f.ctx(10_000)).is_empty());
        assert_eq!(r.run(&set, &f.ctx(61_000)).len(), 1);
    }

    #[test]
    fn the_whole_tree_language_round_trips_as_data() {
        isolate();
        let tree = Behaviour::new(
            "everything",
            sel(vec![
                Node::Parallel {
                    policy: ParallelPolicy::RequireOne,
                    children: vec![
                        cond(Condition::Any(vec![
                            Condition::MoodIs { mood: Mood::Playful },
                            Condition::Not(Box::new(Condition::Silenced)),
                        ])),
                        Node::Invert(Box::new(cond(Condition::Chance { permille: 250 }))),
                    ],
                },
                cooldown(
                    "k",
                    1_000,
                    random(vec![
                        (3, clip("wander", 2_000)),
                        (1, say_one_of(&["a", "b"], Urgency::Whim, Some(60_000))),
                    ]),
                ),
            ]),
        )
        .preempting()
        .needs_tier(Tier::Reduced);
        let json = serde_json::to_string(&tree).unwrap();
        let back: Behaviour = serde_json::from_str(&json).unwrap();
        assert_eq!(back, tree);
        assert!(!json.contains("fn "), "a skin can never carry code");
    }

    #[test]
    fn the_same_seed_and_trace_gives_the_same_run() {
        isolate();
        let tree = seq(vec![
            cond(Condition::Chance { permille: 500 }),
            random(vec![
                (1, act(Action::Tag { name: "a".into() })),
                (1, act(Action::Tag { name: "b".into() })),
                (1, say_one_of(&["x", "y", "z"], Urgency::Whim, None)),
            ]),
        ]);
        let run = || {
            let f = Fixture::new();
            let mut r = BtRunner::seeded(0xC0FFEE);
            let mut out = Vec::new();
            for i in 0..200 {
                r.tick(&tree, &f.ctx(i * 500), &mut out);
            }
            out
        };
        let a = run();
        assert_eq!(a, run());
        assert!(a.len() > 50, "the trace should actually do things: {}", a.len());
    }

    #[test]
    fn runner_state_round_trips() {
        isolate();
        let f = Fixture::new();
        let mut r = BtRunner::seeded(9);
        r.tick(&cooldown("k", 5_000, act(Action::Mark { key: "m".into() })), &f.ctx(0), &mut Vec::new());
        let json = serde_json::to_string(&r).unwrap();
        let back: BtRunner = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
        assert_eq!(back.cooldown_ready_at("k"), Some(5_000));
        assert_eq!(back.marked_at("m"), Some(0));
    }
}
