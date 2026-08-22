//! `wisp-attn` — the attention economy. SPEC §2 gives this crate the
//! interruption budget, flow detection and behaviour trees, and SPEC §3.4 gives
//! it the only door to the operator: *nothing reaches them except as an
//! [`Utterance`](wisp_proto::Utterance) submitted here.*
//!
//! # Purity
//!
//! This crate performs **no I/O of any kind**: no files, no D-Bus, no GPU, no
//! threads, and — deliberately — **no clock**. Every entry point takes `now:
//! Millis` and the local hour is supplied by the host. That makes her whole
//! judgement replayable: a trace of [`Observation`](wisp_proto::Observation)s
//! plus a seed produces exactly the same speech and exactly the same antics,
//! every time. That matters more here than anywhere else in the tree, because
//! the bugs in this crate are not crashes — they are felt as *"she is
//! annoying"*, and you cannot fix those without reproducing them.
//!
//! # The four pieces
//!
//! * [`budget`] (F36) — the token bucket, deferral, staleness, coalescing and
//!   the priority-inversion guard that keeps an `Alarm` from queueing behind
//!   idle chatter.
//! * [`flow`] (F37) — a *confidence*, not a verdict, that the operator is deep
//!   in something, plus the openings when speaking up is natural.
//! * [`bt`] + [`behaviours`] (F50, F38, F39, F40) — declarative, data-only
//!   behaviour trees: her antics, her commentary at each dial position, and the
//!   focus warden that nags in character instead of with a dialog box.
//! * [`relationship`] (F42) — what she remembers about the two of you.
//!
//! [`Attention`] wires them together, and is the only thing that turns a
//! proposal from a behaviour tree into speech.

pub mod behaviours;
pub mod bt;
pub mod budget;
pub mod flow;
pub mod relationship;
pub mod rng;
pub mod text;
pub mod world;

use serde::{Deserialize, Serialize};
use wisp_proto::{Cost, Event, Governed, Millis, Observation, Tier, TierReason, Utterance};

pub use bt::{
    Action, Behaviour, BehaviourSet, BtCtx, BtRunner, Condition, Effect, MoveTarget, Node,
    ParallelPolicy, PokeTarget, Session, SessionEvent, SessionState, Status, Weighted,
};
pub use budget::{Admission, Budget, BudgetConfig, DropReason, Held, HoldReason, UtteranceId};
pub use flow::{Flow, FlowConfig, FlowEstimator, Moment, Opportunity};
pub use relationship::{Bond, Interaction, Relationship, RelationshipConfig};
pub use rng::Rng;
pub use world::{Rect, World};

/// How much she talks. F39's dial.
///
/// Ordered, so a behaviour-tree condition can say "at least chatty".
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
pub enum Chattiness {
    /// She still answers, and still raises alarms. She volunteers nothing.
    Silent,
    /// The shipped default: a few remarks an hour, at good moments.
    #[default]
    Occasional,
    Chatty,
    /// You asked for this.
    Insufferable,
}

/// Her disposition. `wisp-mind` owns the authoritative mood FSM (SPEC §2); this
/// is the projection the behaviour trees read, and it is deliberately a small
/// closed set so a skin can name its clips after it.
///
/// *Contract note:* `wisp-proto` does not define a shared mood type, so this
/// one lives here. If mood ever becomes a cross-crate contract it belongs in
/// proto as a spec amendment, and this becomes a re-export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub enum Mood {
    #[default]
    Calm,
    Curious,
    Playful,
    Smug,
    Sulky,
    Focused,
    Sleepy,
    Alarmed,
    Affectionate,
}

/// One pass of her attention: what she wants to do, and what she got to say.
///
/// `effects` never contains [`Effect::Propose`] — a proposal that survived is
/// in `said`, and one that did not is in the flight recorder with a reason.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Turn {
    pub effects: Vec<Effect>,
    pub said: Vec<Utterance>,
}

impl Turn {
    pub fn is_empty(&self) -> bool {
        self.effects.is_empty() && self.said.is_empty()
    }
}

/// The whole crate, wired together: senses in, behaviour and speech out.
///
/// This is the enforcement point for SPEC §3.4. A behaviour tree cannot speak;
/// it can only propose, and every proposal goes through the budget on its way
/// out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attention {
    pub budget: Budget,
    pub flow: FlowEstimator,
    pub bt: BtRunner,
    pub world: World,
    pub relationship: Relationship,
    behaviours: BehaviourSet,
    mood: Mood,
    energy: f32,
    chattiness: Chattiness,
    hour: u8,
    tier: Tier,
    /// The last timestamp anything gave us, for the entry points the `Governed`
    /// trait does not let us pass a time to.
    now: Millis,
}

impl Default for Attention {
    fn default() -> Self {
        Attention::new(0)
    }
}

impl Attention {
    /// A fresh companion with the shipped behaviour set.
    pub fn new(seed: u64) -> Self {
        Attention::with_behaviours(seed, behaviours::default_set())
    }

    pub fn with_behaviours(seed: u64, behaviours: BehaviourSet) -> Self {
        let chattiness = Chattiness::default();
        Attention {
            budget: Budget::new(BudgetConfig::for_chattiness(chattiness), 0),
            flow: FlowEstimator::default(),
            bt: BtRunner::seeded(seed),
            world: World::default(),
            relationship: Relationship::default(),
            behaviours,
            mood: Mood::default(),
            energy: 0.5,
            chattiness,
            hour: 12,
            tier: Tier::Full,
            now: 0,
        }
    }

    pub fn behaviours(&self) -> &BehaviourSet {
        &self.behaviours
    }

    /// Hot-reload a skin's behaviour set. Antics in progress are abandoned;
    /// cooldowns are kept, so a reload cannot be used to skip them.
    pub fn set_behaviours(&mut self, set: BehaviourSet) {
        self.behaviours = set;
        self.bt.interrupt();
    }

    pub fn chattiness(&self) -> Chattiness {
        self.chattiness
    }

    /// Turn the dial. Takes effect on the bucket immediately.
    pub fn set_chattiness(&mut self, dial: Chattiness) {
        self.chattiness = dial;
        self.budget.set_chattiness(dial);
    }

    /// The host supplies the local hour. This crate never reads a clock.
    pub fn set_hour(&mut self, hour: u8) {
        self.hour = hour.min(23);
        self.flow.set_hour(hour);
    }

    pub fn hour(&self) -> u8 {
        self.hour
    }

    pub fn mood(&self) -> Mood {
        self.mood
    }

    pub fn set_mood(&mut self, mood: Mood) {
        self.mood = mood;
    }

    pub fn energy(&self) -> f32 {
        self.energy
    }

    pub fn set_energy(&mut self, energy: f32) {
        self.energy = energy.clamp(0.0, 1.0);
    }

    pub fn tier(&self) -> Tier {
        self.tier
    }

    /// Mute her by hand. Alarms still land — that is what an alarm is.
    pub fn set_silenced(&mut self, silenced: bool) {
        self.flow.set_silenced(silenced);
    }

    pub fn silenced(&self) -> bool {
        self.flow.silenced()
    }

    /// Feed her the senses.
    pub fn observe(&mut self, now: Millis, obs: &Observation) {
        self.now = now;
        self.world.observe(now, obs);
        self.flow.observe(now, obs);
    }

    /// Offer something for her to say (F36). Nothing is spoken until [`tick`].
    ///
    /// [`tick`]: Attention::tick
    pub fn submit(&mut self, now: Millis, u: Utterance) -> Admission {
        self.now = now;
        self.budget.submit(now, u)
    }

    /// The operator did something to her (F42). Being interacted with is
    /// always an opening — she may answer.
    pub fn interact(&mut self, now: Millis, what: Interaction) {
        self.now = now;
        self.relationship.record(self.hour, what);
        if what == Interaction::Summon {
            self.flow.note_opportunity(now, Opportunity::Asked);
        }
        if what == Interaction::Dismiss {
            self.bt.interrupt();
        }
    }

    /// Her current read of the moment.
    pub fn moment(&self, now: Millis) -> Moment {
        self.flow.moment(now)
    }

    pub fn estimate(&self, now: Millis) -> Flow {
        self.flow.estimate(now)
    }

    /// One pass: run the behaviour trees, put everything they proposed through
    /// the interruption budget, and return what she actually does and says.
    pub fn tick(&mut self, now: Millis) -> Turn {
        self.now = now;
        let moment = self.flow.moment(now);
        let ctx = BtCtx::new(now, &self.world, &moment, &self.relationship)
            .with_mood(self.mood)
            .with_energy(self.energy)
            .with_chattiness(self.chattiness)
            .with_hour(self.hour)
            .with_tier(self.tier);
        let raw = self.bt.run(&self.behaviours, &ctx);

        let mut effects = Vec::with_capacity(raw.len());
        for e in raw {
            match e {
                // The one door to the operator (SPEC §3.4).
                Effect::Propose(u) => {
                    self.budget.submit(now, u);
                }
                Effect::Mood(m) => {
                    self.mood = m;
                    effects.push(Effect::Mood(m));
                }
                other => effects.push(other),
            }
        }
        let said = self.budget.pump(now, &moment);
        Turn { effects, said }
    }

    /// Everything that happened, for the flight recorder. Facts about the past.
    pub fn drain_events(&mut self) -> Vec<Event> {
        self.budget.drain_events()
    }
}

/// SPEC §3.1. Downgrades are synchronous and immediate; there is nothing here
/// that can fail or block, because there is nothing here that touches hardware.
impl Governed for Attention {
    fn set_tier(&mut self, tier: Tier, reason: &TierReason) {
        let now = self.now;
        self.tier = tier;
        match tier {
            // Explicitly silenced. She sheds every costed thought rather than
            // hoarding an hour of chatter to unload when she wakes up, stops
            // whatever antic she was in, and says nothing but alarms.
            Tier::Dormant => {
                self.flow.set_silenced(true);
                self.bt.interrupt();
                let why = format!("governor: dormant ({reason:?})");
                self.budget.shed_chatter(now, &why);
            }
            // A game owns the GPU. She keeps her canned behaviour and her
            // queue — trees with a `min_tier` sit this one out — but she stops
            // whatever she was in the middle of, because it costs frames.
            Tier::Lobotomised => {
                self.flow.set_silenced(false);
                self.bt.interrupt();
            }
            _ => self.flow.set_silenced(false),
        }
    }

    fn cost_at(tier: Tier) -> Cost {
        match tier {
            Tier::Dormant => Cost::FREE,
            // A few queued utterances, a behaviour set and five minutes of
            // observation timestamps. It is all small and all bounded.
            _ => Cost { ram_mib: 1, vram_mib: 0, cpu_centi_pct: 5 },
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Once;

    static ONCE: Once = Once::new();

    /// SPEC §4: **every** test sets `NX_WISP_CONFIG_DIR` to a temp dir. This
    /// crate reads and writes nothing, so nothing here consults it — but the
    /// rule has no exceptions for a reason (NX Orbit, 2026-08-20), and a future
    /// dependency that starts reading it must find the isolated path already
    /// set rather than the operator's real one.
    pub fn isolate() {
        ONCE.call_once(|| {
            let dir = std::env::temp_dir().join(format!("nx-wisp-attn-test-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            std::env::set_var("NX_WISP_CONFIG_DIR", &dir);
        });
        debug_assert!(std::env::var_os("NX_WISP_CONFIG_DIR").is_some());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::isolate;
    use wisp_proto::{EventKind, Urgency};

    fn typing(t: Millis) -> (Millis, Observation) {
        (t, Observation::Idle { idle: false, for_ms: 0 })
    }

    /// Twenty minutes of steady work in one window.
    fn deep_work(a: &mut Attention, from: Millis) -> Millis {
        a.set_hour(10);
        a.observe(from, &Observation::Focus { app_id: "org.kde.kate".into(), title: "x".into() });
        let mut t = from;
        while t < from + 1_200_000 {
            t += 2_000;
            let (at, obs) = typing(t);
            a.observe(at, &obs);
        }
        t
    }

    #[test]
    fn a_behaviour_tree_can_never_speak_around_the_budget() {
        isolate();
        let insufferable = BehaviourSet {
            trees: vec![Behaviour::new(
                "shouting",
                bt::say("I have thoughts and I will share them", Urgency::Whim),
            )],
        };
        let mut a = Attention::with_behaviours(1, insufferable);
        a.set_chattiness(Chattiness::Insufferable);
        let mut said = 0;
        for i in 0..3_600u64 {
            // A busy desk: openings all over the place, so nothing but the
            // budget itself is holding her back.
            if i % 60 == 0 {
                a.observe(i * 1_000, &Observation::Focus {
                    app_id: format!("app{}", i % 4),
                    title: "t".into(),
                });
            }
            let turn = a.tick(i * 1_000);
            assert!(
                !turn.effects.iter().any(|e| matches!(e, Effect::Propose(_))),
                "a proposal escaped the budget"
            );
            said += turn.said.len();
        }
        // A tree that tries to speak every second for an hour still cannot
        // spend more attention than the bucket holds: 30 tokens up front plus
        // 30 refilled, at three a whim.
        assert!(said <= 20, "she said {said} things in an hour with the dial at maximum");
        assert!(said > 0, "and she did get to say something");
    }

    #[test]
    fn an_alarm_lands_even_mid_flow_mid_nap_and_out_of_budget() {
        isolate();
        let mut a = Attention::new(2);
        let t = deep_work(&mut a, 0);
        assert!(a.estimate(t).confidence > 0.7, "{:?}", a.estimate(t));
        // Burn the bucket.
        for i in 0..20 {
            a.submit(t, Utterance::new(format!("chatter {}", "x".repeat(i + 3)), Urgency::Whim));
        }
        a.tick(t);
        let alarm_at = t + 1_000;
        a.submit(alarm_at, Utterance::new("the disk is full", Urgency::Alarm));
        let turn = a.tick(alarm_at);
        assert_eq!(turn.said.len(), 1);
        assert_eq!(turn.said[0].text, "the disk is full");
    }

    #[test]
    fn she_holds_her_tongue_in_flow_and_speaks_when_they_surface() {
        isolate();
        let mut a = Attention::new(3);
        let t = deep_work(&mut a, 0);
        a.submit(t, Utterance::new("your branch is behind origin", Urgency::Notable));
        assert!(a.tick(t).said.is_empty(), "she interrupted deep work");
        // They go and make coffee.
        let t = t + 60_000;
        a.observe(t, &Observation::Idle { idle: true, for_ms: 120_000 });
        let turn = a.tick(t);
        assert_eq!(turn.said.len(), 1, "she never took the opening");
        assert_eq!(turn.said[0].text, "your branch is behind origin");
    }

    #[test]
    fn the_governor_can_silence_her_synchronously() {
        isolate();
        let mut a = Attention::new(4);
        a.observe(1_000, &Observation::Idle { idle: true, for_ms: 300_000 });
        a.submit(1_000, Utterance::new("an idle thought about nothing", Urgency::Whim));
        a.submit(1_000, Utterance::new("something is on fire", Urgency::Alarm));
        a.set_tier(Tier::Dormant, &TierReason::PowerCritical);
        assert!(a.silenced());
        assert_eq!(a.budget.held_count(), 1, "chatter is shed, not hoarded");
        let turn = a.tick(2_000);
        assert_eq!(turn.said.len(), 1, "an alarm still lands at T4");
        let ev = a.drain_events();
        assert!(ev.iter().any(|e| matches!(&e.kind,
            EventKind::Dropped { why, .. } if why.contains("dormant"))));
        // And back up again.
        a.set_tier(Tier::Full, &TierReason::Idle);
        assert!(!a.silenced());
        assert_eq!(Attention::cost_at(Tier::Dormant), Cost::FREE);
        assert!(Attention::cost_at(Tier::Full).ram_mib > 0);
    }

    #[test]
    fn petting_her_is_remembered_and_summoning_her_is_an_opening() {
        isolate();
        let mut a = Attention::new(5);
        a.set_hour(23);
        for _ in 0..10 {
            a.interact(1_000, Interaction::Pet);
        }
        assert_eq!(a.relationship.pets, 10);
        assert_eq!(a.relationship.favourite_hour(), Some(23));
        a.interact(2_000, Interaction::Summon);
        assert_eq!(a.moment(2_000).opportunity, Some((Opportunity::Asked, 2_000)));
        // An opening she can actually use.
        a.submit(2_000, Utterance::new("a small remark about the evening", Urgency::Whim));
        assert_eq!(a.tick(2_000).said.len(), 1);
    }

    #[test]
    fn the_whole_thing_replays_identically() {
        isolate();
        let run = || {
            let mut a = Attention::new(0xA77E);
            a.set_chattiness(Chattiness::Chatty);
            a.set_hour(21);
            let mut log = Vec::new();
            for step in 0..600u64 {
                let t = step * 10_000;
                match step % 7 {
                    0 => a.observe(t, &Observation::Idle { idle: false, for_ms: 0 }),
                    3 => a.observe(t, &Observation::Focus {
                        app_id: format!("app{}", step % 3),
                        title: "t".into(),
                    }),
                    5 => a.observe(t, &Observation::Vitals {
                        cpu_pct: (step % 100) as u8,
                        gpu_pct: 10,
                        vram_used_mib: 512,
                        temp_c: 60,
                        on_battery: false,
                    }),
                    _ => {}
                }
                if step % 11 == 0 {
                    a.submit(t, Utterance::new(format!("thought {step}"), Urgency::Notable));
                }
                let turn = a.tick(t);
                log.push((turn.effects.len(), turn.said.len()));
            }
            log
        };
        let a = run();
        assert_eq!(a, run());
        assert!(a.iter().any(|(e, _)| *e > 0));
        assert!(a.iter().any(|(_, s)| *s > 0));
    }

    #[test]
    fn state_round_trips_whole() {
        isolate();
        let mut a = Attention::new(7);
        deep_work(&mut a, 0);
        a.submit(1_000_000, Utterance::new("something held for later", Urgency::Notable));
        a.drain_events(); // the recorder owns these; they are not her state
        let json = serde_json::to_string(&a).unwrap();
        let back: Attention = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
    }
}
