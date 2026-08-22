//! F42 — the long run. What she remembers about the two of you.
//!
//! Two kinds of number live here and they behave differently on purpose:
//!
//! * **Counters never decay.** Days owned, times petted, times thrown, times
//!   fed, times summoned. "You have had me for sixty days" is a fact, and a fact
//!   that quietly shrank would make her a liar (SPEC §0.4).
//! * **Affection and the hour histogram decay.** These are about *lately*. A
//!   fortnight of neglect should show, and a favourite hour from a job you left
//!   two years ago is not your favourite hour.
//!
//! Pure accumulator: no clock, no files. Time enters as an explicit elapsed
//! span, and persistence belongs to `wisp` (the binary), so this whole struct
//! is `serde`-serialisable and forgiving of older saved shapes.

use serde::{Deserialize, Serialize};

/// Something the operator did to her.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Interaction {
    /// Petted, stroked, scratched behind whatever she has instead of ears.
    Pet,
    /// Fed a treat.
    Feed,
    /// Picked up and thrown across the desktop.
    Throw,
    /// Summoned with the hotkey, or spoken to.
    Summon,
    /// Told to be quiet.
    Dismiss,
    /// Just played with — dragged around, poked back.
    Play,
}

impl Interaction {
    /// How this moves affection. Throwing her is *not* purely negative: she
    /// is a desktop creature and being flung about is a game — but doing it
    /// over and over, and never anything else, is not.
    pub fn affection_delta(self) -> f32 {
        match self {
            Interaction::Pet => 1.0,
            Interaction::Feed => 1.5,
            Interaction::Play => 0.8,
            Interaction::Summon => 0.4,
            Interaction::Throw => 0.15,
            Interaction::Dismiss => -0.6,
        }
    }
}

/// How well she knows you. Derived, never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Bond {
    New,
    Acquainted,
    Attached,
    Devoted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelationshipConfig {
    /// Affection halves over this many days of no interaction.
    pub affection_half_life_days: f32,
    /// The hour histogram halves over this many days.
    pub hour_half_life_days: f32,
    /// Affection is clamped here, so a weekend of petting is not permanent.
    pub affection_cap: f32,
    /// Thresholds for [`Bond`]: (days owned, affection).
    pub acquainted_at: (u32, f32),
    pub attached_at: (u32, f32),
    pub devoted_at: (u32, f32),
    /// Below this share of the busiest hour, there is no favourite hour.
    pub favourite_hour_margin: f32,
}

impl Default for RelationshipConfig {
    fn default() -> Self {
        RelationshipConfig {
            affection_half_life_days: 14.0,
            hour_half_life_days: 30.0,
            affection_cap: 100.0,
            acquainted_at: (3, 5.0),
            attached_at: (14, 30.0),
            devoted_at: (60, 70.0),
            favourite_hour_margin: 1.2,
        }
    }
}

/// Everything she keeps about you between runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Relationship {
    pub cfg: RelationshipConfig,
    // --- counters: these never decay
    pub days_owned: u32,
    pub pets: u64,
    pub feeds: u64,
    pub throws: u64,
    pub summons: u64,
    pub dismissals: u64,
    pub plays: u64,
    /// Sessions she has been started for.
    pub wakes: u64,
    /// Consecutive days with at least one interaction.
    pub streak_days: u32,
    pub best_streak_days: u32,
    // --- decaying state
    pub affection: f32,
    /// Decayed interaction counts per local hour.
    pub hours: [f32; 24],
    /// Whether anything happened since the last [`Relationship::new_day`].
    pub touched_today: bool,
}

impl Default for Relationship {
    fn default() -> Self {
        Relationship {
            cfg: RelationshipConfig::default(),
            days_owned: 0,
            pets: 0,
            feeds: 0,
            throws: 0,
            summons: 0,
            dismissals: 0,
            plays: 0,
            wakes: 0,
            streak_days: 0,
            best_streak_days: 0,
            affection: 0.0,
            hours: [0.0; 24],
            touched_today: false,
        }
    }
}

impl Relationship {
    pub fn new(cfg: RelationshipConfig) -> Self {
        Relationship { cfg, ..Default::default() }
    }

    /// Record something the operator did, at a host-supplied local hour.
    pub fn record(&mut self, hour: u8, what: Interaction) {
        let h = hour.min(23) as usize;
        match what {
            Interaction::Pet => self.pets += 1,
            Interaction::Feed => self.feeds += 1,
            Interaction::Throw => self.throws += 1,
            Interaction::Summon => self.summons += 1,
            Interaction::Dismiss => self.dismissals += 1,
            Interaction::Play => self.plays += 1,
        }
        // Being dismissed is still attention, but it does not make her fonder,
        // and it does not count towards "your favourite hour with her".
        if what != Interaction::Dismiss {
            self.hours[h] += 1.0;
            self.touched_today = true;
        }
        self.affection =
            (self.affection + what.affection_delta()).clamp(0.0, self.cfg.affection_cap);
    }

    pub fn woke(&mut self) {
        self.wakes += 1;
    }

    /// A day has passed. The host decides when a day rolls over — this crate
    /// has no calendar.
    pub fn new_day(&mut self) {
        self.days_owned += 1;
        if self.touched_today {
            self.streak_days += 1;
            self.best_streak_days = self.best_streak_days.max(self.streak_days);
        } else {
            self.streak_days = 0;
        }
        self.touched_today = false;
        self.decay_days(1.0);
    }

    /// Apply decay for a span of elapsed time. Call this with the real gap
    /// after loading a save — she should feel a fortnight away.
    pub fn decay_days(&mut self, days: f32) {
        if days <= 0.0 {
            return;
        }
        let a = half_life_factor(days, self.cfg.affection_half_life_days);
        self.affection *= a;
        let h = half_life_factor(days, self.cfg.hour_half_life_days);
        for slot in self.hours.iter_mut() {
            *slot *= h;
        }
    }

    pub fn decay_ms(&mut self, elapsed_ms: u64) {
        self.decay_days(elapsed_ms as f32 / 86_400_000.0);
    }

    /// The hour of day she sees you most — but only if it is a real preference
    /// rather than a coin-flip between two equally busy hours.
    pub fn favourite_hour(&self) -> Option<u8> {
        let mut best = 0usize;
        let mut best_v = 0.0f32;
        let mut second = 0.0f32;
        for (i, v) in self.hours.iter().enumerate() {
            if *v > best_v {
                second = best_v;
                best_v = *v;
                best = i;
            } else if *v > second {
                second = *v;
            }
        }
        if best_v <= 0.0 {
            return None;
        }
        if second > 0.0 && best_v < second * self.cfg.favourite_hour_margin {
            return None;
        }
        Some(best as u8)
    }

    /// Total interactions that count towards fondness.
    pub fn interactions(&self) -> u64 {
        self.pets + self.feeds + self.throws + self.summons + self.plays
    }

    /// Has she been thrown far more than she has been petted?
    pub fn mistreated(&self) -> bool {
        self.throws > 20 && self.throws > (self.pets + self.feeds).saturating_mul(3)
    }

    pub fn bond(&self) -> Bond {
        let (d, a) = (self.days_owned, self.affection);
        let c = &self.cfg;
        if d >= c.devoted_at.0 && a >= c.devoted_at.1 {
            Bond::Devoted
        } else if d >= c.attached_at.0 && a >= c.attached_at.1 {
            Bond::Attached
        } else if d >= c.acquainted_at.0 && a >= c.acquainted_at.1 {
            Bond::Acquainted
        } else {
            Bond::New
        }
    }
}

fn half_life_factor(elapsed: f32, half_life: f32) -> f32 {
    if half_life <= 0.0 {
        return 0.0;
    }
    0.5f32.powf(elapsed / half_life)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::isolate;

    #[test]
    fn counters_never_decay() {
        isolate();
        let mut r = Relationship::default();
        for _ in 0..10 {
            r.record(21, Interaction::Pet);
            r.record(21, Interaction::Throw);
        }
        r.decay_days(365.0);
        assert_eq!(r.pets, 10);
        assert_eq!(r.throws, 10);
        assert_eq!(r.interactions(), 20);
        assert!(r.affection < 0.01, "but fondness fades: {}", r.affection);
    }

    #[test]
    fn days_owned_counts_and_the_streak_breaks_on_a_missed_day() {
        isolate();
        let mut r = Relationship::default();
        for _ in 0..5 {
            r.record(10, Interaction::Pet);
            r.new_day();
        }
        assert_eq!(r.days_owned, 5);
        assert_eq!(r.streak_days, 5);
        r.new_day(); // a day where she was ignored
        assert_eq!(r.days_owned, 6);
        assert_eq!(r.streak_days, 0);
        assert_eq!(r.best_streak_days, 5);
    }

    #[test]
    fn affection_grows_and_decays_by_half_life() {
        isolate();
        let mut r = Relationship::default();
        for _ in 0..40 {
            r.record(9, Interaction::Pet);
        }
        let peak = r.affection;
        assert_eq!(peak, 40.0);
        r.decay_days(14.0);
        assert!((r.affection - peak / 2.0).abs() < 0.01, "{}", r.affection);
        r.decay_days(14.0);
        assert!((r.affection - peak / 4.0).abs() < 0.01, "{}", r.affection);
        // Decay is the same whether it arrives in one lump or in pieces.
        let mut a = Relationship::default();
        let mut b = Relationship::default();
        for _ in 0..10 {
            a.record(9, Interaction::Feed);
            b.record(9, Interaction::Feed);
        }
        a.decay_days(7.0);
        for _ in 0..7 {
            b.decay_days(1.0);
        }
        assert!((a.affection - b.affection).abs() < 0.001);
    }

    #[test]
    fn affection_is_capped_and_never_negative() {
        isolate();
        let mut r = Relationship::default();
        for _ in 0..1000 {
            r.record(9, Interaction::Feed);
        }
        assert_eq!(r.affection, r.cfg.affection_cap);
        for _ in 0..1000 {
            r.record(9, Interaction::Dismiss);
        }
        assert_eq!(r.affection, 0.0);
        assert_eq!(r.dismissals, 1000);
    }

    #[test]
    fn favourite_hour_needs_a_real_preference() {
        isolate();
        let mut r = Relationship::default();
        assert_eq!(r.favourite_hour(), None, "she does not guess from nothing");
        for _ in 0..10 {
            r.record(23, Interaction::Pet);
        }
        for _ in 0..9 {
            r.record(10, Interaction::Pet);
        }
        assert_eq!(r.favourite_hour(), None, "10 vs 9 is a coin flip, not a favourite");
        for _ in 0..5 {
            r.record(23, Interaction::Pet);
        }
        assert_eq!(r.favourite_hour(), Some(23));
    }

    #[test]
    fn the_favourite_hour_moves_with_your_habits() {
        isolate();
        let mut r = Relationship::default();
        for _ in 0..30 {
            r.record(9, Interaction::Summon);
        }
        assert_eq!(r.favourite_hour(), Some(9));
        // A year later, having become a night owl.
        r.decay_days(180.0);
        for _ in 0..30 {
            r.record(1, Interaction::Summon);
        }
        assert_eq!(r.favourite_hour(), Some(1));
    }

    #[test]
    fn being_dismissed_does_not_count_as_time_together() {
        isolate();
        let mut r = Relationship::default();
        for _ in 0..20 {
            r.record(14, Interaction::Dismiss);
        }
        assert_eq!(r.favourite_hour(), None);
        assert!(!r.touched_today);
    }

    #[test]
    fn the_bond_needs_both_time_and_fondness() {
        isolate();
        let mut r = Relationship::default();
        assert_eq!(r.bond(), Bond::New);
        for _ in 0..80 {
            r.record(20, Interaction::Pet);
        }
        assert_eq!(r.bond(), Bond::New, "one enthusiastic afternoon is not a bond");
        for _ in 0..70 {
            r.new_day();
            for _ in 0..4 {
                r.record(20, Interaction::Pet);
            }
            r.record(20, Interaction::Feed);
            r.record(20, Interaction::Play);
        }
        assert_eq!(r.bond(), Bond::Devoted);
        // Two months of nothing and she is not devoted any more, but she still
        // knows exactly how long she has been here.
        let days = r.days_owned;
        r.decay_days(60.0);
        assert!(r.bond() < Bond::Devoted);
        assert_eq!(r.days_owned, days);
    }

    #[test]
    fn mistreatment_is_being_thrown_and_nothing_else() {
        isolate();
        let mut r = Relationship::default();
        for _ in 0..30 {
            r.record(15, Interaction::Throw);
        }
        assert!(r.mistreated());
        for _ in 0..20 {
            r.record(15, Interaction::Pet);
        }
        assert!(!r.mistreated(), "being played with rough is fine if she is also loved");
    }

    #[test]
    fn state_round_trips_and_tolerates_an_older_save() {
        isolate();
        let mut r = Relationship::default();
        r.record(9, Interaction::Pet);
        r.new_day();
        let json = serde_json::to_string(&r).unwrap();
        let back: Relationship = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
        // A save from before half these fields existed still loads.
        let old: Relationship = serde_json::from_str(r#"{"pets":7,"days_owned":42}"#).unwrap();
        assert_eq!(old.pets, 7);
        assert_eq!(old.days_owned, 42);
        assert_eq!(old.affection, 0.0);
    }
}
