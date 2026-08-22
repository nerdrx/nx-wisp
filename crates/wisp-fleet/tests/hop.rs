//! F47 — the hop invariant, hammered.
//!
//! > **At most one machine has her at any instant, and exactly one once the
//! > link is healthy.**
//!
//! Every test below asserts the first half after *every single step* (see
//! [`World::step`]), which is the only way to catch a duplication that exists
//! for one message and then heals. The second half is asserted after the
//! network is repaired and both sides have gone quiet.
//!
//! No sockets, no hub, no peer machine: [`Presence`] is pure and the network is
//! [`wisp_fleet::hop::mock`].

use wisp_fleet::hop::mock::{MockNet, MockTransport};
use wisp_fleet::hop::{
    Carry, Effect, HopConfig, HopMessage, HopTransport, Hopper, MemoryItem, Phase, Presence,
    SleepReason,
};

fn isolate() -> tempfile::TempDir {
    // SPEC §4: never let a test touch the operator's real state.
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("NX_WISP_CONFIG_DIR", dir.path());
    dir
}

fn carry(mood: &str) -> Carry {
    Carry {
        mood: mood.into(),
        valence: 0.4,
        memory: vec![MemoryItem {
            text: "the operator was swearing at a shader".into(),
            at_ms: 1234,
            weight: 0.8,
        }],
        skin: Some("default".into()),
        extra: Default::default(),
    }
}

struct Node {
    presence: Presence,
    transport: MockTransport,
    awake: bool,
    /// What she was carrying when she last woke up here.
    carry: Carry,
    wakes: usize,
    sleeps: Vec<SleepReason>,
}

struct World {
    net: MockNet,
    nodes: Vec<Node>,
    now: u64,
    log: Vec<String>,
    /// Set only by the tests that *deliberately* start from a forked state —
    /// a restored backup, a cloned VM — which the protocol has to converge
    /// from even though it can never produce it.
    fork_allowed: bool,
}

impl World {
    /// The first id starts with her; the rest are machines she might hop to.
    fn new(ids: &[&str]) -> Self {
        let net = MockNet::new();
        let cfg = HopConfig { retry_ms: 1000, max_attempts: 3 };
        let nodes = ids
            .iter()
            .enumerate()
            .map(|(i, id)| Node {
                presence: if i == 0 {
                    Presence::here(*id).with_config(cfg)
                } else {
                    Presence::elsewhere(*id, ids[0], 1).with_config(cfg)
                },
                transport: net.transport(*id),
                awake: i == 0,
                carry: Carry::default(),
                wakes: usize::from(i == 0),
                sleeps: Vec::new(),
            })
            .collect();
        World { net, nodes, now: 0, log: Vec::new(), fork_allowed: false }
    }

    fn idx(&self, id: &str) -> usize {
        self.nodes.iter().position(|n| n.presence.me() == id).expect("no such node")
    }

    fn apply(&mut self, i: usize, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Send(to, msg) => {
                    let sent = self.nodes[i].transport.send(&to, &msg);
                    self.log.push(format!(
                        "{} -> {to} {}{}",
                        self.nodes[i].presence.me(),
                        kind(&msg),
                        if sent { "" } else { " (LOST)" }
                    ));
                }
                Effect::Wake(c) => {
                    self.nodes[i].awake = true;
                    self.nodes[i].wakes += 1;
                    self.nodes[i].carry = *c;
                }
                Effect::Sleep(reason) => {
                    self.nodes[i].awake = false;
                    self.nodes[i].sleeps.push(reason);
                }
                Effect::Note(note) => {
                    self.log.push(format!("{}: {note}", self.nodes[i].presence.me()))
                }
            }
        }
        self.check();
    }

    /// **The invariant.** Called after every step of every test.
    fn check(&self) {
        let awake: Vec<&str> =
            self.nodes.iter().filter(|n| n.awake).map(|n| n.presence.me()).collect();
        assert!(
            self.fork_allowed || awake.len() <= 1,
            "she exists on {awake:?} at once\ntrace:\n  {}",
            self.log.join("\n  ")
        );
        for node in &self.nodes {
            // The dangerous direction: she is never on a screen whose own state
            // machine says she is somewhere else.
            assert!(
                !node.awake || node.presence.is_active(),
                "{} is showing her while believing {} has her",
                node.presence.me(),
                node.presence.owner()
            );
        }
    }

    fn hop(&mut self, from: &str, to: &str, c: Carry) {
        let i = self.idx(from);
        let effects = self.nodes[i].presence.begin_hop(to, c, self.now).expect("hop refused");
        self.apply(i, effects);
    }

    /// Deliver everything in flight, one message at a time, checking as we go.
    fn deliver(&mut self) -> usize {
        let mut delivered = 0;
        loop {
            let mut any = false;
            for i in 0..self.nodes.len() {
                let id = self.nodes[i].presence.me().to_string();
                for msg in self.net.take(&id) {
                    any = true;
                    delivered += 1;
                    self.log.push(format!("{id} <- {}", kind(&msg)));
                    let effects = self.nodes[i].presence.on_message(&msg, self.now);
                    self.apply(i, effects);
                }
            }
            if !any {
                return delivered;
            }
        }
    }

    fn advance(&mut self, ms: u64) {
        self.now += ms;
        for i in 0..self.nodes.len() {
            let effects = self.nodes[i].presence.tick(self.now);
            self.apply(i, effects);
        }
    }

    /// Repair the network and let everybody reconcile.
    fn settle(&mut self) {
        self.net.heal();
        let ids: Vec<String> =
            self.nodes.iter().map(|n| n.presence.me().to_string()).collect();
        for i in 0..self.nodes.len() {
            for peer in &ids {
                if peer != self.nodes[i].presence.me() {
                    let effects = self.nodes[i].presence.on_link_up(peer);
                    self.apply(i, effects);
                }
            }
        }
        // Several rounds: a stall that only fires on a later tick emits its own
        // claim, and that claim needs delivering too.
        for _ in 0..6 {
            self.deliver();
            self.advance(2000);
            self.deliver();
            for i in 0..self.nodes.len() {
                for peer in &ids {
                    if peer != self.nodes[i].presence.me() {
                        let effects = self.nodes[i].presence.on_link_up(peer);
                        self.apply(i, effects);
                    }
                }
            }
        }
        self.deliver();
    }

    fn awake(&self) -> Vec<&str> {
        self.nodes.iter().filter(|n| n.awake).map(|n| n.presence.me()).collect()
    }

    fn node(&self, id: &str) -> &Node {
        &self.nodes[self.idx(id)]
    }
}

fn kind(msg: &HopMessage) -> String {
    match msg {
        HopMessage::Hop { epoch, attempt, seq, .. } => format!("hop(e{epoch} a{attempt} s{seq})"),
        HopMessage::HopAck { epoch, accepted, refusal, .. } => {
            format!("ack(e{epoch} {}{})", accepted, refusal.map(|r| format!(" {r:?}")).unwrap_or_default())
        }
        HopMessage::Claim { epoch, owner, seq, .. } => format!("claim(e{epoch} {owner} s{seq})"),
        HopMessage::Reclaim { hop_id, epoch, seq, .. } => {
            format!("reclaim({hop_id} e{epoch} s{seq})")
        }
        HopMessage::ReclaimAck { hop_id, ok, known_epoch, seq, .. } => {
            format!("reclaim-ack({hop_id} ok={ok} e{known_epoch} s{seq})")
        }
    }
}

#[test]
fn she_walks_across_and_arrives_with_her_mood_and_her_memory() {
    let _dir = isolate();
    let mut w = World::new(&["aaaa", "bbbb"]);
    assert_eq!(w.awake(), vec!["aaaa"]);

    w.hop("aaaa", "bbbb", carry("delighted"));
    // The instant she steps off the edge she is gone from the first machine —
    // before anything has been acknowledged. That is the half of the invariant
    // this machine can guarantee alone.
    assert_eq!(w.awake(), Vec::<&str>::new());

    w.deliver();
    assert_eq!(w.awake(), vec!["bbbb"]);
    assert_eq!(w.node("bbbb").carry.mood, "delighted");
    assert_eq!(w.node("bbbb").carry.memory.len(), 1);
    assert_eq!(w.node("bbbb").carry.skin.as_deref(), Some("default"));
    assert_eq!(w.node("aaaa").presence.owner(), "bbbb");
    assert_eq!(w.node("bbbb").presence.epoch(), 2);
    assert_eq!(w.node("aaaa").sleeps, vec![SleepReason::HoppedAway]);
}

#[test]
fn a_dropped_ack_is_retried_and_the_receiver_does_not_commit_twice() {
    let _dir = isolate();
    let mut w = World::new(&["aaaa", "bbbb"]);
    w.hop("aaaa", "bbbb", carry("curious"));

    // The hop lands, B accepts and wakes — and then the ack evaporates.
    w.net.drop_next(0);
    let i = w.idx("bbbb");
    for msg in w.net.take("bbbb") {
        let effects = w.nodes[i].presence.on_message(&msg, w.now);
        w.net.drop_next(1); // …the ack it is about to send
        w.apply(i, effects);
    }
    assert_eq!(w.awake(), vec!["bbbb"]);
    assert_eq!(w.node("aaaa").presence.phase(), Phase::HandingOff);

    // A retries with the same hop_id; B replays its decision.
    w.advance(1000);
    w.deliver();
    assert_eq!(w.awake(), vec!["bbbb"]);
    assert_eq!(w.node("bbbb").wakes, 1, "an idempotent hop must not re-arrive");
    assert_eq!(w.node("bbbb").presence.epoch(), 2, "…nor bump the epoch again");
    assert_eq!(w.node("aaaa").presence.phase(), Phase::Away);
    assert_eq!(w.node("aaaa").presence.owner(), "bbbb");
}

#[test]
fn an_ack_lost_for_good_strands_her_rather_than_duplicating_her() {
    let _dir = isolate();
    let mut w = World::new(&["aaaa", "bbbb"]);
    // B can hear A, but A can never hear B.
    w.net.cut("bbbb", "aaaa");
    w.hop("aaaa", "bbbb", carry("wistful"));
    w.deliver();
    assert_eq!(w.awake(), vec!["bbbb"], "she did arrive");

    // A retries, never hears anything, and gives up.
    for _ in 0..5 {
        w.advance(1000);
        w.deliver();
    }
    assert_eq!(w.awake(), vec!["bbbb"], "still exactly one of her");
    assert_eq!(w.node("aaaa").presence.phase(), Phase::Away);
    assert_eq!(w.node("aaaa").sleeps.last(), Some(&SleepReason::Stalled));

    // The link comes back: A learns the truth and stays out of the way.
    w.settle();
    assert_eq!(w.awake(), vec!["bbbb"]);
    assert_eq!(w.node("aaaa").presence.owner(), "bbbb");
    assert_eq!(w.node("aaaa").presence.epoch(), 2);
}

#[test]
fn a_hop_that_never_arrives_brings_her_home_when_the_link_returns() {
    let _dir = isolate();
    let mut w = World::new(&["aaaa", "bbbb"]);
    w.net.cut("aaaa", "bbbb");
    w.hop("aaaa", "bbbb", carry("hopeful"));

    for _ in 0..5 {
        w.advance(1000);
        w.deliver();
    }
    // Nobody has her: the safe half of the trade, and it is temporary.
    assert!(w.awake().is_empty());
    assert!(w.node("aaaa").presence.is_stranded());

    w.settle();
    assert_eq!(w.awake(), vec!["aaaa"], "the peer confirms it never took her");
    assert_eq!(w.node("aaaa").carry.mood, "hopeful", "and she is herself again");
}

#[test]
fn the_operator_can_always_take_her_back() {
    let _dir = isolate();
    let mut w = World::new(&["aaaa", "bbbb"]);
    // The other machine is off — not merely unreachable, gone.
    w.net.cut("aaaa", "bbbb");
    w.net.cut("bbbb", "aaaa");
    w.hop("aaaa", "bbbb", carry("stubborn"));
    for _ in 0..5 {
        w.advance(1000);
        w.deliver();
    }
    assert!(w.awake().is_empty());

    let i = w.idx("aaaa");
    let effects = w.nodes[i].presence.force_reclaim(w.now);
    w.apply(i, effects);
    assert_eq!(w.awake(), vec!["aaaa"]);
    assert_eq!(w.node("aaaa").carry.mood, "stubborn", "the carry was not thrown away");
    assert_eq!(w.node("aaaa").presence.epoch(), 3, "a reclaim beats anything in flight");
}

#[test]
fn simultaneous_hops_settle_on_one_machine_whichever_order_they_land_in() {
    for reversed in [false, true] {
        let _dir = isolate();
        let mut w = World::new(&["aaaa", "bbbb"]);
        // Split brain first: both machines believe they have her at epoch 1.
        let i = w.idx("bbbb");
        w.nodes[i].presence = Presence::here("bbbb")
            .with_config(HopConfig { retry_ms: 1000, max_attempts: 3 });
        w.nodes[i].awake = false; // ...but only one is actually showing her
        w.check();

        // …and both decide to hop to the other at the same instant. Neither
        // hop is delivered until both are in flight, which is what "at the
        // same instant" means on a network.
        if reversed {
            w.hop("bbbb", "aaaa", carry("from-b"));
            w.hop("aaaa", "bbbb", carry("from-a"));
        } else {
            w.hop("aaaa", "bbbb", carry("from-a"));
            w.hop("bbbb", "aaaa", carry("from-b"));
        }

        w.deliver();
        w.settle();
        let awake = w.awake();
        assert_eq!(awake.len(), 1, "exactly one machine, order {reversed}");
        // The ordinal tie-break: the greater id's hop wins, so she lands on the
        // lesser id. Both machines reach that conclusion independently.
        assert_eq!(awake[0], "aaaa");
        assert_eq!(w.node("aaaa").carry.mood, "from-b");
        assert_eq!(w.node("bbbb").presence.owner(), "aaaa");
    }
}

#[test]
fn a_stale_hop_from_a_machine_that_missed_an_epoch_is_refused() {
    let _dir = isolate();
    let mut w = World::new(&["aaaa", "bbbb"]);
    w.hop("aaaa", "bbbb", carry("first"));
    w.deliver();
    assert_eq!(w.awake(), vec!["bbbb"]);

    // A machine that still thinks it is 2024 tries to send her somewhere.
    let stale = HopMessage::Hop {
        hop_id: "stale-one".into(),
        from: "cccc".into(),
        to: "bbbb".into(),
        seq: 1,
        epoch: 2,
        attempt: 1,
        carry: carry("stale"),
    };
    let i = w.idx("bbbb");
    let effects = w.nodes[i].presence.on_message(&stale, w.now);
    w.apply(i, effects);
    assert_eq!(w.awake(), vec!["bbbb"]);
    assert_eq!(w.node("bbbb").carry.mood, "first", "a stale hop cannot overwrite her");
}

#[test]
fn split_brain_after_a_partition_is_resolved_by_a_claim() {
    let _dir = isolate();
    let mut w = World::new(&["aaaa", "bbbb"]);
    // Force the pathological state: both awake at the same epoch. This cannot
    // arise from the protocol — it is what a restored backup or a cloned VM
    // would produce — and it must still converge.
    w.fork_allowed = true;
    let i = w.idx("bbbb");
    w.nodes[i].presence = Presence::here("bbbb");
    w.nodes[i].awake = true;
    w.nodes[i].wakes += 1;

    let awake_before = w.nodes.iter().filter(|n| n.awake).count();
    assert_eq!(awake_before, 2, "the test set up the disease");

    w.settle();
    assert_eq!(w.awake().len(), 1, "the claim exchange cures it");
    assert_eq!(w.awake(), vec!["bbbb"], "the greater id keeps her");
}

/// A deterministic storm: hops, drops and partitions in a fixed pseudo-random
/// order, with the invariant checked after every message. If ownership can ever
/// fork, this finds it.
#[test]
fn the_invariant_survives_a_lossy_storm() {
    let _dir = isolate();
    let mut rng = 0x2026_0822_u64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    for round in 0..40 {
        let mut w = World::new(&["aaaa", "bbbb"]);
        for step in 0..24 {
            match next() % 5 {
                0 => {
                    // Whoever has her tries to send her over.
                    let holder = w.nodes.iter().position(|n| n.presence.is_active());
                    if let Some(i) = holder {
                        let to = if w.nodes[i].presence.me() == "aaaa" { "bbbb" } else { "aaaa" };
                        let c = carry(&format!("r{round}s{step}"));
                        if let Ok(effects) = w.nodes[i].presence.begin_hop(to, c, w.now) {
                            w.apply(i, effects);
                        }
                    }
                }
                1 => w.net.drop_next((next() % 3) as usize),
                2 => {
                    w.deliver();
                }
                3 => w.advance(1000),
                _ => {
                    if next() % 2 == 0 {
                        w.net.cut("aaaa", "bbbb");
                    } else {
                        w.net.cut("bbbb", "aaaa");
                    }
                }
            }
            w.check();
        }
        w.settle();
        assert_eq!(
            w.awake().len(),
            1,
            "round {round} ended with {:?}\ntrace:\n  {}",
            w.awake(),
            w.log.join("\n  ")
        );
    }
}

/// The exact scenario the storm above found, pinned down as its own test: the
/// hop is *not* lost, merely slow, and lands after its sender has already given
/// up. Epoch alone says it is valid. It must still never wake her a second time.
#[test]
fn a_hop_that_arrives_after_its_sender_gave_up_cannot_wake_her_again() {
    let _dir = isolate();
    let mut w = World::new(&["aaaa", "bbbb"]);
    w.hop("aaaa", "bbbb", carry("in-transit"));

    // Model a link that delays `hop` frames specifically: they are neither lost
    // nor delivered, they are simply *late*. Everything else gets through.
    let mut stragglers: Vec<HopMessage> = Vec::new();
    let pump = |w: &mut World, stragglers: &mut Vec<HopMessage>| {
        for id in ["aaaa", "bbbb"] {
            let i = w.idx(id);
            for msg in w.net.take(id) {
                if matches!(msg, HopMessage::Hop { .. }) {
                    stragglers.push(msg);
                    continue;
                }
                let effects = w.nodes[i].presence.on_message(&msg, w.now);
                w.apply(i, effects);
            }
        }
    };

    let mut was_stranded = false;
    for _ in 0..5 {
        w.advance(1000);
        if w.node("aaaa").presence.is_stranded() {
            was_stranded = true;
            assert!(w.awake().is_empty(), "while stranded, nobody has her");
        }
        // The fence goes out with the stall, and B — which has still not seen
        // any of those hops — writes them off before answering.
        pump(&mut w, &mut stragglers);
    }
    assert!(was_stranded, "A gave up on the hop");
    assert!(!stragglers.is_empty(), "…while her hop is still out there");
    assert_eq!(w.awake(), vec!["aaaa"], "the fence brought her home by itself");
    assert_eq!(w.node("aaaa").carry.mood, "in-transit", "with what she left with");

    // Now the late hops finally land. Every one of them is answered from the
    // record. This is the assertion the whole design exists for.
    let b = w.idx("bbbb");
    for msg in stragglers {
        let effects = w.nodes[b].presence.on_message(&msg, w.now);
        w.apply(b, effects);
    }
    assert_eq!(w.awake(), vec!["aaaa"]);
    assert_eq!(w.node("bbbb").wakes, 0, "the straggler never woke her");

    w.settle();
    assert_eq!(w.awake(), vec!["aaaa"]);
}

/// Three machines, because a two-node protocol that only works with two nodes
/// is a coincidence.
#[test]
fn the_invariant_holds_across_three_machines() {
    let _dir = isolate();
    let mut rng = 0x1234_5678_9abcu64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let ids = ["aaaa", "bbbb", "cccc"];

    for round in 0..30 {
        let mut w = World::new(&ids);
        for _ in 0..30 {
            match next() % 5 {
                0 => {
                    if let Some(i) = w.nodes.iter().position(|n| n.presence.is_active()) {
                        let to = ids[(next() % 3) as usize];
                        let c = carry("travelling");
                        if let Ok(effects) = w.nodes[i].presence.begin_hop(to, c, w.now) {
                            w.apply(i, effects);
                        }
                    }
                }
                1 => w.net.drop_next((next() % 3) as usize),
                2 => {
                    w.deliver();
                }
                3 => w.advance(1000),
                _ => {
                    let a = ids[(next() % 3) as usize];
                    let b = ids[(next() % 3) as usize];
                    if a != b {
                        w.net.cut(a, b);
                    }
                }
            }
            w.check();
        }
        w.settle();
        assert!(
            w.awake().len() <= 1,
            "round {round} ended with {:?}\ntrace:\n  {}",
            w.awake(),
            w.log.join("\n  ")
        );
    }
}

/// The driveable shell: two [`Hopper`]s, a real (mock) transport, and state on
/// disk. This is the shape the binary will use.
#[test]
fn hoppers_carry_her_across_and_remember_it_across_a_restart() {
    let dir = isolate();
    let net = MockNet::new();
    let cfg = HopConfig::default();
    let a_path = dir.path().join("a/presence.json");
    let b_path = dir.path().join("b/presence.json");

    let mut a = Hopper::new(
        Presence::here("aaaa").with_config(cfg),
        net.transport("aaaa"),
        Some(a_path.clone()),
    );
    let mut b = Hopper::new(
        Presence::elsewhere("bbbb", "aaaa", 1).with_config(cfg),
        net.transport("bbbb"),
        Some(b_path.clone()),
    );
    assert!(a.is_active() && !b.is_active());

    let local = a.hop_to("bbbb", carry("elsewhere-bound"), 0).unwrap();
    assert!(local.iter().any(|e| matches!(e, Effect::Sleep(SleepReason::HoppedAway))));
    assert!(!a.is_active());

    for msg in net.take("bbbb") {
        let local = b.on_message(&msg, 0);
        assert!(local.iter().any(|e| matches!(e, Effect::Wake(c) if c.mood == "elsewhere-bound")));
    }
    for msg in net.take("aaaa") {
        a.on_message(&msg, 0);
    }
    assert!(b.is_active() && !a.is_active());

    // Both machines reboot. Neither wakes up believing something the other
    // does not: the epoch and the owner came off disk.
    let a = Hopper::load_or_here("aaaa", net.transport("aaaa"), a_path, cfg);
    let b = Hopper::load_or_here("bbbb", net.transport("bbbb"), b_path, cfg);
    assert!(!a.is_active(), "she did not come back from the dead on the old machine");
    assert!(b.is_active(), "she is where she was");
    assert_eq!(a.presence().owner(), "bbbb");
    assert_eq!(b.presence().epoch(), 2);
}
