//! **SPEC §3.5 — deferred cognition.**
//!
//! > At T3/T4 `wisp-mind` accepts work into a bounded queue instead of running
//! > it. On upgrade the queue is replayed **oldest-first with staleness
//! > filtering** — an item whose `stale_after` has passed is dropped, recorded
//! > as dropped, and never silently resurrected.
//!
//! This is the one exception SPEC §3.1 grants: every other subsystem must *shed*
//! work it cannot do rather than queue it. Cognition gets a queue because the
//! work is a thought, and a thought that arrives four minutes late is often
//! still worth having — but only sometimes, which is what `stale_after` is for.
//!
//! Three rules, and all three are tested:
//!
//! 1. **Bounded.** A queue that grows while she is lobotomised would be a memory
//!    leak with a personality. When it is full something goes, and what goes is
//!    the least urgent oldest thing.
//! 2. **Oldest first.** Replay is FIFO within the queue, not by urgency: the
//!    order things happened in is part of what they mean.
//! 3. **A drop is recorded.** Whether it went stale or was pushed out, it
//!    becomes an event. SPEC §0.4 — "why didn't you say anything about that?"
//!    has an answer in the trace.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use wisp_proto::{EventKind, Millis, Tier, Urgency};

use crate::events::EventSink;

/// What kind of thought was postponed. Closed, because "what did she put off?"
/// should be answerable in categories rather than in free text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// The operator asked something and she has not answered yet.
    Reply,
    /// She noticed something and wanted to remark on it.
    Remark,
    /// A tool she wanted to run.
    ToolCall,
    /// F18's nightly consolidation, which only runs at T0 anyway.
    Consolidate,
    /// Something to write into memory.
    Remember,
}

impl JobKind {
    /// How long this kind of thought is worth keeping, by default. A remark
    /// about the machine's state is stale in a minute; an unanswered question
    /// is not stale until the operator has given up on it.
    pub fn default_stale_after_ms(self) -> Option<Millis> {
        match self {
            JobKind::Remark => Some(90_000),
            JobKind::Reply => Some(10 * 60_000),
            JobKind::ToolCall => Some(5 * 60_000),
            // These are the two that are worth doing whenever she gets to them.
            JobKind::Consolidate | JobKind::Remember => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: u64,
    pub kind: JobKind,
    /// One line, for [`EventKind::Deferred`] and the flight recorder.
    pub what: String,
    pub payload: Value,
    pub queued_at: Millis,
    /// Monotonic ms after which this is no longer worth doing.
    pub stale_after: Option<Millis>,
    pub urgency: Urgency,
}

impl Job {
    pub fn new(kind: JobKind, what: impl Into<String>, now: Millis) -> Self {
        Job {
            id: 0,
            kind,
            what: what.into(),
            payload: Value::Null,
            queued_at: now,
            stale_after: kind.default_stale_after_ms().map(|d| now + d),
            urgency: Urgency::Notable,
        }
    }
    pub fn payload(mut self, v: Value) -> Self {
        self.payload = v;
        self
    }
    pub fn urgency(mut self, u: Urgency) -> Self {
        self.urgency = u;
        self
    }
    /// Override the default lifetime.
    pub fn stale_after(mut self, at: Option<Millis>) -> Self {
        self.stale_after = at;
        self
    }
    pub fn is_stale(&self, now: Millis) -> bool {
        self.stale_after.is_some_and(|t| now >= t)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DropReason {
    /// Its `stale_after` had passed by the time she could get to it.
    Stale,
    /// The queue was full and something had to go.
    Full,
    /// The tier went to Dormant, which is not "later", it is "no".
    Silenced,
}

impl DropReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DropReason::Stale => "it had gone stale",
            DropReason::Full => "the deferred queue was full",
            DropReason::Silenced => "she was silenced before she got to it",
        }
    }
}

/// What [`DeferQueue::push`] did.
#[derive(Debug, Clone, PartialEq)]
pub enum Pushed {
    Queued { id: u64, depth: usize },
    /// It went in, but something else had to come out to make room.
    Displaced { id: u64, depth: usize, dropped: Job },
    /// It did not go in: everything already queued was more urgent.
    Refused { job: Job },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeferStats {
    pub queued: u64,
    pub replayed: u64,
    pub dropped_stale: u64,
    pub dropped_full: u64,
    pub high_water: usize,
}

/// What one replay produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Replayed {
    /// In the order they were queued.
    pub ready: Vec<Job>,
    pub dropped: Vec<(Job, DropReason)>,
}

impl Replayed {
    pub fn is_empty(&self) -> bool {
        self.ready.is_empty() && self.dropped.is_empty()
    }
}

pub struct DeferQueue {
    cap: usize,
    q: VecDeque<Job>,
    next_id: u64,
    events: EventSink,
    stats: DeferStats,
}

impl std::fmt::Debug for DeferQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeferQueue")
            .field("depth", &self.q.len())
            .field("cap", &self.cap)
            .field("stats", &self.stats)
            .finish()
    }
}

impl Default for DeferQueue {
    fn default() -> Self {
        DeferQueue::new(32)
    }
}

impl DeferQueue {
    pub fn new(cap: usize) -> Self {
        DeferQueue {
            cap: cap.max(1),
            q: VecDeque::new(),
            next_id: 0,
            events: EventSink::silent(),
            stats: DeferStats::default(),
        }
    }

    pub fn with_events(mut self, events: EventSink) -> Self {
        self.events = events;
        self
    }

    pub fn len(&self) -> usize {
        self.q.len()
    }
    pub fn is_empty(&self) -> bool {
        self.q.is_empty()
    }
    pub fn cap(&self) -> usize {
        self.cap
    }
    pub fn stats(&self) -> &DeferStats {
        &self.stats
    }
    pub fn peek(&self) -> Option<&Job> {
        self.q.front()
    }
    pub fn jobs(&self) -> impl Iterator<Item = &Job> {
        self.q.iter()
    }

    /// Should this tier be queueing rather than doing?
    pub fn should_defer(tier: Tier) -> bool {
        !tier.may_think()
    }

    pub fn push(&mut self, mut job: Job) -> Pushed {
        self.next_id += 1;
        job.id = self.next_id;

        let mut displaced = None;
        if self.q.len() >= self.cap {
            // The least urgent, and among equals the oldest. `Urgency` is
            // ordered `Whim < Notable < Answer < Alarm`, so idle chatter is
            // what goes and the operator's actual question is what stays.
            let victim = self
                .q
                .iter()
                .enumerate()
                .min_by(|(ia, a), (ib, b)| a.urgency.cmp(&b.urgency).then(ia.cmp(ib)))
                .map(|(i, _)| i);
            match victim {
                Some(i) if self.q[i].urgency <= job.urgency => {
                    let d = self.q.remove(i).expect("index from the same queue");
                    self.stats.dropped_full += 1;
                    self.record_drop(&d, DropReason::Full);
                    displaced = Some(d);
                }
                _ => {
                    // Everything queued is more urgent than this. Refusing the
                    // newcomer is the honest move, and it is still recorded.
                    self.stats.dropped_full += 1;
                    self.record_drop(&job, DropReason::Full);
                    return Pushed::Refused { job };
                }
            }
        }

        let id = job.id;
        let what = job.what.clone();
        self.q.push_back(job);
        self.stats.queued += 1;
        self.stats.high_water = self.stats.high_water.max(self.q.len());
        self.events.emit(EventKind::Deferred {
            what,
            queued: self.q.len(),
        });
        match displaced {
            Some(dropped) => Pushed::Displaced {
                id,
                depth: self.q.len(),
                dropped,
            },
            None => Pushed::Queued {
                id,
                depth: self.q.len(),
            },
        }
    }

    /// Everything that is still worth doing, oldest first, with the stale ones
    /// pulled out and recorded.
    pub fn replay(&mut self, now: Millis) -> Replayed {
        let mut out = Replayed::default();
        while let Some(job) = self.q.pop_front() {
            if job.is_stale(now) {
                self.stats.dropped_stale += 1;
                self.record_replay(&job, true);
                out.dropped.push((job, DropReason::Stale));
            } else {
                self.stats.replayed += 1;
                self.record_replay(&job, false);
                out.ready.push(job);
            }
        }
        out
    }

    /// Take only what is ready *now*, leaving the rest queued. For a partial
    /// upgrade where she is allowed to think but not allowed to think much.
    pub fn replay_some(&mut self, now: Millis, limit: usize) -> Replayed {
        let mut out = Replayed::default();
        while out.ready.len() < limit {
            let Some(job) = self.q.pop_front() else { break };
            if job.is_stale(now) {
                self.stats.dropped_stale += 1;
                self.record_replay(&job, true);
                out.dropped.push((job, DropReason::Stale));
            } else {
                self.stats.replayed += 1;
                self.record_replay(&job, false);
                out.ready.push(job);
            }
        }
        out
    }

    /// Throw the queue away — T4 Dormant is not "later".
    pub fn silence(&mut self) -> Vec<Job> {
        let gone: Vec<Job> = self.q.drain(..).collect();
        for j in &gone {
            self.record_drop(j, DropReason::Silenced);
        }
        gone
    }

    fn record_drop(&self, job: &Job, why: DropReason) {
        self.events.emit(EventKind::Dropped {
            text: job.what.clone(),
            why: why.as_str().to_string(),
        });
    }

    fn record_replay(&self, job: &Job, dropped: bool) {
        self.events.emit(EventKind::Replayed {
            what: job.what.clone(),
            dropped,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(kind: JobKind, what: &str, now: Millis) -> Job {
        Job::new(kind, what, now)
    }

    #[test]
    fn a_lobotomised_tier_queues_and_a_full_one_does_not() {
        assert!(DeferQueue::should_defer(Tier::Lobotomised));
        assert!(DeferQueue::should_defer(Tier::Dormant));
        assert!(!DeferQueue::should_defer(Tier::Reduced));
        assert!(!DeferQueue::should_defer(Tier::Full));
        assert!(!DeferQueue::should_defer(Tier::Feral));
    }

    #[test]
    fn replay_is_oldest_first() {
        let mut q = DeferQueue::new(8);
        for (i, t) in [(1u64, 0), (2, 100), (3, 200)] {
            q.push(job(JobKind::Remember, &format!("thing {i}"), t).stale_after(None));
        }
        let r = q.replay(300);
        assert_eq!(
            r.ready.iter().map(|j| j.what.as_str()).collect::<Vec<_>>(),
            vec!["thing 1", "thing 2", "thing 3"]
        );
        assert!(q.is_empty());
    }

    #[test]
    fn a_stale_thought_is_dropped_and_recorded_not_resurrected() {
        let (sink, log) = EventSink::collector();
        let mut q = DeferQueue::new(8).with_events(sink);
        q.push(job(JobKind::Remark, "your shaders are done", 0));
        q.push(job(JobKind::Reply, "yes, 47 windows", 0));

        // Two minutes later: the remark has expired, the answer has not.
        let r = q.replay(120_000);
        assert_eq!(r.ready.len(), 1);
        assert_eq!(r.ready[0].kind, JobKind::Reply);
        assert_eq!(r.dropped.len(), 1);
        assert_eq!(r.dropped[0].1, DropReason::Stale);

        let replayed = log.replayed();
        assert!(
            replayed.contains(&("your shaders are done".to_string(), true)),
            "the drop must be in the trace: {replayed:?}"
        );
        assert!(replayed.contains(&("yes, 47 windows".to_string(), false)));

        // And it is really gone.
        assert!(q.replay(120_001).is_empty());
        assert_eq!(q.stats().dropped_stale, 1);
    }

    #[test]
    fn a_full_queue_drops_idle_chatter_before_an_answer() {
        let (sink, log) = EventSink::collector();
        let mut q = DeferQueue::new(2).with_events(sink);
        q.push(job(JobKind::Remark, "whim one", 0).urgency(Urgency::Whim));
        q.push(job(JobKind::Reply, "the operator asked", 0).urgency(Urgency::Answer));

        let p = q.push(job(JobKind::Remark, "whim two", 10).urgency(Urgency::Notable));
        match p {
            Pushed::Displaced { dropped, depth, .. } => {
                assert_eq!(dropped.what, "whim one");
                assert_eq!(depth, 2);
            }
            other => panic!("expected a displacement, got {other:?}"),
        }
        assert!(q.jobs().any(|j| j.what == "the operator asked"));
        assert!(log
            .all()
            .iter()
            .any(|e| matches!(e, EventKind::Dropped { text, .. } if text == "whim one")));
    }

    #[test]
    fn a_whim_cannot_push_out_an_answer() {
        let mut q = DeferQueue::new(1);
        q.push(job(JobKind::Reply, "the operator asked", 0).urgency(Urgency::Answer));
        let p = q.push(job(JobKind::Remark, "idle thought", 1).urgency(Urgency::Whim));
        assert!(matches!(p, Pushed::Refused { .. }), "{p:?}");
        assert_eq!(q.len(), 1);
        assert_eq!(q.peek().expect("still there").what, "the operator asked");
    }

    #[test]
    fn the_queue_never_grows_past_its_bound_however_long_she_is_lobotomised() {
        let mut q = DeferQueue::new(4);
        for i in 0..500u64 {
            q.push(job(JobKind::Remark, &format!("thought {i}"), i * 10));
        }
        assert_eq!(q.len(), 4);
        assert_eq!(q.stats().high_water, 4);
        assert_eq!(q.stats().dropped_full, 496);
    }

    #[test]
    fn being_silenced_throws_the_queue_away_and_says_so() {
        let (sink, log) = EventSink::collector();
        let mut q = DeferQueue::new(8).with_events(sink);
        q.push(job(JobKind::Remark, "a", 0));
        q.push(job(JobKind::Reply, "b", 0));
        let gone = q.silence();
        assert_eq!(gone.len(), 2);
        assert!(q.is_empty());
        let dropped: Vec<String> = log
            .all()
            .into_iter()
            .filter_map(|e| match e {
                EventKind::Dropped { text, why } if why.contains("silenced") => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(dropped, vec!["a", "b"]);
    }
}
