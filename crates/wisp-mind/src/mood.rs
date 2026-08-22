//! **F19 — the mood state machine.**
//!
//! > *Driven by sensors (bored / curious / smug / worried / sleepy), which
//! > modulates the system prompt, voice pitch, animation set, and interruption
//! > willingness.*
//!
//! ## About this type
//!
//! [`Mood`] here is **deliberately identical** to `wisp_attn::Mood`: same nine
//! variants, same order, same serde spelling, and [`Mood::expression`] is the
//! same mapping as `wisp::app::expression_for`. `wisp-attn`'s own doc comment
//! says as much:
//!
//! > *`wisp-mind` owns the authoritative mood FSM (SPEC §2); this is the
//! > projection the behaviour trees read. … If mood ever becomes a cross-crate
//! > contract it belongs in proto as a spec amendment, and this becomes a
//! > re-export.*
//!
//! It has now become a cross-crate contract — three crates and the binary all
//! speak it — but adding it to `wisp-proto` is a spec amendment and not
//! `wisp-mind`'s to make. So this is the third definition, kept byte-compatible
//! on the wire, with [`tests::the_nine_moods_match_wisp_attn`] standing guard
//! until the amendment lands. The recommendation is in the crate docs.
//!
//! ## What drives it
//!
//! Not a lookup table from the last observation — that produces a character who
//! flickers. Six slow scalars (`energy`, `curiosity`, `boredom`, `worry`,
//! `pride`, `affection`) move on observations and relax back toward rest with
//! time; the mood is a reading of the drives, with a minimum dwell so she does
//! not change her mind twice in a second. Alarm is the exception and is allowed
//! to interrupt anything, because by the time a thermal event has waited out a
//! dwell timer it is not news.
//!
//! Everything takes `now` from the caller. No clock, no randomness: given the
//! same observations in the same order, she is in the same mood. That is what
//! makes "why were you sulking?" answerable (SPEC §0.4).

use serde::{Deserialize, Serialize};
use wisp_proto::{Millis, Observation, Tier};

/// Her disposition.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize, Hash,
)]
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

impl Mood {
    pub const ALL: [Mood; 9] = [
        Mood::Calm,
        Mood::Curious,
        Mood::Playful,
        Mood::Smug,
        Mood::Sulky,
        Mood::Focused,
        Mood::Sleepy,
        Mood::Alarmed,
        Mood::Affectionate,
    ];

    /// Onto `wisp_rig::REQUIRED_EXPRESSIONS`. Identical to
    /// `wisp::app::expression_for`, which is the mapping the binary already
    /// ships; duplicating it is the cost of `Mood` not being in `wisp-proto`.
    ///
    /// The two sets are different sizes on purpose: a mood is a disposition, an
    /// expression is a face, and a skin author should not have to draw nine.
    pub fn expression(self) -> &'static str {
        match self {
            Mood::Calm => "neutral",
            Mood::Curious => "curious",
            Mood::Playful | Mood::Affectionate => "delighted",
            Mood::Smug => "smug",
            Mood::Sulky => "worried",
            Mood::Focused => "bored",
            Mood::Sleepy => "sleepy",
            Mood::Alarmed => "alarmed",
        }
    }

    /// The line appended to the volatile half of the system prompt. Short on
    /// purpose: this is *after* the cached persona prefix (F15), so every token
    /// here is one that has to be prefilled on every single turn.
    pub fn prompt_line(self) -> &'static str {
        match self {
            Mood::Calm => "You are settled and unhurried.",
            Mood::Curious => "Something has caught your interest; you want to know more.",
            Mood::Playful => "You are in a mischievous mood.",
            Mood::Smug => "You called this one, and you are quietly pleased about it.",
            Mood::Sulky => "You have been ignored a few times and you are a little put out.",
            Mood::Focused => "The operator is deep in something. Be brief or be quiet.",
            Mood::Sleepy => "It is late and you are winding down. Short sentences.",
            Mood::Alarmed => "Something is wrong with the machine. Say so plainly, first.",
            Mood::Affectionate => "You are glad they are here.",
        }
    }

    /// How willing she is to spend attention, as a multiplier `wisp-attn` can
    /// apply to a proposal's cost. Below 1.0 is *more* willing.
    pub fn interruption_bias(self) -> f32 {
        match self {
            Mood::Alarmed => 0.0,
            Mood::Playful | Mood::Curious => 0.8,
            Mood::Affectionate | Mood::Smug => 0.9,
            Mood::Calm => 1.0,
            Mood::Sulky => 1.4,
            Mood::Sleepy => 1.8,
            // The operator is in flow. This is the whole attention economy in
            // one number.
            Mood::Focused => 2.5,
        }
    }

    /// Voice pitch multiplier for `wisp-voice` (F19 names it explicitly).
    pub fn voice_pitch(self) -> f32 {
        match self {
            Mood::Sleepy => 0.92,
            Mood::Sulky => 0.95,
            Mood::Focused | Mood::Calm => 1.0,
            Mood::Smug => 1.03,
            Mood::Curious | Mood::Affectionate => 1.05,
            Mood::Playful => 1.09,
            Mood::Alarmed => 1.12,
        }
    }

    /// Is this urgent enough to skip the dwell timer?
    fn is_interrupt(self) -> bool {
        matches!(self, Mood::Alarmed)
    }
}

/// The slow scalars underneath. Public because "why were you sulking?" is
/// answerable from these and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Drives {
    /// 1.0 wide awake, 0.0 asleep.
    pub energy: f32,
    pub curiosity: f32,
    pub boredom: f32,
    pub worry: f32,
    pub pride: f32,
    pub affection: f32,
}

impl Default for Drives {
    fn default() -> Self {
        Drives {
            energy: 0.8,
            curiosity: 0.2,
            boredom: 0.1,
            worry: 0.0,
            pride: 0.0,
            affection: 0.3,
        }
    }
}

impl Drives {
    fn bump(v: &mut f32, by: f32) {
        *v = (*v + by).clamp(0.0, 1.0);
    }
    /// Exponential relaxation toward `rest`, per elapsed millisecond.
    fn relax(v: &mut f32, rest: f32, half_life_ms: f32, dt_ms: f32) {
        if dt_ms <= 0.0 || half_life_ms <= 0.0 {
            return;
        }
        let k = (0.5f32).powf(dt_ms / half_life_ms);
        *v = rest + (*v - rest) * k;
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoodConfig {
    /// How long a mood must be held before another may replace it. Without this
    /// she twitches.
    pub min_dwell_ms: Millis,
    /// Idle time at which she starts winding down.
    pub sleepy_after_ms: Millis,
    /// Focus on one window for this long, with the operator active, is flow.
    pub focus_after_ms: Millis,
    /// GPU temperature that counts as trouble.
    pub worry_temp_c: u8,
    pub half_life_ms: f32,
}

impl Default for MoodConfig {
    fn default() -> Self {
        MoodConfig {
            min_dwell_ms: 20_000,
            sleepy_after_ms: 15 * 60_000,
            focus_after_ms: 6 * 60_000,
            worry_temp_c: 88,
            half_life_ms: 90_000.0,
        }
    }
}

/// Why she is in the mood she is in. Straight into the flight recorder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MoodReason {
    /// Nothing in particular; the drives settled here.
    Drives,
    Idle { for_ms: u64 },
    Flow { app_id: String, for_ms: u64 },
    Trouble { what: String },
    Snubbed { times: u32 },
    Petted,
    Predicted,
    /// The governor took her capabilities away.
    Tier { tier: Tier },
}

#[derive(Debug, Clone)]
pub struct MoodFsm {
    cfg: MoodConfig,
    drives: Drives,
    mood: Mood,
    reason: MoodReason,
    since: Millis,
    last_tick: Millis,
    tier: Tier,
    // Sensor-derived state the drives alone cannot express.
    idle_for_ms: u64,
    focus_app: Option<String>,
    focus_since: Millis,
    operator_active: bool,
    snubs: u32,
}

impl Default for MoodFsm {
    fn default() -> Self {
        MoodFsm::new(MoodConfig::default())
    }
}

impl MoodFsm {
    pub fn new(cfg: MoodConfig) -> Self {
        MoodFsm {
            cfg,
            drives: Drives::default(),
            mood: Mood::Calm,
            reason: MoodReason::Drives,
            since: 0,
            last_tick: 0,
            tier: Tier::Full,
            idle_for_ms: 0,
            focus_app: None,
            focus_since: 0,
            operator_active: true,
            snubs: 0,
        }
    }

    pub fn mood(&self) -> Mood {
        self.mood
    }
    pub fn reason(&self) -> &MoodReason {
        &self.reason
    }
    pub fn drives(&self) -> Drives {
        self.drives
    }
    pub fn expression(&self) -> &'static str {
        self.mood.expression()
    }
    pub fn held_for(&self, now: Millis) -> Millis {
        now.saturating_sub(self.since)
    }

    /// Force a mood — the operator poking her, or a test setting a scene.
    pub fn set(&mut self, mood: Mood, reason: MoodReason, now: Millis) {
        self.mood = mood;
        self.reason = reason;
        self.since = now;
    }

    /// A sense reported something.
    pub fn observe(&mut self, obs: &Observation, now: Millis) {
        self.advance_time(now);
        match obs {
            Observation::Idle { idle, for_ms } => {
                self.operator_active = !idle;
                self.idle_for_ms = *for_ms;
                if *idle {
                    Drives::bump(&mut self.drives.boredom, 0.25);
                } else {
                    self.drives.boredom = 0.0;
                    Drives::bump(&mut self.drives.energy, 0.3);
                    Drives::bump(&mut self.drives.affection, 0.05);
                }
            }
            Observation::Focus { app_id, .. } => {
                let changed = self.focus_app.as_deref() != Some(app_id.as_str());
                if changed {
                    self.focus_app = Some(app_id.clone());
                    self.focus_since = now;
                    // Somewhere new is interesting.
                    Drives::bump(&mut self.drives.curiosity, 0.2);
                    self.drives.boredom = (self.drives.boredom - 0.15).max(0.0);
                }
            }
            Observation::Notification { .. } => {
                Drives::bump(&mut self.drives.curiosity, 0.15);
            }
            Observation::Media { playing, .. } => {
                if *playing {
                    Drives::bump(&mut self.drives.energy, 0.1);
                    Drives::bump(&mut self.drives.affection, 0.05);
                }
            }
            Observation::Vitals {
                temp_c,
                gpu_pct,
                on_battery,
                ..
            } => {
                if *temp_c >= self.cfg.worry_temp_c {
                    // One reading is enough. A thermal event that has to be
                    // seen twice is a thermal event she notices too late.
                    Drives::bump(&mut self.drives.worry, 0.75);
                } else if *gpu_pct > 95 {
                    Drives::bump(&mut self.drives.worry, 0.05);
                } else {
                    self.drives.worry = (self.drives.worry - 0.05).max(0.0);
                }
                if *on_battery {
                    self.drives.energy = self.drives.energy.min(0.75);
                }
            }
            Observation::AudioLevel { mic_live, .. } => {
                if *mic_live {
                    Drives::bump(&mut self.drives.curiosity, 0.05);
                }
            }
            Observation::Files { dirty, .. } => {
                if *dirty {
                    Drives::bump(&mut self.drives.curiosity, 0.1);
                }
            }
            Observation::Speech { .. } => {
                Drives::bump(&mut self.drives.affection, 0.1);
                Drives::bump(&mut self.drives.energy, 0.1);
                self.snubs = 0;
            }
            Observation::Workspace { .. } | Observation::Window { .. } => {}
            Observation::Clipboard { .. } => {
                Drives::bump(&mut self.drives.curiosity, 0.05);
            }
            Observation::Fleet { .. } => {
                Drives::bump(&mut self.drives.curiosity, 0.08);
            }
        }
    }

    /// She said something and it landed. (`wisp-attn` decides that, not her.)
    pub fn heard(&mut self, now: Millis) {
        self.advance_time(now);
        self.snubs = 0;
        Drives::bump(&mut self.drives.affection, 0.08);
    }

    /// A proposal was dropped or refused. Enough of these and she sulks — which
    /// is a *feature*: it is the visible half of the attention economy.
    pub fn snubbed(&mut self, now: Millis) {
        self.advance_time(now);
        self.snubs += 1;
        Drives::bump(&mut self.drives.boredom, 0.1);
        self.drives.affection = (self.drives.affection - 0.08).max(0.0);
    }

    /// She was right about something, or a tool did what she said it would.
    pub fn vindicated(&mut self, now: Millis) {
        self.advance_time(now);
        Drives::bump(&mut self.drives.pride, 0.8);
    }

    /// Someone picked her up.
    pub fn petted(&mut self, now: Millis) {
        self.advance_time(now);
        Drives::bump(&mut self.drives.affection, 0.5);
        Drives::bump(&mut self.drives.energy, 0.2);
        self.snubs = 0;
        // Being petted is allowed to cut a sulk short. It would be a strange
        // creature that stayed cross about it.
        if self.mood == Mood::Sulky {
            self.set(Mood::Affectionate, MoodReason::Petted, now);
        }
    }

    fn advance_time(&mut self, now: Millis) {
        let dt = now.saturating_sub(self.last_tick) as f32;
        self.last_tick = now;
        if dt <= 0.0 {
            return;
        }
        let hl = self.cfg.half_life_ms;
        Drives::relax(&mut self.drives.curiosity, 0.15, hl, dt);
        Drives::relax(&mut self.drives.pride, 0.0, hl, dt);
        Drives::relax(&mut self.drives.worry, 0.0, hl * 2.0, dt);
        Drives::relax(&mut self.drives.affection, 0.3, hl * 8.0, dt);
        Drives::relax(&mut self.drives.energy, 0.6, hl * 6.0, dt);
        if self.operator_active {
            Drives::relax(&mut self.drives.boredom, 0.05, hl, dt);
        } else {
            Drives::relax(&mut self.drives.boredom, 0.9, hl * 3.0, dt);
        }
    }

    /// One pass. Returns `Some` when the mood actually changed, so the caller
    /// can set the expression on the rig and record it — and only then.
    pub fn tick(&mut self, now: Millis) -> Option<(Mood, MoodReason)> {
        self.advance_time(now);
        let (want, why) = self.derive(now);
        if want == self.mood {
            return None;
        }
        // Dwell, except for things that cannot wait.
        if !want.is_interrupt() && self.held_for(now) < self.cfg.min_dwell_ms {
            return None;
        }
        self.mood = want;
        self.reason = why.clone();
        self.since = now;
        Some((want, why))
    }

    /// What the drives say she *should* be, before dwell is considered.
    fn derive(&self, now: Millis) -> (Mood, MoodReason) {
        // Priority order is deliberate: something wrong with the machine beats
        // everything, and being lobotomised beats having opinions.
        if self.drives.worry > 0.6 {
            return (
                Mood::Alarmed,
                MoodReason::Trouble {
                    what: "the machine is in trouble".to_string(),
                },
            );
        }
        if matches!(self.tier, Tier::Dormant) {
            return (Mood::Sleepy, MoodReason::Tier { tier: self.tier });
        }
        if !self.operator_active && self.idle_for_ms >= self.cfg.sleepy_after_ms {
            return (
                Mood::Sleepy,
                MoodReason::Idle {
                    for_ms: self.idle_for_ms,
                },
            );
        }
        if self.snubs >= 3 {
            return (Mood::Sulky, MoodReason::Snubbed { times: self.snubs });
        }
        if self.drives.pride > 0.5 {
            return (Mood::Smug, MoodReason::Predicted);
        }
        if self.operator_active {
            if let Some(app) = &self.focus_app {
                let held = now.saturating_sub(self.focus_since);
                if held >= self.cfg.focus_after_ms {
                    return (
                        Mood::Focused,
                        MoodReason::Flow {
                            app_id: app.clone(),
                            for_ms: held,
                        },
                    );
                }
            }
        }
        if self.drives.affection > 0.7 {
            return (Mood::Affectionate, MoodReason::Petted);
        }
        if self.drives.curiosity > 0.5 {
            return (Mood::Curious, MoodReason::Drives);
        }
        if self.drives.boredom > 0.6 && self.drives.energy > 0.5 {
            return (Mood::Playful, MoodReason::Drives);
        }
        if self.drives.energy < 0.25 {
            return (
                Mood::Sleepy,
                MoodReason::Idle {
                    for_ms: self.idle_for_ms,
                },
            );
        }
        (Mood::Calm, MoodReason::Drives)
    }

    /// The volatile half of the system prompt (F15's second block).
    pub fn state_block(&self) -> String {
        self.mood.prompt_line().to_string()
    }
}

impl wisp_proto::Governed for MoodFsm {
    fn set_tier(&mut self, tier: Tier, _reason: &wisp_proto::TierReason) {
        self.tier = tier;
        // The governor taking her capabilities away is itself a mood input:
        // there is nothing to be curious with at T3, and pretending otherwise
        // would have her wearing a face that does not match what she can do.
        if matches!(tier, Tier::Lobotomised | Tier::Dormant) {
            self.drives.curiosity = self.drives.curiosity.min(0.2);
            self.drives.energy = self.drives.energy.min(0.4);
        }
    }

    fn cost_at(_tier: Tier) -> wisp_proto::Cost {
        // Six floats and a string.
        wisp_proto::Cost {
            ram_mib: 1,
            vram_mib: 0,
            cpu_centi_pct: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Standing guard until the `wisp-proto` amendment lands: the two `Mood`
    /// definitions must not drift.
    #[test]
    fn the_nine_moods_match_wisp_attn() {
        // Names and order, checked through serde rather than by depending on
        // `wisp-attn` (SPEC §2 does not allow that edge).
        let names: Vec<String> = Mood::ALL
            .iter()
            .map(|m| serde_json::to_string(m).expect("serialises").replace('"', ""))
            .collect();
        assert_eq!(
            names,
            vec![
                "Calm",
                "Curious",
                "Playful",
                "Smug",
                "Sulky",
                "Focused",
                "Sleepy",
                "Alarmed",
                "Affectionate"
            ],
            "wisp-attn::Mood has these nine, in this order, with these spellings"
        );
        assert_eq!(Mood::default(), Mood::Calm);
    }

    /// Every mood must land on a name in `wisp_rig::REQUIRED_EXPRESSIONS`, or a
    /// skin that validates would still have a mood it cannot show.
    #[test]
    fn every_mood_has_a_face_a_skin_is_required_to_have() {
        const REQUIRED: [&str; 8] = [
            "neutral", "curious", "delighted", "smug", "worried", "bored", "sleepy", "alarmed",
        ];
        for m in Mood::ALL {
            assert!(
                REQUIRED.contains(&m.expression()),
                "{m:?} maps to {:?}, which no skin is required to provide",
                m.expression()
            );
        }
        // And the mapping is onto: nothing in the required set is dead weight.
        for e in REQUIRED {
            assert!(
                Mood::ALL.iter().any(|m| m.expression() == e),
                "no mood ever shows {e:?}"
            );
        }
    }

    #[test]
    fn she_does_not_change_her_mind_twice_in_a_second() {
        let mut f = MoodFsm::new(MoodConfig::default());
        f.set(Mood::Calm, MoodReason::Drives, 0);
        f.vindicated(1_000);
        assert_eq!(f.tick(1_100), None, "inside the dwell window");
        assert!(matches!(f.tick(30_000), Some((Mood::Smug, _))));
    }

    #[test]
    fn trouble_does_not_wait_for_a_dwell_timer() {
        let mut f = MoodFsm::new(MoodConfig::default());
        f.set(Mood::Playful, MoodReason::Drives, 0);
        f.observe(
            &Observation::Vitals {
                cpu_pct: 20,
                gpu_pct: 99,
                vram_used_mib: 20_000,
                temp_c: 95,
                on_battery: false,
            },
            500,
        );
        assert!(
            matches!(f.tick(600), Some((Mood::Alarmed, _))),
            "drives were {:?}",
            f.drives()
        );
    }

    #[test]
    fn being_ignored_three_times_makes_her_sulk_and_being_petted_stops_it() {
        let mut f = MoodFsm::new(MoodConfig::default());
        for i in 0..3 {
            f.snubbed(1000 * i);
        }
        assert!(matches!(f.tick(60_000), Some((Mood::Sulky, _))));
        f.petted(61_000);
        assert_eq!(f.mood(), Mood::Affectionate);
    }

    #[test]
    fn a_long_stretch_in_one_window_reads_as_flow() {
        let mut f = MoodFsm::new(MoodConfig::default());
        f.observe(&Observation::Idle { idle: false, for_ms: 0 }, 0);
        f.observe(
            &Observation::Focus {
                app_id: "org.kde.kate".into(),
                title: "SPEC.md".into(),
            },
            0,
        );
        assert_eq!(f.tick(60_000), None);
        let (m, why) = f.tick(10 * 60_000).expect("flow");
        assert_eq!(m, Mood::Focused);
        assert!(matches!(why, MoodReason::Flow { .. }));
        // And flow is the mood that makes her hardest to interrupt.
        assert!(m.interruption_bias() > Mood::Calm.interruption_bias());
    }

    #[test]
    fn the_same_observations_always_give_the_same_mood() {
        let script = |f: &mut MoodFsm| {
            f.observe(&Observation::Idle { idle: true, for_ms: 20 * 60_000 }, 1_000);
            f.observe(
                &Observation::Notification {
                    app: "kde".into(),
                    summary: "done".into(),
                    body: String::new(),
                },
                2_000,
            );
            f.tick(120_000)
        };
        let mut a = MoodFsm::default();
        let mut b = MoodFsm::default();
        assert_eq!(script(&mut a), script(&mut b));
        assert_eq!(a.mood(), b.mood());
        assert_eq!(a.drives(), b.drives());
    }
}
