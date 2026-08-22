//! F47 — the fleet hop: she walks off the edge of one machine and appears on
//! another, carrying her mood and her short-term memory.
//!
//! # The invariant
//!
//! **At most one machine has her at any instant, and exactly one once the link
//! is healthy.** Everything below exists to make that true and, more
//! importantly, *testable*: [`Presence`] is a pure state machine — no sockets,
//! no clock of its own — so a test can run two of them against a deliberately
//! lossy link and assert the invariant after every single step.
//!
//! # The shape of it
//!
//! Ownership is a `(epoch, owner)` pair. The epoch only ever increases, and a
//! hop is "please take ownership at epoch+1". Five messages, each carrying the
//! sender's monotonic `seq`:
//!
//! ```text
//! hop          { hop_id, from, to, seq, epoch, attempt, carry }  "I'm coming over"
//! hop-ack      { hop_id, accepted, refusal, known_epoch, … }     "you're here" / "no"
//! claim        { from, seq, epoch, owner }                       "this is what I know"
//! reclaim      { hop_id, epoch, … }                              "fence that hop off"
//! reclaim-ack  { hop_id, ok, known_epoch, known_owner, … }       "fenced" / "too late"
//! ```
//!
//! and one rule that does most of the work: **the sender goes quiet the moment
//! it sends `hop`, but stays owner-of-record until the ack arrives.** So a
//! refusal brings her straight home (no loss) and a lost ack cannot make her
//! exist twice (no duplication).
//!
//! ## The failure cases, deliberately
//!
//! * **Dropped ack.** The sender retries with the *same* `hop_id`. The receiver
//!   remembers what it decided per `hop_id` and re-acks that decision instead
//!   of committing a second time.
//! * **Ack lost for good.** After `max_attempts` she is *stranded*: asleep on
//!   both machines rather than awake on both. This is a deliberate choice of
//!   temporary loss over duplication — the two generals problem does not have a
//!   third option. It resolves as soon as the link comes back, via `reclaim`:
//!   the peer writes that `hop_id` off as refused *before* answering, so a copy
//!   of the original hop that is still sitting in a queue somewhere can never
//!   wake her afterwards. [`Presence::force_reclaim`] is the operator's manual
//!   override for a machine that is never coming back.
//! * **A straggling hop.** The randomised storm in `tests/hop.rs` found this
//!   one, and it is why `seq` and `reclaim` exist at all: a hop delivered
//!   *after* its sender gave up is, by epoch alone, still perfectly valid.
//! * **Simultaneous hops.** Both sides propose the same epoch. The tie is
//!   broken by node id — the *greater* id's hop wins, the same ordinal rule NX
//!   Hub already uses to decide which hub dials which — so both machines reach
//!   the same conclusion without another round trip.
//! * **Split brain** (two owners at one epoch, after a partition): `claim`
//!   settles it by epoch, then by the same ordinal tie-break.
//!
//! # What this needs from NX Hub
//!
//! The hub already federates: UDP beacon on 9022, WS on 9023, pairing by
//! six-digit code into a shared secret, per-message HMAC over the exact body
//! bytes with a monotonic sequence number. **This module rides that transport
//! rather than inventing one** — but as of hub v0.10 the fleet session carries
//! only hub-to-hub verbs (`summary`, `install`, `bus-roster`, …) and there is
//! no way for an *app* to send an authenticated message to its counterpart on a
//! paired machine.
//!
//! So this module is written against the one verb it needs, and degrades
//! cleanly when it is not there:
//!
//! ```text
//! client -> hub  {"type":"relay",     "peer":"<hub id|*>", "app":"nx-wisp", "body":{…}}
//! hub -> client  {"type":"relay",     "peer":"<hub id>",   "app":"nx-wisp", "body":{…}}
//! hub -> client  {"type":"relay-ack", "ok":true|false, "error":"…"}
//! ```
//!
//! The hub would wrap `body` in a fleet payload (`{"type":"app-relay", app,
//! from, body}`), send it over the session it already maintains with that peer,
//! and hand an inbound one to the local connector client with the matching app
//! id and the `relay` capability. Nothing about the pairing, the HMAC, the
//! sequence numbers or the sockets changes — the fleet link stays the hub's,
//! and an app can only ever reach *its own counterpart* on a paired machine.
//!
//! Until that exists, [`RelayTransport`] fails every send, the hop simply never
//! happens, and everything else here carries on: an older hub costs her the
//! hop and nothing else. [`mock::MockTransport`] stands in for the tests.

use std::collections::VecDeque;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A machine's identity in the fleet. In production this is NX Hub's own fleet
/// id (16 lowercase hex, minted once into `fleet.json`) so that "which machine"
/// means the same thing to her as it does to the hub.
pub type NodeId = String;

/// The hop payload cap. The connector frame limit is 16 KB and the fleet's is
/// 64 KB, so the tighter of the two decides, with room for the envelope.
pub const MAX_CARRY_BYTES: usize = 12 * 1024;

/// One remembered thing, small enough to cross a LAN.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryItem {
    pub text: String,
    /// Monotonic ms on the machine that recorded it. Machines do not share a
    /// clock, so this is provenance, never an ordering key across the hop.
    pub at_ms: u64,
    /// 0..=1. What gets dropped first when the carry has to shrink.
    pub weight: f32,
}

/// What she takes with her. `wisp-mind` decides what goes in; this crate only
/// promises to deliver it or to fail loudly.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Carry {
    /// Her mood FSM's current state, by name.
    pub mood: String,
    /// -1.0..=1.0.
    #[serde(default)]
    pub valence: f32,
    /// Short-term memory, newest last.
    #[serde(default)]
    pub memory: Vec<MemoryItem>,
    /// The skin she is wearing, so she does not arrive as someone else.
    #[serde(default)]
    pub skin: Option<String>,
    /// Room for a future subsystem to add its own scrap of state without a
    /// change here.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, Value>,
}

impl Carry {
    pub fn encoded_len(&self) -> usize {
        serde_json::to_string(self).map(|s| s.len()).unwrap_or(usize::MAX)
    }

    pub fn fits(&self) -> bool {
        self.encoded_len() <= MAX_CARRY_BYTES
    }

    /// Shrink until it fits: the least important memories go first, then the
    /// oldest. Her mood always survives — arriving in the wrong mood would be
    /// worse than arriving with less to say.
    pub fn trim_to_fit(&mut self) {
        while !self.fits() && !self.memory.is_empty() {
            let victim = self
                .memory
                .iter()
                .enumerate()
                .min_by(|(ai, a), (bi, b)| {
                    a.weight
                        .partial_cmp(&b.weight)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(ai.cmp(bi))
                })
                .map(|(i, _)| i);
            match victim {
                Some(i) => {
                    self.memory.remove(i);
                }
                None => break,
            }
        }
        if !self.fits() {
            self.extra.clear();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Refusal {
    /// The hop's epoch is not newer than what the receiver already knows.
    StaleEpoch,
    /// Addressed to someone else.
    NotMe,
    /// A simultaneous hop won the tie-break.
    Busy,
    /// The carry did not survive the trip intact.
    Malformed,
}

/// Every message carries `seq`: the sender's own strictly increasing counter,
/// covering *all* of its outbound hop traffic. The receiver ignores anything
/// with a `seq` it has already passed.
///
/// This is not belt-and-braces. Without it a hop that a slow link delivers
/// **after** its sender has given up and taken her back is still, by epoch
/// alone, a perfectly valid hop — and the receiver accepts it, and now she
/// exists twice. (The randomised storm in `tests/hop.rs` found exactly that.)
/// The counter is what makes "this is older than something I already heard
/// from you" expressible at all. NX Hub's fleet envelope has a monotonic
/// sequence for the same reason, per session; this one spans reconnects, so it
/// is persisted with the rest of the presence state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum HopMessage {
    Hop {
        hop_id: String,
        from: NodeId,
        to: NodeId,
        seq: u64,
        /// The epoch the sender is asking the receiver to own.
        epoch: u64,
        attempt: u32,
        carry: Carry,
    },
    HopAck {
        hop_id: String,
        from: NodeId,
        to: NodeId,
        seq: u64,
        epoch: u64,
        accepted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refusal: Option<Refusal>,
        /// The *committed* epoch of the sender of this ack — never a hop it has
        /// in flight. Advertising an uncommitted epoch is how both machines end
        /// up asleep waiting for each other.
        known_epoch: u64,
        known_owner: NodeId,
    },
    /// "Here is what I believe about who has her." Sent when a link comes up,
    /// and by a machine whose hop stalled.
    Claim {
        from: NodeId,
        seq: u64,
        epoch: u64,
        owner: NodeId,
    },
    /// **The fence.** "My hop `hop_id` was never acknowledged. If you have not
    /// taken it, promise me you never will, and I will take her back."
    ///
    /// This is what makes un-stranding safe. A bare "do you have her?" cannot
    /// be: the answer is only true at the instant it is given, and a copy of
    /// the original hop may still be sitting in the peer's queue behind it.
    /// A `reclaim` is answered by *recording the decision* — the peer writes
    /// the hop off as refused before replying, so the straggler, whenever it
    /// lands, is answered from that record instead of waking her a second time.
    Reclaim {
        from: NodeId,
        to: NodeId,
        seq: u64,
        hop_id: String,
        /// The epoch that hop was offering.
        epoch: u64,
    },
    ReclaimAck {
        from: NodeId,
        to: NodeId,
        seq: u64,
        hop_id: String,
        /// True: the hop is fenced off and she is yours again.
        /// False: too late — I have her.
        ok: bool,
        known_epoch: u64,
        known_owner: NodeId,
    },
}

impl HopMessage {
    pub fn from(&self) -> &str {
        match self {
            HopMessage::Hop { from, .. }
            | HopMessage::HopAck { from, .. }
            | HopMessage::Claim { from, .. }
            | HopMessage::Reclaim { from, .. }
            | HopMessage::ReclaimAck { from, .. } => from,
        }
    }

    pub fn seq(&self) -> u64 {
        match self {
            HopMessage::Hop { seq, .. }
            | HopMessage::HopAck { seq, .. }
            | HopMessage::Claim { seq, .. }
            | HopMessage::Reclaim { seq, .. }
            | HopMessage::ReclaimAck { seq, .. } => *seq,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    /// She is here and awake.
    Owner,
    /// She has stepped off the edge and the ack has not landed yet. Not
    /// visible, still owner-of-record.
    HandingOff,
    /// She is somewhere else, or stranded.
    Away,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepReason {
    /// She hopped to another machine.
    HoppedAway,
    /// Another machine holds a newer epoch.
    Superseded,
    /// A hop was never acknowledged. Asleep on both machines is the safe half
    /// of the trade; see the module docs.
    Stalled,
}

/// What the caller must do about a state change. `wisp-fleet` decides; the
/// binary carries it out.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Send(NodeId, HopMessage),
    /// Put her on this machine, with this state.
    Wake(Box<Carry>),
    /// Take her off this machine.
    Sleep(SleepReason),
    /// One line for the flight recorder.
    Note(String),
}

#[derive(Debug, Clone, Copy)]
pub struct HopConfig {
    /// Re-send an unacknowledged hop this often.
    pub retry_ms: u64,
    /// Give up (and strand her) after this many attempts.
    pub max_attempts: u32,
}

impl Default for HopConfig {
    fn default() -> Self {
        // ~6 s of trying on a LAN before she admits the other machine is not
        // answering. Long enough for a hub restart, short enough that the
        // operator notices her absence rather than waiting for it.
        Self { retry_ms: 1500, max_attempts: 4 }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Pending {
    hop_id: String,
    to: NodeId,
    epoch: u64,
    carry: Carry,
    attempt: u32,
    next_retry_at: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct Stranded {
    hop_id: String,
    to: NodeId,
    epoch: u64,
    carry: Carry,
}

#[derive(Debug, Clone, PartialEq)]
struct Decided {
    hop_id: String,
    accepted: bool,
    epoch: u64,
    refusal: Option<Refusal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HopError {
    #[error("she is not here to send")]
    NotOwner,
    #[error("a hop is already in flight")]
    AlreadyHopping,
    #[error("cannot hop to this machine")]
    SelfHop,
    #[error("this hub cannot relay to other machines")]
    RelayUnsupported,
}

/// Persisted across restarts: which machine had her, and at what epoch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PresenceState {
    pub me: NodeId,
    pub epoch: u64,
    pub owner: NodeId,
    pub phase: Phase,
    /// Our outbound counter. Persisted, because a peer remembers the highest
    /// one it has seen from us and will ignore anything below it.
    #[serde(default)]
    pub seq: u64,
}

/// The ownership state machine. Pure, deterministic, and the only thing in this
/// crate that is allowed to decide whether she exists here.
#[derive(Debug)]
pub struct Presence {
    me: NodeId,
    epoch: u64,
    owner: NodeId,
    phase: Phase,
    pending: Option<Pending>,
    /// The hop that stalled: everything needed to fence it off and take her
    /// back, rather than resurrecting a blank.
    stranded: Option<Stranded>,
    decided: VecDeque<Decided>,
    /// Our own outbound counter, and the highest we have accepted from each
    /// peer. See [`HopMessage`].
    seq: u64,
    heard: Vec<(NodeId, u64)>,
    cfg: HopConfig,
}

const DECIDED_CAP: usize = 64;
/// How far a restart jumps its sequence counter. See [`Presence::restore`].
const SEQ_RESTART_GAP: u64 = 1000;

impl Presence {
    /// She is here, at the first epoch.
    pub fn here(me: impl Into<NodeId>) -> Self {
        let me = me.into();
        Self {
            owner: me.clone(),
            me,
            epoch: 1,
            phase: Phase::Owner,
            pending: None,
            stranded: None,
            decided: VecDeque::new(),
            seq: 0,
            heard: Vec::new(),
            cfg: HopConfig::default(),
        }
    }

    /// This machine is one she might hop *to*, but she is not here yet.
    pub fn elsewhere(me: impl Into<NodeId>, owner: impl Into<NodeId>, epoch: u64) -> Self {
        Self {
            me: me.into(),
            owner: owner.into(),
            epoch,
            phase: Phase::Away,
            pending: None,
            stranded: None,
            decided: VecDeque::new(),
            seq: 0,
            heard: Vec::new(),
            cfg: HopConfig::default(),
        }
    }

    pub fn with_config(mut self, cfg: HopConfig) -> Self {
        self.cfg = cfg;
        self
    }

    /// **The invariant, as a function.** True on exactly one machine at a time.
    pub fn is_active(&self) -> bool {
        matches!(self.phase, Phase::Owner)
    }

    pub fn me(&self) -> &str {
        &self.me
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn owner(&self) -> &str {
        &self.owner
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    /// A hop went unanswered; she is asleep everywhere until the link returns.
    pub fn is_stranded(&self) -> bool {
        self.phase == Phase::Away && self.owner == self.me
    }

    pub fn snapshot(&self) -> PresenceState {
        PresenceState {
            me: self.me.clone(),
            epoch: self.epoch,
            owner: self.owner.clone(),
            phase: self.phase,
            seq: self.seq,
        }
    }

    pub fn restore(state: PresenceState, cfg: HopConfig) -> Self {
        Self {
            me: state.me,
            epoch: state.epoch,
            owner: state.owner,
            // A hop that was in flight when the process died is not in flight
            // any more; treat it as stalled and let `claim` sort it out.
            phase: if state.phase == Phase::HandingOff { Phase::Away } else { state.phase },
            pending: None,
            stranded: None,
            decided: VecDeque::new(),
            // Jump the counter forward: the last few sends may not have been
            // flushed to disk before we died, and a peer that remembers a
            // higher `seq` than ours would ignore everything we say.
            seq: state.seq + SEQ_RESTART_GAP,
            heard: Vec::new(),
            cfg,
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("tmp");
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&tmp, serde_json::to_vec_pretty(&self.snapshot())?)?;
        std::fs::rename(&tmp, path)
    }

    pub fn load(path: &Path, cfg: HopConfig) -> Option<Self> {
        let raw = std::fs::read_to_string(path).ok()?;
        serde_json::from_str::<PresenceState>(&raw).ok().map(|s| Presence::restore(s, cfg))
    }

    /// What we would tell a peer about ownership right now. Takes `&mut self`
    /// because every outbound message burns a sequence number.
    pub fn claim(&mut self) -> HopMessage {
        HopMessage::Claim {
            from: self.me.clone(),
            seq: self.next_seq(),
            epoch: self.epoch,
            owner: self.owner.clone(),
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Have we already heard something *later* than this from this peer?
    fn is_stale(&mut self, from: &str, seq: u64) -> bool {
        match self.heard.iter_mut().find(|(id, _)| id == from) {
            Some((_, last)) if seq <= *last => true,
            Some((_, last)) => {
                *last = seq;
                false
            }
            None => {
                self.heard.push((from.to_string(), seq));
                false
            }
        }
    }

    /// A link to `peer` came up: reconcile. If a hop to that peer stalled, this
    /// is also when we ask for her back.
    pub fn on_link_up(&mut self, peer: &str) -> Vec<Effect> {
        let mut effects = vec![Effect::Send(peer.to_string(), self.claim())];
        if let Some(fence) = self.reclaim_for(peer) {
            effects.push(Effect::Send(peer.to_string(), fence));
        }
        effects
    }

    /// Step off the edge of this screen towards `to`.
    ///
    /// She goes quiet immediately — that is the half of the invariant this
    /// machine controls — but stays owner-of-record until the ack lands, so a
    /// refusal or a timeout can bring her back without anyone else having had
    /// her in the meantime.
    pub fn begin_hop(
        &mut self,
        to: &str,
        mut carry: Carry,
        now_ms: u64,
    ) -> Result<Vec<Effect>, HopError> {
        if to == self.me {
            return Err(HopError::SelfHop);
        }
        if self.pending.is_some() {
            return Err(HopError::AlreadyHopping);
        }
        if !self.is_active() {
            return Err(HopError::NotOwner);
        }
        carry.trim_to_fit();
        let hop_id = new_hop_id();
        let epoch = self.epoch + 1;
        let msg = HopMessage::Hop {
            hop_id: hop_id.clone(),
            from: self.me.clone(),
            to: to.to_string(),
            seq: self.next_seq(),
            epoch,
            attempt: 1,
            carry: carry.clone(),
        };
        self.pending = Some(Pending {
            hop_id,
            to: to.to_string(),
            epoch,
            carry,
            attempt: 1,
            next_retry_at: now_ms.saturating_add(self.cfg.retry_ms),
        });
        self.phase = Phase::HandingOff;
        Ok(vec![
            Effect::Sleep(SleepReason::HoppedAway),
            Effect::Note(format!("hopping to {to} at epoch {epoch}")),
            Effect::Send(to.to_string(), msg),
        ])
    }

    /// Time passed: retry, or give up.
    pub fn tick(&mut self, now_ms: u64) -> Vec<Effect> {
        let Some(p) = self.pending.as_mut() else {
            return Vec::new();
        };
        if now_ms < p.next_retry_at {
            return Vec::new();
        }
        if p.attempt >= self.cfg.max_attempts {
            let stranded =
                Stranded { hop_id: p.hop_id.clone(), to: p.to.clone(), epoch: p.epoch, carry: p.carry.clone() };
            let to = stranded.to.clone();
            self.pending = None;
            self.stranded = Some(stranded);
            self.phase = Phase::Away;
            // Note what is true: she is asleep here, and possibly asleep there
            // too. Never awake in both places. The claim is the question that
            // ends the stranding as soon as anything gets through: "this is
            // what I know — correct me, or confirm it and I will take her back".
            let fence = self.reclaim_for(&to).expect("just stranded");
            return vec![
                Effect::Note(format!("hop to {to} was never acknowledged; stranded")),
                Effect::Sleep(SleepReason::Stalled),
                Effect::Send(to, fence),
            ];
        }
        p.attempt += 1;
        p.next_retry_at = now_ms.saturating_add(self.cfg.retry_ms);
        let (hop_id, to, epoch, attempt, carry) =
            (p.hop_id.clone(), p.to.clone(), p.epoch, p.attempt, p.carry.clone());
        let msg = HopMessage::Hop {
            hop_id,
            from: self.me.clone(),
            to: to.clone(),
            seq: self.next_seq(),
            epoch,
            attempt,
            carry,
        };
        vec![Effect::Send(to, msg)]
    }

    /// The operator's override for a stranded wisp: take her back, at a fresh
    /// epoch that beats anything the silent machine could be holding.
    pub fn force_reclaim(&mut self, _now_ms: u64) -> Vec<Effect> {
        // Beat the hop that is in flight too, not just the committed epoch —
        // otherwise a late-arriving ack for that hop would out-rank the
        // operator's decision.
        let pending = self.pending.take();
        let stranded = self.stranded.take();
        // Beat every epoch this machine ever proposed, not just the committed
        // one: the silent machine may have taken any of them.
        let proposed = pending
            .as_ref()
            .map(|p| p.epoch)
            .unwrap_or(0)
            .max(stranded.as_ref().map(|s| s.epoch).unwrap_or(0));
        self.epoch = self.epoch.max(proposed) + 1;
        self.owner = self.me.clone();
        self.phase = Phase::Owner;
        // Whichever copy of her state we still hold: the one that was mid-hop,
        // or the one a stall left behind.
        let carry = pending.map(|p| p.carry).or(stranded.map(|s| s.carry)).unwrap_or_default();
        vec![
            Effect::Note(format!("operator reclaimed her at epoch {}", self.epoch)),
            Effect::Wake(Box::new(carry)),
        ]
    }

    /// `_now_ms` is accepted for symmetry with [`Presence::tick`] and because a
    /// future condition (a lease, say) will need it; none of today's branches
    /// depend on the clock, which is exactly why they are easy to test.
    pub fn on_message(&mut self, msg: &HopMessage, _now_ms: u64) -> Vec<Effect> {
        // Anything older than what we have already heard from this machine is
        // a straggler from before its last word, and acting on it is how she
        // ends up in two places at once.
        if self.is_stale(msg.from(), msg.seq()) {
            return vec![Effect::Note(format!(
                "ignored a straggler from {} (seq {})",
                msg.from(),
                msg.seq()
            ))];
        }
        match msg {
            HopMessage::Hop { hop_id, from, to, epoch, carry, .. } => {
                self.on_hop(hop_id, from, to, *epoch, carry)
            }
            HopMessage::HopAck { hop_id, from, epoch, accepted, refusal, known_epoch, known_owner, .. } => {
                self.on_ack(hop_id, from, *epoch, *accepted, *refusal, *known_epoch, known_owner)
            }
            HopMessage::Claim { from, epoch, owner, .. } => self.on_claim(from, *epoch, owner),
            HopMessage::Reclaim { from, to, hop_id, epoch, .. } => {
                self.on_reclaim(from, to, hop_id, *epoch)
            }
            HopMessage::ReclaimAck { from, hop_id, ok, known_epoch, known_owner, .. } => {
                self.on_reclaim_ack(from, hop_id, *ok, *known_epoch, known_owner)
            }
        }
    }

    /// The fence request for a stalled hop to `peer`, if that is where she went.
    fn reclaim_for(&mut self, peer: &str) -> Option<HopMessage> {
        let (hop_id, epoch) = {
            let s = self.stranded.as_ref()?;
            if s.to != peer {
                return None;
            }
            (s.hop_id.clone(), s.epoch)
        };
        Some(HopMessage::Reclaim {
            from: self.me.clone(),
            to: peer.to_string(),
            seq: self.next_seq(),
            hop_id,
            epoch,
        })
    }

    /// Someone's hop to us stalled and they want her back.
    fn on_reclaim(&mut self, from: &str, to: &str, hop_id: &str, epoch: u64) -> Vec<Effect> {
        if to != self.me {
            return Vec::new();
        }
        let taken = self.decided.iter().any(|d| d.hop_id == hop_id && d.accepted);
        if !taken {
            // Write it off *before* answering. From here on that hop_id can
            // only ever be refused, however late it arrives.
            self.remember(Decided {
                hop_id: hop_id.to_string(),
                accepted: false,
                epoch,
                refusal: Some(Refusal::StaleEpoch),
            });
        }
        let ack = HopMessage::ReclaimAck {
            from: self.me.clone(),
            to: from.to_string(),
            seq: self.next_seq(),
            hop_id: hop_id.to_string(),
            ok: !taken,
            known_epoch: self.epoch,
            known_owner: self.owner.clone(),
        };
        vec![
            Effect::Note(format!(
                "{from} asked for {hop_id} back: {}",
                if taken { "too late, she is here" } else { "fenced off" }
            )),
            Effect::Send(from.to_string(), ack),
        ]
    }

    /// The answer to our fence request.
    fn on_reclaim_ack(
        &mut self,
        from: &str,
        hop_id: &str,
        ok: bool,
        known_epoch: u64,
        known_owner: &str,
    ) -> Vec<Effect> {
        let Some(stranded) = self.stranded.as_ref().filter(|s| s.hop_id == hop_id).cloned() else {
            return vec![Effect::Note(format!("ignored a stale reclaim-ack for {hop_id}"))];
        };
        self.stranded = None;
        if ok {
            // The epoch we were offering is now provably unused, so we may take
            // it ourselves and outrank anything older.
            self.epoch = self.epoch.max(stranded.epoch);
            self.owner = self.me.clone();
            self.phase = Phase::Owner;
            return vec![
                Effect::Note(format!("{from} fenced off {hop_id}; she is mine again")),
                Effect::Wake(Box::new(stranded.carry)),
            ];
        }
        if known_epoch > self.epoch {
            self.epoch = known_epoch;
            self.owner = known_owner.to_string();
        }
        self.phase = Phase::Away;
        vec![Effect::Note(format!("{from} had her after all, at epoch {known_epoch}"))]
    }

    fn on_hop(
        &mut self,
        hop_id: &str,
        from: &str,
        to: &str,
        epoch: u64,
        carry: &Carry,
    ) -> Vec<Effect> {
        if to != self.me {
            return vec![Effect::Note(format!("hop {hop_id} was not addressed to me"))];
        }
        // Idempotence. A retry after a dropped ack must replay the decision,
        // never make a second one.
        if let Some(prior) = self.decided.iter().find(|d| d.hop_id == hop_id).cloned() {
            return vec![
                Effect::Note(format!("hop {hop_id} already decided; re-acking")),
                Effect::Send(
                    from.to_string(),
                    self.ack(hop_id, from, prior.epoch, prior.accepted, prior.refusal),
                ),
            ];
        }

        let (accept, refusal) = if epoch <= self.epoch {
            (false, Some(Refusal::StaleEpoch))
        } else if let Some(p) = &self.pending {
            if p.epoch > epoch {
                (false, Some(Refusal::StaleEpoch))
            } else if p.epoch == epoch {
                // Simultaneous hop. Both sides run this same comparison and
                // reach opposite, consistent conclusions.
                if from > self.me.as_str() {
                    (true, None)
                } else {
                    (false, Some(Refusal::Busy))
                }
            } else {
                (true, None)
            }
        } else {
            (true, None)
        };

        let mut effects = Vec::new();
        if accept {
            if let Some(p) = self.pending.take() {
                effects.push(Effect::Note(format!(
                    "cancelled my own hop {} — {from} won the tie-break",
                    p.hop_id
                )));
            }
            self.epoch = epoch;
            self.owner = self.me.clone();
            self.phase = Phase::Owner;
            self.stranded = None;
            effects.push(Effect::Note(format!("she arrived from {from} at epoch {epoch}")));
            effects.push(Effect::Wake(Box::new(carry.clone())));
        } else {
            effects.push(Effect::Note(format!(
                "refused hop {hop_id} from {from}: {refusal:?}"
            )));
        }
        self.remember(Decided {
            hop_id: hop_id.to_string(),
            accepted: accept,
            epoch,
            refusal,
        });
        effects.push(Effect::Send(from.to_string(), self.ack(hop_id, from, epoch, accept, refusal)));
        effects
    }

    fn ack(
        &mut self,
        hop_id: &str,
        to: &str,
        epoch: u64,
        accepted: bool,
        refusal: Option<Refusal>,
    ) -> HopMessage {
        HopMessage::HopAck {
            hop_id: hop_id.to_string(),
            seq: self.next_seq(),
            from: self.me.clone(),
            to: to.to_string(),
            epoch,
            accepted,
            refusal,
            // Committed, never pending: see the struct's doc comment.
            known_epoch: self.epoch,
            known_owner: self.owner.clone(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn on_ack(
        &mut self,
        hop_id: &str,
        from: &str,
        epoch: u64,
        accepted: bool,
        refusal: Option<Refusal>,
        known_epoch: u64,
        known_owner: &str,
    ) -> Vec<Effect> {
        let Some(p) = self.pending.as_ref().filter(|p| p.hop_id == hop_id).cloned() else {
            // A duplicate ack, or one for a hop we already abandoned.
            return vec![Effect::Note(format!("ignored a stale ack for {hop_id}"))];
        };
        self.pending = None;

        if accepted {
            self.epoch = epoch.max(self.epoch);
            self.owner = p.to.clone();
            self.phase = Phase::Away;
            self.stranded = None;
            return vec![Effect::Note(format!("she is on {} now, at epoch {}", p.to, self.epoch))];
        }

        let mut effects =
            vec![Effect::Note(format!("{from} refused the hop: {refusal:?}"))];
        if known_epoch > self.epoch {
            self.epoch = known_epoch;
            self.owner = known_owner.to_string();
        }
        if self.owner == self.me {
            // Nobody else took her: she comes home, exactly as she left.
            self.phase = Phase::Owner;
            effects.push(Effect::Wake(Box::new(p.carry)));
        } else {
            self.phase = Phase::Away;
            self.stranded = Some(Stranded {
                hop_id: p.hop_id.clone(),
                to: p.to.clone(),
                epoch: p.epoch,
                carry: p.carry,
            });
            effects.push(Effect::Sleep(SleepReason::Superseded));
        }
        effects
    }

    fn on_claim(&mut self, from: &str, epoch: u64, owner: &str) -> Vec<Effect> {
        let was_active = self.is_active();
        let mut effects = Vec::new();

        let adopt = if epoch > self.epoch {
            true
        } else if epoch == self.epoch && owner != self.owner {
            // Split brain at one epoch: the greater id wins, so both machines
            // agree without another round trip.
            owner > self.owner.as_str()
        } else {
            false
        };

        if adopt {
            self.epoch = epoch;
            self.owner = owner.to_string();
            if self.owner == self.me {
                // A peer says she is mine and I did not know: take her, but say
                // so — her carry did not survive whatever went wrong.
                if !was_active {
                    self.phase = Phase::Owner;
                    let carry = self.stranded.take().map(|s| s.carry).unwrap_or_default();
                    effects.push(Effect::Note(
                        "resumed ownership from a peer's claim (state may be incomplete)".into(),
                    ));
                    effects.push(Effect::Wake(Box::new(carry)));
                }
            } else {
                self.pending = None;
                self.phase = Phase::Away;
                if was_active {
                    effects.push(Effect::Sleep(SleepReason::Superseded));
                }
                effects.push(Effect::Note(format!("{owner} has her at epoch {epoch}")));
            }
        } else if self.is_stranded() {
            // Hearing from the peer at all is the cue to try the fence. Note
            // what we do *not* do: take her back because the peer's claim
            // happens to agree that she is ours. That answer is only true at
            // the instant it was sent, and the original hop may still be
            // queued behind it — which is exactly how the randomised storm in
            // `tests/hop.rs` produced two of her.
            if let Some(fence) = self.reclaim_for(from) {
                effects.push(Effect::Send(from.to_string(), fence));
            }
            if epoch < self.epoch || owner != self.owner {
                effects.push(Effect::Send(from.to_string(), self.claim()));
            }
        } else if epoch < self.epoch
            || owner != self.owner
            // A peer claiming *itself* is asking a question — usually "my hop
            // stalled, do you have her?" — and silence is not an answer. Our
            // reply cannot loop: it names someone other than us as owner, and
            // that branch never replies again.
            || owner == from
        {
            effects.push(Effect::Send(from.to_string(), self.claim()));
        }
        effects
    }

    fn remember(&mut self, d: Decided) {
        if self.decided.len() == DECIDED_CAP {
            self.decided.pop_front();
        }
        self.decided.push_back(d);
    }
}

fn new_hop_id() -> String {
    crate::ws::random_hex(8)
}

/// How a [`HopMessage`] gets to the other machine.
///
/// Deliberately synchronous and fallible-by-return: the state machine treats
/// "the transport said no" exactly like "the message was lost", which is a
/// case it already has to survive.
pub trait HopTransport: Send + Sync {
    fn send(&self, to: &NodeId, msg: &HopMessage) -> bool;
    /// A hint only. `send` may still fail.
    fn reachable(&self, _to: &NodeId) -> bool {
        true
    }
}

/// The production transport: hand the message to the local hub and let it go
/// out over the authenticated fleet session it already maintains.
///
/// **This depends on a hub verb that does not exist yet** (see the module
/// docs). Until it does, [`ConnectorHandle::relay_supported`] stays false and
/// every send returns false, which the state machine reads as an unreachable
/// peer — she simply never hops, and nothing else misbehaves.
///
/// [`ConnectorHandle::relay_supported`]: crate::connector::ConnectorHandle::relay_supported
pub struct RelayTransport {
    conn: crate::connector::ConnectorHandle,
    app: String,
}

impl RelayTransport {
    pub fn new(conn: crate::connector::ConnectorHandle, app: impl Into<String>) -> Self {
        Self { conn, app: app.into() }
    }
}

impl HopTransport for RelayTransport {
    fn send(&self, to: &NodeId, msg: &HopMessage) -> bool {
        let Ok(body) = serde_json::to_value(msg) else {
            return false;
        };
        self.conn.send_relay(to.clone(), self.app.clone(), body)
    }

    fn reachable(&self, _to: &NodeId) -> bool {
        self.conn.connected()
    }
}

/// [`Presence`] plus a transport plus a file: the whole hop, driveable.
///
/// The state machine stays pure; this is the thin shell that actually sends the
/// messages it asks for and writes the result down. Every method returns the
/// *local* effects only — `Wake`, `Sleep`, `Note` — because the sends have
/// already happened by the time it returns.
pub struct Hopper<T: HopTransport> {
    presence: Presence,
    transport: T,
    path: Option<std::path::PathBuf>,
}

impl<T: HopTransport> Hopper<T> {
    pub fn new(presence: Presence, transport: T, path: Option<std::path::PathBuf>) -> Self {
        Self { presence, transport, path }
    }

    /// Restore from disk if we have been here before, else start with her here.
    pub fn load_or_here(
        me: &str,
        transport: T,
        path: std::path::PathBuf,
        cfg: HopConfig,
    ) -> Self {
        let presence = Presence::load(&path, cfg)
            .filter(|p| p.me() == me)
            .unwrap_or_else(|| Presence::here(me).with_config(cfg));
        Self { presence, transport, path: Some(path) }
    }

    pub fn presence(&self) -> &Presence {
        &self.presence
    }

    /// Is she on this machine right now?
    pub fn is_active(&self) -> bool {
        self.presence.is_active()
    }

    pub fn hop_to(
        &mut self,
        peer: &str,
        carry: Carry,
        now_ms: u64,
    ) -> Result<Vec<Effect>, HopError> {
        if !self.transport.reachable(&peer.to_string()) {
            // Better to refuse than to go quiet on a hop that cannot land.
            return Err(HopError::RelayUnsupported);
        }
        let effects = self.presence.begin_hop(peer, carry, now_ms)?;
        Ok(self.dispatch(effects))
    }

    pub fn on_message(&mut self, msg: &HopMessage, now_ms: u64) -> Vec<Effect> {
        let effects = self.presence.on_message(msg, now_ms);
        self.dispatch(effects)
    }

    pub fn tick(&mut self, now_ms: u64) -> Vec<Effect> {
        let effects = self.presence.tick(now_ms);
        self.dispatch(effects)
    }

    pub fn on_link_up(&mut self, peer: &str) -> Vec<Effect> {
        let effects = self.presence.on_link_up(peer);
        self.dispatch(effects)
    }

    pub fn force_reclaim(&mut self, now_ms: u64) -> Vec<Effect> {
        let effects = self.presence.force_reclaim(now_ms);
        self.dispatch(effects)
    }

    fn dispatch(&mut self, effects: Vec<Effect>) -> Vec<Effect> {
        let mut local = Vec::with_capacity(effects.len());
        for effect in effects {
            match effect {
                Effect::Send(to, msg) => {
                    if !self.transport.send(&to, &msg) {
                        // A transport that says no is a message that was lost,
                        // and the state machine already survives those.
                        tracing::debug!(peer = %to, "hop message could not be sent");
                    }
                }
                other => local.push(other),
            }
        }
        if let Some(path) = &self.path {
            if let Err(e) = self.presence.save(path) {
                tracing::warn!(error = %e, "could not persist presence");
            }
        }
        local
    }
}

/// An in-memory fleet, for tests and for anyone who wants to exercise the hop
/// without two machines.
pub mod mock {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Net {
        inboxes: HashMap<NodeId, Vec<HopMessage>>,
        /// Links that are currently down, by (from, to).
        cut: Vec<(NodeId, NodeId)>,
        /// Drop this many more messages, whatever the link state.
        drop_next: usize,
        sent: usize,
        dropped: usize,
    }

    /// A shared, controllable network.
    #[derive(Clone, Default)]
    pub struct MockNet {
        inner: Arc<Mutex<Net>>,
    }

    impl MockNet {
        pub fn new() -> Self {
            Self::default()
        }

        /// A transport that sends *from* this node.
        pub fn transport(&self, from: impl Into<NodeId>) -> MockTransport {
            MockTransport { net: self.clone(), from: from.into() }
        }

        /// Take everything waiting for `node`.
        pub fn take(&self, node: &str) -> Vec<HopMessage> {
            let mut net = self.inner.lock().expect("mock net poisoned");
            net.inboxes.remove(node).unwrap_or_default()
        }

        pub fn pending(&self, node: &str) -> usize {
            self.inner.lock().expect("mock net poisoned").inboxes.get(node).map_or(0, Vec::len)
        }

        /// Drop the next `n` messages, wherever they are going.
        pub fn drop_next(&self, n: usize) {
            self.inner.lock().expect("mock net poisoned").drop_next += n;
        }

        pub fn cut(&self, from: &str, to: &str) {
            self.inner
                .lock()
                .expect("mock net poisoned")
                .cut
                .push((from.to_string(), to.to_string()));
        }

        pub fn heal(&self) {
            let mut net = self.inner.lock().expect("mock net poisoned");
            net.cut.clear();
            net.drop_next = 0;
        }

        pub fn stats(&self) -> (usize, usize) {
            let net = self.inner.lock().expect("mock net poisoned");
            (net.sent, net.dropped)
        }
    }

    pub struct MockTransport {
        net: MockNet,
        from: NodeId,
    }

    impl HopTransport for MockTransport {
        fn send(&self, to: &NodeId, msg: &HopMessage) -> bool {
            let mut net = self.net.inner.lock().expect("mock net poisoned");
            net.sent += 1;
            if net.drop_next > 0 {
                net.drop_next -= 1;
                net.dropped += 1;
                return false;
            }
            if net.cut.iter().any(|(f, t)| f == &self.from && t == to) {
                net.dropped += 1;
                return false;
            }
            // Round-trip through JSON, so a message that could not survive the
            // wire cannot pass a test either.
            let text = match serde_json::to_string(msg) {
                Ok(t) => t,
                Err(_) => return false,
            };
            let decoded: HopMessage = match serde_json::from_str(&text) {
                Ok(m) => m,
                Err(_) => return false,
            };
            net.inboxes.entry(to.clone()).or_default().push(decoded);
            true
        }

        fn reachable(&self, to: &NodeId) -> bool {
            let net = self.net.inner.lock().expect("mock net poisoned");
            !net.cut.iter().any(|(f, t)| f == &self.from && t == to)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_carry_that_is_too_big_sheds_the_least_important_memories_first() {
        let mut carry = Carry { mood: "wistful".into(), valence: -0.2, ..Carry::default() };
        for i in 0..400 {
            carry.memory.push(MemoryItem {
                text: format!("thought number {i} {}", "x".repeat(64)),
                at_ms: i as u64,
                weight: if i == 0 { 1.0 } else { 0.1 },
            });
        }
        assert!(!carry.fits());
        carry.trim_to_fit();
        assert!(carry.fits());
        assert_eq!(carry.mood, "wistful");
        assert!(carry.memory.iter().any(|m| m.weight == 1.0), "the important one survived");
    }

    #[test]
    fn messages_round_trip_through_json() {
        let msg = HopMessage::Hop {
            hop_id: "abc".into(),
            from: "aaaa".into(),
            to: "bbbb".into(),
            seq: 3,
            epoch: 7,
            attempt: 1,
            carry: Carry { mood: "curious".into(), ..Carry::default() },
        };
        let text = serde_json::to_string(&msg).unwrap();
        assert!(text.contains("\"type\":\"hop\""));
        assert_eq!(serde_json::from_str::<HopMessage>(&text).unwrap(), msg);
    }

    #[test]
    fn state_survives_a_restart_as_away_never_as_mid_hop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("presence.json");
        let mut a = Presence::here("aaaa");
        a.begin_hop("bbbb", Carry::default(), 0).unwrap();
        a.save(&path).unwrap();
        let restored = Presence::load(&path, HopConfig::default()).unwrap();
        assert_eq!(restored.phase(), Phase::Away);
        assert!(!restored.is_active(), "a process that died mid-hop must not wake up owning her");
    }
}
