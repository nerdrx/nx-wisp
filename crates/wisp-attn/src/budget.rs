//! F36 — the interruption budget. SPEC §3.4: *nothing reaches the operator
//! except as an `Utterance` submitted here*.
//!
//! This is the one piece of NX Wisp whose bugs are felt as "she is annoying",
//! so it is a plain deterministic state machine: integer token arithmetic, an
//! explicit priority order, and an explanation for every decision it makes.
//!
//! The shape of it:
//!
//! * A **token bucket** refilling continuously over a window (default: six
//!   tokens an hour). `Urgency::cost()` prices a `Whim` at 3 and a `Notable`
//!   at 1; `Answer` and `Alarm` are free because the operator either asked, or
//!   needs to know now.
//! * **Deferral.** An utterance whose moment is wrong is *held*, not dropped.
//!   It waits for an opening — the operator went idle, came back, changed
//!   focus, a build finished, the music stopped.
//! * **Staleness.** A held thought that missed its moment is dropped unsaid and
//!   recorded as [`EventKind::Dropped`], and a tombstone keeps it from being
//!   silently resurrected by a re-submission a minute later (SPEC §3.5's rule,
//!   applied to speech).
//! * **Coalescing.** Three similar thoughts become one thing said once.
//! * **Priority inversion guard.** An `Alarm` is never behind a `Whim`: it
//!   ignores the bucket, flow, the quiet gap, the dial, and the queue cap, and
//!   it evicts held chatter rather than being refused for want of a slot.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use wisp_proto::{Event, EventKind, Millis, Urgency, Utterance};

use crate::flow::{Moment, Opportunity};
use crate::text::{similarity, topic_tokens};
use crate::Chattiness;

/// Handle for a queued thought, so a host can ask what became of it.
pub type UtteranceId = u64;

/// Why something was not said. Every one of these reaches the flight recorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DropReason {
    /// Its `stale_after` passed while it waited (or had already passed).
    Stale,
    /// The chattiness dial is at `Silent` and this cost attention.
    DialSilent,
    /// The queue was full and nothing cheaper was holding a slot.
    QueueFull,
    /// Evicted to make room for something more urgent.
    Superseded,
    /// An identical thought was dropped for staleness recently. It missed its
    /// moment; it does not get to come back as if nothing happened.
    Resurrected,
}

impl DropReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DropReason::Stale => "stale",
            DropReason::DialSilent => "dial silent",
            DropReason::QueueFull => "queue full",
            DropReason::Superseded => "superseded",
            DropReason::Resurrected => "stale (already dropped once)",
        }
    }
}

/// Why a held thought is still held. Answers "why haven't you said it yet".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HoldReason {
    /// Its own `defer_until` has not arrived.
    NotYet,
    /// She has been silenced (governor at `Dormant`, or the operator muted her).
    Silenced,
    /// The operator looks like they are in flow and this does not break flow.
    InFlow,
    /// Not enough attention tokens left in the window.
    NoTokens,
    /// She spoke too recently; the quiet gap has not elapsed.
    TooSoon,
    /// Nothing has happened that makes this a good moment to speak.
    NoOpening,
}

impl HoldReason {
    pub fn as_str(self) -> &'static str {
        match self {
            HoldReason::NotYet => "deferred",
            HoldReason::Silenced => "silenced",
            HoldReason::InFlow => "in flow",
            HoldReason::NoTokens => "out of budget",
            HoldReason::TooSoon => "too soon after the last thing she said",
            HoldReason::NoOpening => "waiting for a good moment",
        }
    }
}

/// What happened to a submission, before any pump runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Queued. It may still be said on the very next pump.
    Queued(UtteranceId),
    /// Folded into a similar thought already waiting.
    Coalesced { into: UtteranceId },
    /// Refused outright; already recorded as dropped.
    Refused(DropReason),
}

impl Admission {
    pub fn id(&self) -> Option<UtteranceId> {
        match self {
            Admission::Queued(id) | Admission::Coalesced { into: id } => Some(*id),
            Admission::Refused(_) => None,
        }
    }
}

/// A thought waiting for its moment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Held {
    pub id: UtteranceId,
    pub utterance: Utterance,
    pub first_seen: Millis,
    pub last_seen: Millis,
    /// How many thoughts are folded into this one. 1 means nothing coalesced.
    pub merged: u32,
    /// The phrasings that were folded in, oldest first, for the recorder.
    pub sources: Vec<String>,
    /// `stale_after`, or the dial default for its urgency. `None` = never.
    pub stale_at: Option<Millis>,
    /// Normalised content words, for coalescing. Persisted so a restored queue
    /// coalesces exactly as the live one did.
    #[serde(default)]
    fingerprint: BTreeSet<String>,
}

impl Held {
    /// True once the operator would be hearing yesterday's news.
    pub fn is_stale(&self, now: Millis) -> bool {
        matches!(self.stale_at, Some(t) if now >= t)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Tombstone {
    fingerprint: BTreeSet<String>,
    until: Millis,
}

/// The dial, the bucket size, and every threshold, in one serialisable place.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetConfig {
    /// The bucket refills fully over this window.
    pub window_ms: Millis,
    /// Tokens in a full bucket. With `Whim = 3`, six is two whims an hour.
    pub capacity: u32,
    /// Start full, so she may say something in her first minute.
    pub start_full: bool,
    /// Minimum quiet between two costed utterances.
    pub min_gap_ms: Millis,
    /// How many costed thoughts may wait at once.
    pub max_queue: usize,
    /// Refuse anything that costs attention (the `Silent` dial position).
    pub refuse_costed: bool,
    /// Two thoughts this similar, this close together, are one thought.
    pub coalesce_threshold: f32,
    pub coalesce_within_ms: Millis,
    /// Default lifetimes when the author did not set `stale_after`.
    pub default_stale_whim_ms: Millis,
    pub default_stale_notable_ms: Millis,
    /// How long a dropped thought stays dead.
    pub tombstone_ms: Millis,
    /// Flow confidence at or above which only `Answer`/`Alarm` get through.
    pub flow_threshold: f32,
    /// Flow confidence below which a `Notable` no longer needs an opening.
    pub calm_threshold: f32,
    /// How long an opening stays open.
    pub opportunity_window_ms: Millis,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        BudgetConfig::for_chattiness(Chattiness::default())
    }
}

impl BudgetConfig {
    /// The F39 dial, expressed as budget. Everything else about her behaviour
    /// at a dial position is a behaviour-tree condition; this is the part that
    /// has to be arithmetic.
    pub fn for_chattiness(dial: Chattiness) -> Self {
        let (capacity, min_gap_ms, refuse_costed) = match dial {
            Chattiness::Silent => (0, 3_600_000, true),
            Chattiness::Occasional => (6, 240_000, false),
            Chattiness::Chatty => (12, 90_000, false),
            Chattiness::Insufferable => (30, 20_000, false),
        };
        BudgetConfig {
            window_ms: 3_600_000,
            capacity,
            start_full: true,
            min_gap_ms,
            max_queue: 32,
            refuse_costed,
            coalesce_threshold: 0.5,
            coalesce_within_ms: 600_000,
            default_stale_whim_ms: 120_000,
            default_stale_notable_ms: 900_000,
            tombstone_ms: 600_000,
            flow_threshold: 0.6,
            calm_threshold: 0.3,
            opportunity_window_ms: 45_000,
        }
    }

    /// Free-urgency thoughts ignore `max_queue` (an `Alarm` is never refused a
    /// slot by chatter) but the queue still has to be bounded.
    fn hard_cap(&self) -> usize {
        self.max_queue.saturating_add(64)
    }
}

/// The token bucket of SPEC §3.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    cfg: BudgetConfig,
    /// Thousandths of a token, so refill is exact integer arithmetic and two
    /// runs of the same trace agree to the last millitoken.
    tokens_milli: u64,
    last_refill: Millis,
    last_spoke: Option<Millis>,
    queue: Vec<Held>,
    tombstones: Vec<Tombstone>,
    next_id: UtteranceId,
    #[serde(skip)]
    events: Vec<Event>,
    /// Lifetime counters, for the "why are you so quiet" answer.
    pub said_count: u64,
    pub dropped_count: u64,
    pub coalesced_count: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Budget::new(BudgetConfig::default(), 0)
    }
}

impl Budget {
    pub fn new(cfg: BudgetConfig, now: Millis) -> Self {
        let tokens_milli = if cfg.start_full { cfg.capacity as u64 * 1000 } else { 0 };
        Budget {
            cfg,
            tokens_milli,
            last_refill: now,
            last_spoke: None,
            queue: Vec::new(),
            tombstones: Vec::new(),
            next_id: 1,
            events: Vec::new(),
            said_count: 0,
            dropped_count: 0,
            coalesced_count: 0,
        }
    }

    pub fn config(&self) -> &BudgetConfig {
        &self.cfg
    }

    /// Swap the dial. Held thoughts survive; the bucket is clamped to the new
    /// capacity so turning the dial down takes effect immediately (a downgrade
    /// is synchronous, SPEC §3.1).
    pub fn set_config(&mut self, cfg: BudgetConfig) {
        self.tokens_milli = self.tokens_milli.min(cfg.capacity as u64 * 1000);
        self.cfg = cfg;
    }

    pub fn set_chattiness(&mut self, dial: Chattiness) {
        self.set_config(BudgetConfig::for_chattiness(dial));
    }

    /// Whole tokens available right now (call after a pump or refill).
    pub fn tokens(&self) -> u32 {
        (self.tokens_milli / 1000) as u32
    }

    pub fn tokens_milli(&self) -> u64 {
        self.tokens_milli
    }

    pub fn held(&self) -> &[Held] {
        &self.queue
    }

    pub fn held_count(&self) -> usize {
        self.queue.len()
    }

    /// Events for the flight recorder. Facts about the past, never commands.
    pub fn drain_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    // ---- submission -----------------------------------------------------

    /// Offer a thought to the budget. Nothing is said here — [`Budget::pump`]
    /// decides that — but obvious refusals happen immediately so a caller is
    /// told "no" while it still has the context to care.
    pub fn submit(&mut self, now: Millis, u: Utterance) -> Admission {
        self.refill(now);
        self.expire(now);
        self.push(now, EventKind::Proposed(u.clone()));

        let cost = u.urgency.cost();

        if cost > 0 && self.cfg.refuse_costed {
            return self.refuse(now, &u.text, DropReason::DialSilent);
        }
        if matches!(u.stale_after, Some(t) if now >= t) {
            return self.refuse(now, &u.text, DropReason::Stale);
        }

        let fingerprint = topic_tokens(&u.text, u.expression.as_deref());

        // A thought that missed its moment does not get a second life.
        if cost > 0 && self.tombstoned(&fingerprint) {
            return self.refuse(now, &u.text, DropReason::Resurrected);
        }

        // Coalescing: only chatter merges. An `Answer` is a reply to a specific
        // question and an `Alarm` is a specific fault; folding either into
        // something else would lose the thing that mattered.
        if cost > 0 {
            if let Some(idx) = self.best_merge(now, &fingerprint) {
                let id = self.merge_into(idx, now, u, fingerprint);
                self.coalesced_count += 1;
                return Admission::Coalesced { into: id };
            }
        }

        if let Some(reason) = self.make_room(now, u.urgency) {
            return self.refuse(now, &u.text, reason);
        }

        let id = self.next_id;
        self.next_id += 1;
        let stale_at = self.stale_at_for(now, &u);
        self.queue.push(Held {
            id,
            utterance: u,
            first_seen: now,
            last_seen: now,
            merged: 1,
            sources: Vec::new(),
            stale_at,
            fingerprint,
        });
        Admission::Queued(id)
    }

    fn stale_at_for(&self, now: Millis, u: &Utterance) -> Option<Millis> {
        if let Some(t) = u.stale_after {
            return Some(t);
        }
        // Only chatter gets a default lifetime. An `Answer` the operator asked
        // for, and an `Alarm`, expire only if their author said so.
        match u.urgency {
            Urgency::Whim => Some(now.saturating_add(self.cfg.default_stale_whim_ms)),
            Urgency::Notable => Some(now.saturating_add(self.cfg.default_stale_notable_ms)),
            Urgency::Answer | Urgency::Alarm => None,
        }
    }

    fn tombstoned(&self, fingerprint: &BTreeSet<String>) -> bool {
        self.tombstones
            .iter()
            .any(|t| similarity(&t.fingerprint, fingerprint) >= self.cfg.coalesce_threshold)
    }

    fn best_merge(&self, now: Millis, fingerprint: &BTreeSet<String>) -> Option<usize> {
        let mut best: Option<(usize, f32)> = None;
        for (i, h) in self.queue.iter().enumerate() {
            if h.utterance.urgency.cost() == 0 {
                continue;
            }
            if now.saturating_sub(h.last_seen) > self.cfg.coalesce_within_ms {
                continue;
            }
            // Never fold a thought into one that has already gone stale.
            if h.is_stale(now) {
                continue;
            }
            let s = similarity(&h.fingerprint, fingerprint);
            if s >= self.cfg.coalesce_threshold && best.is_none_or(|(_, b)| s > b) {
                best = Some((i, s));
            }
        }
        best.map(|(i, _)| i)
    }

    /// Merge rules, and why:
    ///
    /// * **text**: the newest phrasing wins. If she has three thoughts about the
    ///   failing build, the current one is the true one.
    /// * **urgency**: the highest wins, but the cost is *not* re-charged — a
    ///   merge must never cost more than the thoughts would have separately.
    /// * **`defer_until`**: the earliest wins (`None` is earliest of all), so a
    ///   merge never postpones something that was ready.
    /// * **`stale_at`**: the latest wins (`None` is latest of all). At least one
    ///   member of the group is still fresh, and the text she will say is that
    ///   member's. Taking the earliest here would let one perishable thought
    ///   kill a live one and then tombstone the whole topic.
    fn merge_into(
        &mut self,
        idx: usize,
        now: Millis,
        u: Utterance,
        fingerprint: BTreeSet<String>,
    ) -> UtteranceId {
        let stale_at = self.stale_at_for(now, &u);
        let h = &mut self.queue[idx];
        h.sources.push(h.utterance.text.clone());
        h.utterance.text = u.text;
        h.utterance.urgency = h.utterance.urgency.max(u.urgency);
        if u.expression.is_some() {
            h.utterance.expression = u.expression;
        }
        h.utterance.defer_until = match (h.utterance.defer_until, u.defer_until) {
            (Some(a), Some(b)) => Some(a.min(b)),
            _ => None,
        };
        h.utterance.stale_after = match (h.utterance.stale_after, u.stale_after) {
            (Some(a), Some(b)) => Some(a.max(b)),
            _ => None,
        };
        h.stale_at = match (h.stale_at, stale_at) {
            (Some(a), Some(b)) => Some(a.max(b)),
            _ => None,
        };
        h.last_seen = now;
        h.merged += 1;
        // Union of fingerprints: the merged thought is about both topics, so it
        // keeps absorbing either of them.
        h.fingerprint.extend(fingerprint);
        h.id
    }

    /// Make a slot. Returns `Some(reason)` if the submission must be refused.
    fn make_room(&mut self, now: Millis, urgency: Urgency) -> Option<DropReason> {
        let costed = urgency.cost() > 0;
        let cap = if costed { self.cfg.max_queue } else { self.cfg.hard_cap() };
        if self.queue.len() < cap {
            return None;
        }
        // Priority inversion guard: something urgent takes a chatterer's slot.
        let victim = self
            .queue
            .iter()
            .enumerate()
            .filter(|(_, h)| h.utterance.urgency < urgency)
            .min_by_key(|(_, h)| {
                (h.utterance.urgency, h.stale_at.unwrap_or(Millis::MAX), std::cmp::Reverse(h.id))
            })
            .map(|(i, _)| i);
        match victim {
            Some(i) => {
                let h = self.queue.remove(i);
                self.record_drop(now, &h.utterance.text, DropReason::Superseded);
                None
            }
            None => Some(DropReason::QueueFull),
        }
    }

    fn refuse(&mut self, now: Millis, text: &str, reason: DropReason) -> Admission {
        self.record_drop(now, text, reason);
        Admission::Refused(reason)
    }

    fn record_drop(&mut self, now: Millis, text: &str, reason: DropReason) {
        self.dropped_count += 1;
        self.push(
            now,
            EventKind::Dropped { text: text.to_string(), why: reason.as_str().to_string() },
        );
    }

    fn push(&mut self, at: Millis, kind: EventKind) {
        self.events.push(Event { at, kind });
    }

    // ---- the bucket -----------------------------------------------------

    /// Continuous refill, in exact integers. Time that produced a fractional
    /// millitoken is carried, not lost, so a thousand small ticks refill at the
    /// same rate as one big one.
    fn refill(&mut self, now: Millis) {
        // Monotonic time (SPEC: `Millis` never walks backwards). If it does,
        // trust the newer stamp rather than banking free tokens.
        if now <= self.last_refill {
            self.last_refill = now;
            return;
        }
        let cap_milli = self.cfg.capacity as u64 * 1000;
        if cap_milli == 0 {
            self.tokens_milli = 0;
            self.last_refill = now;
            return;
        }
        if self.cfg.window_ms == 0 {
            self.tokens_milli = cap_milli;
            self.last_refill = now;
            return;
        }
        let elapsed = now - self.last_refill;
        let gained = (elapsed as u128 * cap_milli as u128 / self.cfg.window_ms as u128) as u64;
        if gained == 0 {
            return;
        }
        // Advance the clock only by the time those whole millitokens cost.
        let consumed_ms = (gained as u128 * self.cfg.window_ms as u128 / cap_milli as u128) as u64;
        self.last_refill = self.last_refill.saturating_add(consumed_ms.min(elapsed));
        self.tokens_milli = self.tokens_milli.saturating_add(gained).min(cap_milli);
        if self.tokens_milli == cap_milli {
            self.last_refill = now;
        }
    }

    /// Drop everything that missed its moment, and lay a tombstone so it does
    /// not come back a minute later as if nothing happened.
    fn expire(&mut self, now: Millis) {
        self.tombstones.retain(|t| now < t.until);
        let mut i = 0;
        while i < self.queue.len() {
            if self.queue[i].is_stale(now) {
                let h = self.queue.remove(i);
                self.record_drop(now, &h.utterance.text, DropReason::Stale);
                if self.cfg.tombstone_ms > 0 {
                    self.tombstones.push(Tombstone {
                        fingerprint: h.fingerprint,
                        until: now.saturating_add(self.cfg.tombstone_ms),
                    });
                }
            } else {
                i += 1;
            }
        }
    }

    // ---- the decision ---------------------------------------------------

    /// Why is this thought still waiting? `None` means it would be said now.
    pub fn hold_reason(&self, now: Millis, m: &Moment, h: &Held) -> Option<HoldReason> {
        self.gate(now, m, h, self.tokens_milli, self.last_spoke).err()
    }

    fn gate(
        &self,
        now: Millis,
        m: &Moment,
        h: &Held,
        tokens_milli: u64,
        last_spoke: Option<Millis>,
    ) -> Result<(), HoldReason> {
        let u = &h.utterance;
        // An explicit hint from the author is honoured for every urgency: if
        // something set `defer_until` on an `Alarm`, that was deliberate.
        if matches!(u.defer_until, Some(d) if now < d) {
            return Err(HoldReason::NotYet);
        }
        // Priority inversion guard, in one line: an Alarm passes every gate
        // below. Nothing chatter does can hold one back.
        if u.urgency == Urgency::Alarm {
            return Ok(());
        }
        if m.silenced {
            return Err(HoldReason::Silenced);
        }
        // The operator asked. It is free, it ignores flow and the quiet gap.
        if u.urgency == Urgency::Answer {
            return Ok(());
        }
        if m.flow >= self.cfg.flow_threshold && !u.urgency.breaks_flow() {
            return Err(HoldReason::InFlow);
        }
        if tokens_milli < u.urgency.cost() as u64 * 1000 {
            return Err(HoldReason::NoTokens);
        }
        if let Some(last) = last_spoke {
            if now.saturating_sub(last) < self.cfg.min_gap_ms {
                return Err(HoldReason::TooSoon);
            }
        }
        // The moment itself. A whim needs an opening — something in the world
        // just changed in a way that makes speaking natural. A notable will
        // take an opening, or a demonstrably calm operator.
        let opening = m.opening_within(now, self.cfg.opportunity_window_ms).is_some();
        match u.urgency {
            Urgency::Whim if !opening => Err(HoldReason::NoOpening),
            Urgency::Notable if !opening && m.flow > self.cfg.calm_threshold => {
                Err(HoldReason::NoOpening)
            }
            _ => Ok(()),
        }
    }

    /// Run the scheduler. Returns what she should say now, most urgent first.
    ///
    /// Order is `(urgency desc, first seen asc, id asc)` — fully determined, so
    /// the same trace always produces the same speech.
    pub fn pump(&mut self, now: Millis, m: &Moment) -> Vec<Utterance> {
        self.refill(now);
        self.expire(now);

        let mut order: Vec<UtteranceId> = self.queue.iter().map(|h| h.id).collect();
        order.sort_by_key(|id| {
            let h = self.queue.iter().find(|h| h.id == *id).expect("id from queue");
            (std::cmp::Reverse(h.utterance.urgency), h.first_seen, h.id)
        });

        let mut said = Vec::new();
        for id in order {
            let Some(idx) = self.queue.iter().position(|h| h.id == id) else { continue };
            if self.gate(now, m, &self.queue[idx], self.tokens_milli, self.last_spoke).is_err() {
                continue;
            }
            let h = self.queue.remove(idx);
            let cost = h.utterance.urgency.cost() as u64 * 1000;
            self.tokens_milli = self.tokens_milli.saturating_sub(cost);
            // Only costed speech starts the quiet gap. An answer or an alarm is
            // not "her talking", and must not mute the next thing she owes.
            if cost > 0 {
                self.last_spoke = Some(now);
            }
            self.said_count += 1;
            self.push(now, EventKind::Said { text: h.utterance.text.clone() });
            said.push(h.utterance);
        }
        said
    }

    /// Forget a held thought without saying it — the host's "never mind".
    pub fn withdraw(&mut self, now: Millis, id: UtteranceId, why: &str) -> bool {
        let Some(idx) = self.queue.iter().position(|h| h.id == id) else { return false };
        let h = self.queue.remove(idx);
        self.dropped_count += 1;
        self.push(now, EventKind::Dropped { text: h.utterance.text, why: why.to_string() });
        true
    }

    /// Shed every costed thought. Used on a downgrade to `Lobotomised` or
    /// `Dormant`: SPEC §3.1 says a subsystem that cannot honour a downgrade
    /// sheds the work rather than queueing it. Alarms and answers survive.
    pub fn shed_chatter(&mut self, now: Millis, why: &str) -> usize {
        let mut shed = Vec::new();
        let mut i = 0;
        while i < self.queue.len() {
            if self.queue[i].utterance.urgency.cost() > 0 {
                shed.push(self.queue.remove(i));
            } else {
                i += 1;
            }
        }
        for h in &shed {
            self.dropped_count += 1;
            self.push(
                now,
                EventKind::Dropped { text: h.utterance.text.clone(), why: why.to_string() },
            );
        }
        shed.len()
    }
}

/// Convenience for the common case in tests and callers: a bare thought.
pub fn whim(text: &str) -> Utterance {
    Utterance::new(text, Urgency::Whim)
}
pub fn notable(text: &str) -> Utterance {
    Utterance::new(text, Urgency::Notable)
}
pub fn answer(text: &str) -> Utterance {
    Utterance::new(text, Urgency::Answer)
}
pub fn alarm(text: &str) -> Utterance {
    Utterance::new(text, Urgency::Alarm)
}

/// A moment with an opening right now, for tests and for hosts that already
/// know the operator is free.
pub fn open_moment(now: Millis) -> Moment {
    Moment { flow: 0.0, opportunity: Some((Opportunity::WentIdle, now)), silenced: false }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::isolate;

    const HOUR: Millis = 3_600_000;

    fn cfg() -> BudgetConfig {
        BudgetConfig::for_chattiness(Chattiness::Occasional)
    }

    fn budget() -> Budget {
        Budget::new(cfg(), 0)
    }

    fn said_texts(v: &[Utterance]) -> Vec<String> {
        v.iter().map(|u| u.text.clone()).collect()
    }

    /// A unique content word, so bulk test chatter does not accidentally
    /// coalesce with itself (`chatter 1` and `chatter 2` are the *same* topic
    /// to the merge rule — as they should be).
    fn word(n: usize) -> String {
        let a = (b'a' + (n / 26 % 26) as u8) as char;
        let b = (b'a' + (n % 26) as u8) as char;
        format!("{a}{b}{a}x")
    }

    /// Two unique words: shares nothing with any other `distinct(j)`.
    fn distinct(i: usize) -> String {
        format!("{} {}", word(2 * i), word(2 * i + 1))
    }

    #[test]
    fn a_whim_costs_three_and_the_bucket_empties() {
        isolate();
        let mut b = budget();
        assert_eq!(b.tokens(), 6);
        let mut t = 0;
        // Two whims, spaced past the quiet gap, each on an opening.
        for i in 0..2 {
            t += 300_000;
            b.submit(t, whim(&format!("thought number {i} about nothing")));
            let out = b.pump(t, &open_moment(t));
            assert_eq!(out.len(), 1, "round {i} said {out:?}");
        }
        // 6 tokens, +0.5 refilled over the ten minutes, -3 -3 spent: half a
        // token left, which buys nothing.
        assert_eq!(b.tokens(), 0);
        assert_eq!(b.tokens_milli(), 500);
        t += 300_000;
        b.submit(t, whim("a third completely unrelated remark"));
        assert!(b.pump(t, &open_moment(t)).is_empty(), "budget should be exhausted");
        assert_eq!(b.held_count(), 1, "and it should be held, not dropped");
    }

    #[test]
    fn the_bucket_refills_continuously_and_exactly() {
        isolate();
        let mut b = budget();
        // Six tokens, two whims: empty, with four minutes of drip in between.
        b.submit(0, whim("first remark about the weather"));
        assert_eq!(b.pump(0, &open_moment(0)).len(), 1);
        assert_eq!(b.tokens(), 3);
        let t = 250_000;
        b.submit(t, whim("second remark about the garden"));
        assert_eq!(b.pump(t, &open_moment(t)).len(), 1);
        assert_eq!(b.tokens(), 0, "3 + 0.41 refilled - 3 spent");

        // Half an hour later she can afford a whim again.
        let t = t + HOUR / 2;
        assert_eq!({
            b.pump(t, &Moment::busy());
            b.tokens()
        }, 3);

        // A thousand tiny ticks refill exactly as fast as one big one: the
        // sub-millitoken remainder is carried, never lost or double-counted.
        let mut a = Budget::new(cfg(), 0);
        let mut c = Budget::new(cfg(), 0);
        a.submit(0, whim("spend three tokens right now please"));
        c.submit(0, whim("spend three tokens right now please"));
        a.pump(0, &open_moment(0));
        c.pump(0, &open_moment(0));
        for i in 1..=1000u64 {
            a.pump(i * 1_000, &Moment::busy());
        }
        c.pump(1_000_000, &Moment::busy());
        assert_eq!(a.tokens_milli(), c.tokens_milli());
        assert_eq!(a.tokens_milli(), 3000 + 1_000_000 / 600);
    }

    #[test]
    fn the_bucket_never_banks_past_capacity() {
        isolate();
        let mut b = budget();
        b.pump(HOUR * 100, &Moment::busy());
        assert_eq!(b.tokens(), 6);
    }

    #[test]
    fn an_alarm_always_gets_through() {
        isolate();
        let mut b = budget();
        // Worst case we can build: no tokens, deep flow, she just spoke, the
        // queue is full of whims, and there is no opening.
        b.submit(0, whim("burn the whole bucket right now"));
        b.pump(0, &open_moment(0));
        assert_eq!(b.tokens(), 3);
        for i in 0..64 {
            b.submit(1_000, whim(&distinct(i)));
        }
        let m = Moment { flow: 0.99, opportunity: None, silenced: true };
        assert!(b.pump(2_000, &m).is_empty());

        b.submit(2_000, alarm("your gpu is at ninety eight degrees"));
        let out = b.pump(2_000, &m);
        assert_eq!(said_texts(&out), vec!["your gpu is at ninety eight degrees"]);
        assert_eq!(b.tokens(), 3, "an alarm is free");
    }

    #[test]
    fn an_alarm_is_never_stuck_behind_queued_whims() {
        isolate();
        let mut b = budget();
        for i in 0..40 {
            b.submit(0, whim(&distinct(i)));
        }
        b.submit(0, notable("a moderately interesting fact about disks"));
        b.submit(0, alarm("the disk is full"));
        b.submit(0, answer("forty two, as you asked"));
        let out = b.pump(1_000, &open_moment(1_000));
        // Alarm first, then the answer; both free, both ahead of everything.
        assert_eq!(out[0].text, "the disk is full");
        assert_eq!(out[1].text, "forty two, as you asked");
        assert_eq!(out[0].urgency, Urgency::Alarm);
    }

    #[test]
    fn an_alarm_evicts_chatter_rather_than_being_refused_a_slot() {
        isolate();
        let mut small = BudgetConfig::for_chattiness(Chattiness::Chatty);
        small.max_queue = 3;
        let mut b = Budget::new(small, 0);
        let m = Moment { flow: 0.9, opportunity: None, silenced: false };
        for i in 0..3 {
            assert!(matches!(b.submit(0, whim(&distinct(i))), Admission::Queued(_)));
        }
        assert!(matches!(
            b.submit(0, whim(&distinct(3))),
            Admission::Refused(DropReason::QueueFull)
        ));
        // The alarm gets in anyway.
        assert!(matches!(b.submit(0, alarm("smoke detected")), Admission::Queued(_)));
        assert_eq!(said_texts(&b.pump(0, &m)), vec!["smoke detected"]);
        // ...and a Notable evicts a Whim to take a slot.
        let before = b.held_count();
        assert!(matches!(b.submit(0, notable("the build finished")), Admission::Queued(_)));
        assert_eq!(b.held_count(), before, "a whim gave up its slot");
        let evicted = b
            .drain_events()
            .into_iter()
            .any(|e| matches!(e.kind, EventKind::Dropped { ref why, .. } if why == "superseded"));
        assert!(evicted);
    }

    #[test]
    fn whims_are_dropped_under_load_but_notables_survive_longer() {
        isolate();
        let mut b = budget();
        let m = Moment { flow: 0.95, opportunity: None, silenced: false };
        b.submit(0, whim("something idle about the wallpaper"));
        b.submit(0, notable("something worth knowing about backups"));
        assert!(b.pump(0, &m).is_empty(), "flow silences both");
        // Two minutes later the whim is gone; the notable has fifteen.
        b.pump(120_001, &m);
        assert_eq!(b.held_count(), 1);
        assert_eq!(b.held()[0].utterance.urgency, Urgency::Notable);
        b.pump(900_001, &m);
        assert_eq!(b.held_count(), 0);
        assert_eq!(b.dropped_count, 2);
    }

    #[test]
    fn a_wrong_moment_holds_rather_than_drops() {
        isolate();
        let mut b = budget();
        let deep = Moment { flow: 0.9, opportunity: None, silenced: false };
        b.submit(0, notable("your branch is four commits behind"));
        assert!(b.pump(0, &deep).is_empty());
        assert_eq!(b.held_count(), 1, "held, not dropped");
        assert_eq!(b.hold_reason(0, &deep, &b.held()[0]), Some(HoldReason::InFlow));
        // The operator comes back from being away: an opening.
        let free = Moment { flow: 0.1, opportunity: Some((Opportunity::CameBack, 60_000)), silenced: false };
        assert_eq!(said_texts(&b.pump(60_000, &free)), vec!["your branch is four commits behind"]);
    }

    #[test]
    fn a_whim_waits_for_an_opening_even_when_the_operator_is_calm() {
        isolate();
        // Chatty, so the quiet gap (90s) is shorter than a whim's lifetime.
        let mut b = Budget::new(BudgetConfig::for_chattiness(Chattiness::Chatty), 0);
        let calm_no_opening = Moment { flow: 0.05, opportunity: None, silenced: false };
        b.submit(0, whim("that variable name is a war crime"));
        assert!(b.pump(0, &calm_no_opening).is_empty());
        assert_eq!(b.hold_reason(0, &calm_no_opening, &b.held()[0]), Some(HoldReason::NoOpening));
        // A notable, by contrast, will speak to a demonstrably calm operator.
        b.submit(0, notable("the deploy finished a while ago"));
        let out = b.pump(0, &calm_no_opening);
        assert_eq!(said_texts(&out), vec!["the deploy finished a while ago"]);
        // And the whim goes when an opening appears (after the quiet gap).
        let t = 90_000;
        let opened = Moment {
            flow: 0.05,
            opportunity: Some((Opportunity::FocusChanged, t)),
            silenced: false,
        };
        assert_eq!(said_texts(&b.pump(t, &opened)), vec!["that variable name is a war crime"]);
    }

    #[test]
    fn an_opening_goes_stale_after_its_window() {
        isolate();
        let mut b = budget();
        b.submit(0, whim("the fan just spun up like a jet"));
        let old = Moment { flow: 0.0, opportunity: Some((Opportunity::WentIdle, 0)), silenced: false };
        assert!(b.pump(46_000, &old).is_empty(), "a 45s-old opening has closed");
        assert_eq!(b.hold_reason(46_000, &old, &b.held()[0]), Some(HoldReason::NoOpening));
    }

    #[test]
    fn the_quiet_gap_spaces_her_out_but_never_gates_answers() {
        isolate();
        let mut b = Budget::new(BudgetConfig::for_chattiness(Chattiness::Chatty), 0);
        b.submit(0, notable("the tests are green now"));
        b.submit(0, notable("your laptop battery is low"));
        let out = b.pump(0, &open_moment(0));
        assert_eq!(out.len(), 1, "she says one thing at a time");
        assert_eq!(b.hold_reason(0, &open_moment(0), &b.held()[0]), Some(HoldReason::TooSoon));
        // An answer is not gated by the gap she created.
        b.submit(0, answer("it is seventeen"));
        assert_eq!(said_texts(&b.pump(0, &open_moment(0))), vec!["it is seventeen"]);
        // And an answer does not start a new quiet gap for the held notable.
        let t = 90_000;
        assert_eq!(b.pump(t, &open_moment(t)).len(), 1);
    }

    #[test]
    fn stale_items_are_dropped_recorded_and_never_resurface() {
        isolate();
        let mut b = budget();
        let mut u = whim("the compile is taking forever today");
        u.stale_after = Some(10_000);
        b.submit(0, u);
        b.drain_events();

        let out = b.pump(10_000, &open_moment(10_000));
        assert!(out.is_empty());
        assert_eq!(b.held_count(), 0);
        let ev = b.drain_events();
        assert!(ev.iter().any(|e| matches!(&e.kind,
            EventKind::Dropped { text, why } if text.contains("compile") && why == "stale")));

        // The same thought, offered again a minute later, does not sneak back.
        let again = b.submit(70_000, whim("the compile is taking forever today"));
        assert_eq!(again, Admission::Refused(DropReason::Resurrected));
        assert!(b.pump(70_000, &open_moment(70_000)).is_empty());
        // A near-identical rephrasing is caught too.
        assert!(matches!(
            b.submit(80_000, whim("compile is taking forever")),
            Admission::Refused(DropReason::Resurrected)
        ));
        // An unrelated thought is not.
        assert!(matches!(b.submit(80_000, whim("your headphones are unplugged")), Admission::Queued(_)));
        // The tombstone expires with the window.
        assert!(matches!(
            b.submit(700_000, whim("the compile is taking forever today")),
            Admission::Queued(_)
        ));
    }

    #[test]
    fn an_alarm_is_never_tombstoned() {
        isolate();
        let mut b = budget();
        let mut u = notable("the disk is nearly full");
        u.stale_after = Some(1);
        b.submit(0, u);
        b.pump(1, &Moment::busy());
        assert!(matches!(b.submit(2, alarm("the disk is nearly full")), Admission::Queued(_)));
        assert_eq!(b.pump(2, &Moment::busy()).len(), 1);
    }

    #[test]
    fn an_already_stale_submission_is_refused_immediately() {
        isolate();
        let mut b = budget();
        let mut u = notable("this was true five minutes ago");
        u.stale_after = Some(1_000);
        assert_eq!(b.submit(5_000, u), Admission::Refused(DropReason::Stale));
        assert_eq!(b.held_count(), 0);
    }

    #[test]
    fn three_similar_thoughts_become_one() {
        isolate();
        let mut b = budget();
        let a = b.submit(0, whim("the build failed"));
        let c = b.submit(1_000, whim("the build failed again"));
        let d = b.submit(2_000, whim("your build failed, twice now"));
        let id = a.id().unwrap();
        assert_eq!(c, Admission::Coalesced { into: id });
        assert_eq!(d, Admission::Coalesced { into: id });
        assert_eq!(b.held_count(), 1);
        assert_eq!(b.held()[0].merged, 3);
        assert_eq!(b.coalesced_count, 2);

        let out = b.pump(3_000, &open_moment(3_000));
        assert_eq!(said_texts(&out), vec!["your build failed, twice now"], "freshest phrasing");
        assert_eq!(b.tokens(), 3, "charged once, not three times");
    }

    #[test]
    fn coalescing_takes_the_highest_urgency_and_the_latest_deadline() {
        isolate();
        let mut b = budget();
        let mut first = whim("the tests are failing");
        first.stale_after = Some(5_000);
        b.submit(0, first);
        let mut second = notable("the tests are failing badly");
        second.stale_after = Some(500_000);
        b.submit(1_000, second);
        assert_eq!(b.held_count(), 1);
        assert_eq!(b.held()[0].utterance.urgency, Urgency::Notable);
        // The perishable member must not kill the fresh one.
        b.pump(6_000, &Moment::busy());
        assert_eq!(b.held_count(), 1);
    }

    #[test]
    fn dissimilar_thoughts_do_not_coalesce() {
        isolate();
        let mut b = budget();
        b.submit(0, whim("the build failed"));
        let second = b.submit(0, whim("your headphones just disconnected"));
        assert!(matches!(second, Admission::Queued(_)));
        assert_eq!(b.held_count(), 2);
    }

    #[test]
    fn answers_and_alarms_never_coalesce() {
        isolate();
        let mut b = budget();
        b.submit(0, alarm("the disk is full"));
        b.submit(0, alarm("the disk is full"));
        b.submit(0, answer("the answer is fourteen"));
        b.submit(0, answer("the answer is fourteen"));
        assert_eq!(b.held_count(), 4, "each one is its own fact");
    }

    #[test]
    fn the_silent_dial_refuses_chatter_but_not_alarms() {
        isolate();
        let mut b = Budget::new(BudgetConfig::for_chattiness(Chattiness::Silent), 0);
        assert_eq!(b.submit(0, whim("hello there friend")), Admission::Refused(DropReason::DialSilent));
        assert_eq!(
            b.submit(0, notable("your branch diverged")),
            Admission::Refused(DropReason::DialSilent)
        );
        assert!(matches!(b.submit(0, alarm("the disk is full")), Admission::Queued(_)));
        assert!(matches!(b.submit(0, answer("it is nine")), Admission::Queued(_)));
        assert_eq!(b.pump(0, &Moment::busy()).len(), 2);
    }

    #[test]
    fn silence_holds_answers_but_alarms_still_land() {
        isolate();
        let mut b = budget();
        let silenced = Moment { flow: 0.0, opportunity: Some((Opportunity::WentIdle, 0)), silenced: true };
        b.submit(0, answer("the answer you asked for"));
        b.submit(0, alarm("something is actually on fire"));
        assert_eq!(said_texts(&b.pump(0, &silenced)), vec!["something is actually on fire"]);
        assert_eq!(b.held_count(), 1);
        // Unsilenced, the answer is still there — it was held, not dropped.
        assert_eq!(said_texts(&b.pump(1_000, &open_moment(1_000))), vec!["the answer you asked for"]);
    }

    #[test]
    fn defer_until_is_honoured_for_every_urgency() {
        isolate();
        let mut b = budget();
        let mut u = alarm("the backup window opens at midnight");
        u.defer_until = Some(10_000);
        b.submit(0, u);
        assert!(b.pump(9_999, &open_moment(9_999)).is_empty());
        assert_eq!(b.pump(10_000, &open_moment(10_000)).len(), 1);
    }

    #[test]
    fn turning_the_dial_down_takes_effect_immediately() {
        isolate();
        let mut b = Budget::new(BudgetConfig::for_chattiness(Chattiness::Insufferable), 0);
        assert_eq!(b.tokens(), 30);
        b.set_chattiness(Chattiness::Occasional);
        assert_eq!(b.tokens(), 6, "the bucket is clamped, not carried over");
    }

    #[test]
    fn shedding_chatter_keeps_what_matters() {
        isolate();
        let mut b = budget();
        b.submit(0, whim("idle thought about the desktop wallpaper"));
        b.submit(0, notable("a note about your unpushed commits"));
        b.submit(0, alarm("the machine is overheating"));
        b.submit(0, answer("yes, it is tuesday"));
        assert_eq!(b.shed_chatter(0, "governor: dormant"), 2);
        assert_eq!(b.held_count(), 2);
        assert!(b.held().iter().all(|h| h.utterance.urgency.cost() == 0));
    }

    #[test]
    fn withdrawing_a_thought_records_it() {
        isolate();
        let mut b = budget();
        let id = b.submit(0, whim("never mind this one")).id().unwrap();
        b.drain_events();
        assert!(b.withdraw(0, id, "the operator answered it themselves"));
        assert!(!b.withdraw(0, id, "again"));
        let ev = b.drain_events();
        assert!(ev.iter().any(|e| matches!(&e.kind, EventKind::Dropped { why, .. } if why.contains("answered"))));
    }

    #[test]
    fn every_decision_reaches_the_recorder() {
        isolate();
        let mut b = budget();
        b.submit(0, whim("a thought that will be said out loud"));
        b.pump(0, &open_moment(0));
        let ev = b.drain_events();
        assert!(matches!(ev[0].kind, EventKind::Proposed(_)));
        assert!(matches!(ev[1].kind, EventKind::Said { .. }));
        assert!(b.drain_events().is_empty());
    }

    #[test]
    fn the_same_trace_twice_gives_the_same_speech() {
        isolate();
        let run = || {
            let mut b = budget();
            let mut out = Vec::new();
            for step in 0..50u64 {
                let t = step * 30_000;
                b.submit(t, whim(&distinct(step as usize % 7)));
                if step % 3 == 0 {
                    b.submit(t, notable(&distinct(20 + step as usize % 5)));
                }
                if step == 17 {
                    b.submit(t, alarm("the fan stopped"));
                }
                let m = if step % 4 == 0 { open_moment(t) } else { Moment::busy() };
                out.extend(said_texts(&b.pump(t, &m)));
            }
            out
        };
        assert_eq!(run(), run());
        assert!(!run().is_empty());
    }

    #[test]
    fn under_sustained_load_she_stays_within_budget() {
        isolate();
        let mut b = budget();
        let mut said = 0;
        for step in 0..(12 * 60u64) {
            let t = step * 5_000; // one candidate every five seconds for an hour
            b.submit(t, whim(&distinct(step as usize % 300)));
            said += b.pump(t, &open_moment(t)).len();
        }
        // Six tokens up front and six more over the hour, at three a whim and
        // four minutes of quiet between: she speaks at 0s, 240s, 1800s and
        // would speak again at exactly 3600s, one tick past the end.
        assert_eq!(said, 3, "she spoke {said} times in an hour");
        assert!(b.dropped_count > 100, "and the rest were dropped, not hoarded");
        assert!(b.held_count() <= b.config().max_queue);
    }

    #[test]
    fn config_and_state_round_trip_through_serde() {
        isolate();
        let mut b = budget();
        b.submit(0, whim("something to keep in the queue"));
        let json = serde_json::to_string(&b).unwrap();
        let back: Budget = serde_json::from_str(&json).unwrap();
        assert_eq!(back.held_count(), 1);
        assert_eq!(back.tokens_milli(), b.tokens_milli());
    }
}
