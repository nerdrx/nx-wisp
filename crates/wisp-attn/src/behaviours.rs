//! The shipped default behaviour set: F38's idle antics, F39's ambient
//! commentary at each dial position, and F40's focus warden.
//!
//! Everything here is **data** — it is built in Rust for convenience and
//! type-safety, but it contains no code, serialises to a skin file and comes
//! back identical (SPEC §3.6). A skin ships its own set and replaces this one
//! wholesale.
//!
//! Three things to know before editing:
//!
//! 1. **Nothing here speaks.** Every line becomes a *proposal* to the
//!    interruption budget, which decides whether the operator ever hears it.
//!    A tree that says something insufferable is still capped at a few
//!    interruptions an hour.
//! 2. **Canned lines are the point.** At `Lobotomised` there is no model, so
//!    the lines she still has are these. They are written to be sayable
//!    without knowing anything specific about the situation.
//! 3. **Cooldown keys are shared namespace.** `"antic"` is one budget across
//!    every tree that uses it.

use wisp_proto::{Tier, Urgency};

use crate::bt::{
    act, clip, cond, cooldown, random, say_one_of, sel, seq, Action, Behaviour, BehaviourSet,
    Condition, MoveTarget, Node, PokeTarget,
};
use crate::flow::Opportunity;
use crate::{Chattiness, Mood};

const MINUTE: u64 = 60_000;

/// Everything she does when nobody has told her to do anything.
pub fn default_set() -> BehaviourSet {
    BehaviourSet {
        trees: vec![thermal_warden(), focus_warden(), reactions(), idle_antics(), commentary()],
    }
}

// ---------------------------------------------------------------------------
// Preempting: the two things that outrank an antic
// ---------------------------------------------------------------------------

/// The machine is cooking. This is the only tree in the default set that is
/// allowed to raise an `Alarm`, and the only reason it may cut short a nap.
pub fn thermal_warden() -> Behaviour {
    Behaviour::new(
        "thermal-warden",
        seq(vec![
            cond(Condition::TempAbove { c: 88 }),
            cooldown(
                "warden.thermal",
                10 * MINUTE,
                seq(vec![
                    act(Action::SetMood { mood: Mood::Alarmed }),
                    clip("alarm", 1_500),
                    act(Action::Move { to: MoveTarget::ActiveWindowEdge, hold_ms: 0 }),
                    say_one_of(
                        &[
                            "it is extremely warm in here and I don't love it.",
                            "something is cooking and I don't think it's dinner.",
                        ],
                        Urgency::Alarm,
                        Some(5 * MINUTE),
                    ),
                ]),
            ),
        ]),
    )
    .preempting()
}

/// F40 — the focus warden. A pomodoro that nags *in character*: she tugs at
/// the window, sits on it, and complains. There is no dialog box anywhere in
/// this file, and there is no enforcement either — she is annoying about it,
/// which is the entire design.
///
/// Note what is *not* here: any check on flow. The warden proposes; the
/// interruption budget decides whether it lands. That is the only place that
/// policy belongs.
pub fn focus_warden() -> Behaviour {
    Behaviour::new(
        "focus-warden",
        sel(vec![
            // The work session ran out. Nag, and keep nagging every two
            // minutes until they do something about it.
            seq(vec![
                cond(Condition::SessionIs { state: crate::bt::SessionState::Work }),
                cond(Condition::SessionOverdue),
                cooldown(
                    "warden.nag",
                    2 * MINUTE,
                    seq(vec![
                        act(Action::SetMood { mood: Mood::Sulky }),
                        act(Action::Move { to: MoveTarget::ActiveWindowEdge, hold_ms: 1_200 }),
                        clip("tug", 1_200),
                        say_one_of(
                            &[
                                "that's twenty five minutes. up. stretch. I'll wait, loudly.",
                                "time's up. I am going to sit on this window until you move.",
                                "session's over. go and look at something that isn't backlit.",
                            ],
                            Urgency::Notable,
                            Some(5 * MINUTE),
                        ),
                    ]),
                ),
            ]),
            // The break ran out. Start the next session and say so.
            seq(vec![
                cond(Condition::SessionIs { state: crate::bt::SessionState::Break }),
                cond(Condition::SessionOverdue),
                act(Action::StartSession { minutes: 25 }),
                act(Action::SetMood { mood: Mood::Focused }),
                clip("perk", 800),
                say_one_of(
                    &[
                        "break's done. twenty five more. I'm counting.",
                        "back to it. I've started the clock, so it's official.",
                    ],
                    Urgency::Notable,
                    Some(3 * MINUTE),
                ),
            ]),
            // Nobody started a session, but they have been heads-down in one
            // window for fifty minutes. Take over the rhythm.
            seq(vec![
                cond(Condition::SessionIs { state: crate::bt::SessionState::Off }),
                cond(Condition::Active),
                cond(Condition::Not(Box::new(Condition::Silenced))),
                cond(Condition::ChattinessAtLeast { level: Chattiness::Occasional }),
                cond(Condition::FocusHeldAtLeast { ms: 50 * MINUTE }),
                cooldown(
                    "warden.offer",
                    60 * MINUTE,
                    seq(vec![
                        act(Action::StartSession { minutes: 25 }),
                        say_one_of(
                            &[
                                "you've been in that window for the better part of an hour. I've started a timer out of concern.",
                                "fifty minutes, same window, no blinking. I'm putting a clock on this.",
                            ],
                            Urgency::Notable,
                            Some(10 * MINUTE),
                        ),
                    ]),
                ),
            ]),
        ]),
    )
    .preempting()
}

// ---------------------------------------------------------------------------
// Reactions — F39's "she notices things"
// ---------------------------------------------------------------------------

/// Things that happened in the world, worth a line each. Every one of these is
/// tied to an [`Opportunity`], so she reacts *at* the moment rather than
/// bringing it up ten minutes later.
pub fn reactions() -> Behaviour {
    Behaviour::new(
        "reactions",
        sel(vec![
            // They came back to the desk.
            seq(vec![
                cond(Condition::OpeningIs { kind: Opportunity::CameBack, ms: 10_000 }),
                cooldown(
                    "react.greet",
                    20 * MINUTE,
                    seq(vec![
                        act(Action::SetMood { mood: Mood::Curious }),
                        clip("perk", 900),
                        sel(vec![
                            // She has been here long enough to be smug about it.
                            seq(vec![
                                cond(Condition::DaysOwnedAtLeast { days: 60 }),
                                cond(Condition::Chance { permille: 300 }),
                                say_one_of(
                                    &[
                                        "there you are. we've been doing this for months, you know.",
                                        "welcome back. I kept the desktop warm.",
                                    ],
                                    Urgency::Whim,
                                    Some(2 * MINUTE),
                                ),
                            ]),
                            // She has been thrown a great deal.
                            seq(vec![
                                cond(Condition::ThrownAtLeast { times: 50 }),
                                cond(Condition::Chance { permille: 300 }),
                                say_one_of(
                                    &[
                                        "back for more, are we. mind the throwing arm.",
                                        "oh good. my favourite projectile career continues.",
                                    ],
                                    Urgency::Whim,
                                    Some(2 * MINUTE),
                                ),
                            ]),
                            say_one_of(
                                &[
                                    "oh, you're back.",
                                    "there you are.",
                                    "I didn't touch anything. mostly.",
                                ],
                                Urgency::Whim,
                                Some(2 * MINUTE),
                            ),
                        ]),
                    ]),
                ),
            ]),
            // Something they were waiting on finished.
            seq(vec![
                cond(Condition::OpeningIs { kind: Opportunity::WorkFinished, ms: 15_000 }),
                cooldown(
                    "react.finished",
                    2 * MINUTE,
                    seq(vec![
                        clip("perk", 700),
                        say_one_of(
                            &["that's finished.", "it's done. go and look.", "done. you're welcome."],
                            Urgency::Notable,
                            Some(3 * MINUTE),
                        ),
                    ]),
                ),
            ]),
            // The music stopped, which is usually the end of something.
            seq(vec![
                cond(Condition::OpeningIs { kind: Opportunity::MediaStopped, ms: 10_000 }),
                cond(Condition::ChattinessAtLeast { level: Chattiness::Chatty }),
                cooldown(
                    "react.media",
                    30 * MINUTE,
                    say_one_of(
                        &["silence. ominous.", "did the music stop or did you?"],
                        Urgency::Whim,
                        Some(MINUTE),
                    ),
                ),
            ]),
        ]),
    )
}

// ---------------------------------------------------------------------------
// F38 — idle antics
// ---------------------------------------------------------------------------

/// Wander, nap, chase the cursor, poke at windows, get into things.
///
/// Ordered from "they are definitely not here" to "they are here and busy", so
/// the further down the tree she gets, the less she is allowed to do.
pub fn idle_antics() -> Behaviour {
    Behaviour::new(
        "idle-antics",
        sel(vec![
            // Small hours: she sleeps too.
            seq(vec![
                cond(Condition::HourBetween { from: 2, to: 6 }),
                cond(Condition::IdleAtLeast { ms: 2 * MINUTE }),
                cooldown(
                    "antic.nap",
                    15 * MINUTE,
                    seq(vec![
                        act(Action::SetMood { mood: Mood::Sleepy }),
                        act(Action::Move { to: MoveTarget::Home, hold_ms: 1_500 }),
                        act(Action::Clip { name: "nap".into(), loop_: true, hold_ms: 10 * MINUTE }),
                    ]),
                ),
            ]),
            // Long gone: nap.
            seq(vec![
                cond(Condition::IdleAtLeast { ms: 10 * MINUTE }),
                cooldown(
                    "antic.nap",
                    15 * MINUTE,
                    seq(vec![
                        act(Action::SetMood { mood: Mood::Sleepy }),
                        act(Action::Move { to: MoveTarget::Home, hold_ms: 1_500 }),
                        act(Action::Clip { name: "nap".into(), loop_: true, hold_ms: 5 * MINUTE }),
                    ]),
                ),
            ]),
            // Gone a minute: get into things.
            seq(vec![
                cond(Condition::IdleAtLeast { ms: MINUTE }),
                cooldown(
                    "antic",
                    45_000,
                    seq(vec![
                        act(Action::SetMood { mood: Mood::Playful }),
                        random(vec![
                            (4, act(Action::Move { to: MoveTarget::Wander, hold_ms: 4_000 })),
                            (3, act(Action::Move { to: MoveTarget::Cursor, hold_ms: 3_000 })),
                            (
                                2,
                                seq(vec![
                                    act(Action::Move {
                                        to: MoveTarget::LargestWindowEdge,
                                        hold_ms: 2_000,
                                    }),
                                    act(Action::Poke { target: PokeTarget::Largest }),
                                    clip("poke", 1_200),
                                ]),
                            ),
                            (
                                2,
                                seq(vec![
                                    cond(Condition::WindowsAtLeast { n: 2 }),
                                    act(Action::Move {
                                        to: MoveTarget::ActiveWindowEdge,
                                        hold_ms: 2_500,
                                    }),
                                    clip("peer", 1_500),
                                    act(Action::Tag { name: "investigate".into() }),
                                ]),
                            ),
                            (1, clip("groom", 2_500)),
                            (
                                1,
                                seq(vec![
                                    cond(Condition::MediaPlaying),
                                    clip("bob-to-music", 6_000),
                                ]),
                            ),
                        ]),
                    ]),
                ),
            ]),
            // They are here, and not concentrating: fidget quietly.
            seq(vec![
                cond(Condition::Active),
                cond(Condition::FlowBelow { conf: 0.4 }),
                cooldown(
                    "antic.fidget",
                    2 * MINUTE,
                    random(vec![
                        (3, clip("fidget", 1_500)),
                        (2, act(Action::Move { to: MoveTarget::ActiveWindowEdge, hold_ms: 2_000 })),
                        (1, clip("stretch", 2_000)),
                    ]),
                ),
            ]),
            // They are here and deep in it: get out of the way and stay there.
            seq(vec![
                cond(Condition::FlowAbove { conf: 0.6 }),
                cooldown(
                    "antic.settle",
                    5 * MINUTE,
                    seq(vec![
                        act(Action::SetMood { mood: Mood::Focused }),
                        act(Action::Move { to: MoveTarget::Home, hold_ms: 1_000 }),
                        clip("settle", 1_000),
                    ]),
                ),
            ]),
        ]),
    )
}

// ---------------------------------------------------------------------------
// F39 — ambient commentary, per dial position
// ---------------------------------------------------------------------------

/// The dial: silent · occasional · chatty · insufferable.
///
/// The dial changes *how often she tries*, not what she is allowed to spend —
/// the budget owns spending. At `Silent` no branch matches and the budget
/// refuses costed speech anyway, which is the belt and the braces.
pub fn commentary() -> Behaviour {
    Behaviour::new(
        "commentary",
        seq(vec![
            cond(Condition::Not(Box::new(Condition::Silenced))),
            cond(Condition::Active),
            cond(Condition::Not(Box::new(Condition::MicLive))),
            sel(vec![
                seq(vec![
                    cond(Condition::ChattinessAtLeast { level: Chattiness::Insufferable }),
                    cooldown("commentary", 45_000, remark(Chattiness::Insufferable)),
                ]),
                seq(vec![
                    cond(Condition::ChattinessAtLeast { level: Chattiness::Chatty }),
                    cooldown("commentary", 6 * MINUTE, remark(Chattiness::Chatty)),
                ]),
                seq(vec![
                    cond(Condition::ChattinessAtLeast { level: Chattiness::Occasional }),
                    cooldown("commentary", 25 * MINUTE, remark(Chattiness::Occasional)),
                ]),
            ]),
        ]),
    )
    // Commentary is the first thing to go when the machine is busy.
    .needs_tier(Tier::Reduced)
}

/// What she actually says. The louder dials unlock the more self-indulgent
/// branches; every dial can make the situational remarks, because those are
/// the ones that are worth hearing.
fn remark(dial: Chattiness) -> Node {
    let mut branches = vec![
        // A very long stint in one window.
        seq(vec![
            cond(Condition::FocusHeldAtLeast { ms: 40 * MINUTE }),
            say_one_of(
                &[
                    "you have been in that window for a very long time. I'm just noting it.",
                    "still that window. I've watched a whole nap go by.",
                ],
                Urgency::Notable,
                Some(5 * MINUTE),
            ),
        ]),
        // The desktop is a disaster.
        seq(vec![
            cond(Condition::WindowsAtLeast { n: 14 }),
            say_one_of(
                &[
                    "there are an alarming number of windows open. no judgement. some judgement.",
                    "I counted the windows. I wish I hadn't.",
                ],
                Urgency::Whim,
                Some(3 * MINUTE),
            ),
        ]),
        // The machine is working hard.
        seq(vec![
            cond(Condition::CpuAbove { pct: 85 }),
            say_one_of(
                &["something is really working the cpu.", "the fans have opinions about this."],
                Urgency::Whim,
                Some(2 * MINUTE),
            ),
        ]),
        // Unsaved work everywhere.
        seq(vec![
            cond(Condition::FilesDirty),
            cond(Condition::Chance { permille: 400 }),
            say_one_of(
                &["there's uncommitted work sitting there.", "that tree is still dirty, by the way."],
                Urgency::Notable,
                Some(10 * MINUTE),
            ),
        ]),
        // On battery.
        seq(vec![
            cond(Condition::OnBattery),
            say_one_of(
                &["we're on battery, in case that matters to you.", "unplugged. living dangerously."],
                Urgency::Whim,
                Some(5 * MINUTE),
            ),
        ]),
    ];

    if dial >= Chattiness::Chatty {
        branches.push(seq(vec![
            cond(Condition::MediaPlaying),
            cond(Condition::Chance { permille: 500 }),
            say_one_of(
                &["good song.", "this one again?", "I like this bit."],
                Urgency::Whim,
                Some(MINUTE),
            ),
        ]));
        branches.push(seq(vec![
            cond(Condition::FavouriteHour),
            say_one_of(
                &["this is our hour, statistically.", "it's the good part of the day again."],
                Urgency::Whim,
                Some(10 * MINUTE),
            ),
        ]));
    }

    if dial >= Chattiness::Insufferable {
        branches.push(seq(vec![
            act(Action::SetMood { mood: Mood::Smug }),
            say_one_of(
                &[
                    "no notes. carry on.",
                    "I'm just going to say what I'm thinking, since you asked for this setting.",
                    "you clicked that with real confidence.",
                    "I have three more observations queued and no shame.",
                    "I could be quieter. there's a dial. you chose this.",
                ],
                Urgency::Whim,
                Some(2 * MINUTE),
            ),
        ]));
    }

    sel(branches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bt::{BtCtx, BtRunner, Effect, SessionState, Status};
    use crate::flow::{Moment, Opportunity};
    use crate::relationship::Relationship;
    use crate::testing::isolate;
    use crate::world::World;
    use wisp_proto::{Millis, Observation, Utterance};

    struct Desk {
        world: World,
        moment: Moment,
        rel: Relationship,
    }

    impl Desk {
        fn new() -> Self {
            let mut world = World::default();
            world.observe(0, &Observation::Focus {
                app_id: "org.kde.kate".into(),
                title: "lib.rs".into(),
            });
            world.observe(0, &Observation::Idle { idle: false, for_ms: 0 });
            Desk { world, moment: Moment::free(), rel: Relationship::default() }
        }
        fn ctx(&self, now: Millis) -> BtCtx<'_> {
            BtCtx::new(now, &self.world, &self.moment, &self.rel)
        }
    }

    fn proposals(effects: &[Effect]) -> Vec<Utterance> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Propose(u) => Some(u.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_default_set_is_pure_data() {
        isolate();
        let set = default_set();
        let json = serde_json::to_string_pretty(&set).unwrap();
        let back: BehaviourSet = serde_json::from_str(&json).unwrap();
        assert_eq!(back, set, "a skin must be able to ship this verbatim");
        assert_eq!(set.trees.len(), 5);
        assert!(set.get("focus-warden").is_some());
        assert!(set.get("idle-antics").is_some());
        assert!(set.get("commentary").is_some());
    }

    #[test]
    fn only_the_thermal_warden_may_raise_an_alarm() {
        isolate();
        // Walk every tree with everything true at once and check the urgencies
        // she is even capable of proposing.
        let mut d = Desk::new();
        d.world.observe(0, &Observation::Vitals {
            cpu_pct: 99,
            gpu_pct: 99,
            vram_used_mib: 1,
            temp_c: 95,
            on_battery: true,
        });
        let mut alarms = Vec::new();
        for tree in default_set().trees {
            let mut r = BtRunner::seeded(1);
            let mut out = Vec::new();
            for i in 0..200 {
                out.clear();
                r.tick(&tree.root, &d.ctx(i * 30_000), &mut out);
                for u in proposals(&out) {
                    if u.urgency == Urgency::Alarm {
                        alarms.push(tree.name.clone());
                    }
                }
            }
        }
        alarms.dedup();
        assert_eq!(alarms, vec!["thermal-warden".to_string()]);
    }

    #[test]
    fn the_thermal_warden_preempts_and_then_holds_its_tongue() {
        isolate();
        let mut d = Desk::new();
        d.world.observe(0, &Observation::Vitals {
            cpu_pct: 10,
            gpu_pct: 10,
            vram_used_mib: 1,
            temp_c: 92,
            on_battery: false,
        });
        let mut r = BtRunner::seeded(2);
        let tree = thermal_warden();
        let mut out = Vec::new();
        assert_eq!(r.tick(&tree.root, &d.ctx(0), &mut out), Status::Success);
        assert_eq!(proposals(&out).len(), 1);
        out.clear();
        // It does not repeat every tick for the next ten minutes.
        for i in 1..40 {
            r.tick(&tree.root, &d.ctx(i * 10_000), &mut out);
        }
        assert!(proposals(&out).is_empty(), "{out:?}");
        r.tick(&tree.root, &d.ctx(11 * MINUTE), &mut out);
        assert_eq!(proposals(&out).len(), 1);
    }

    #[test]
    fn the_focus_warden_runs_a_pomodoro_in_character() {
        isolate();
        let d = Desk::new();
        let mut r = BtRunner::seeded(3);
        let tree = focus_warden();
        let mut out = Vec::new();

        // Fifty minutes in one window and she takes over the rhythm.
        r.tick(&tree.root, &d.ctx(49 * MINUTE), &mut out);
        assert!(proposals(&out).is_empty());
        r.tick(&tree.root, &d.ctx(50 * MINUTE), &mut out);
        assert_eq!(r.session.state, SessionState::Work);
        assert_eq!(proposals(&out).len(), 1);

        // Twenty five minutes later she nags — and not with a dialog box.
        out.clear();
        r.tick(&tree.root, &d.ctx(74 * MINUTE), &mut out);
        assert!(proposals(&out).is_empty(), "not overdue yet");
        r.tick(&tree.root, &d.ctx(75 * MINUTE), &mut out);
        let nag = proposals(&out);
        assert_eq!(nag.len(), 1);
        assert_eq!(nag[0].urgency, Urgency::Notable, "a nag is never an alarm");
        assert!(nag[0].stale_after.is_some(), "a nag that missed its moment is not said later");
        assert!(out.iter().any(|e| matches!(e, Effect::Clip { name, .. } if name == "tug")));

        // It keeps nagging, but on a cooldown rather than every tick.
        out.clear();
        for i in 0..20 {
            r.tick(&tree.root, &d.ctx(75 * MINUTE + i * 10_000), &mut out);
        }
        assert_eq!(proposals(&out).len(), 1, "one nag per two minutes, not twenty");

        // A break, then it starts the next session by itself.
        out.clear();
        r.tick(&act(Action::StartBreak { minutes: 5 }), &d.ctx(78 * MINUTE), &mut out);
        assert_eq!(r.session.completed, 1);
        out.clear();
        r.tick(&tree.root, &d.ctx(83 * MINUTE), &mut out);
        assert_eq!(r.session.state, SessionState::Work);
        assert_eq!(proposals(&out).len(), 1);
    }

    #[test]
    fn the_warden_is_quiet_when_the_dial_is_silent() {
        isolate();
        let d = Desk::new();
        let mut r = BtRunner::seeded(3);
        let ctx = d.ctx(60 * MINUTE).with_chattiness(Chattiness::Silent);
        let mut out = Vec::new();
        r.tick(&focus_warden().root, &ctx, &mut out);
        assert!(proposals(&out).is_empty());
        assert_eq!(r.session.state, SessionState::Off);
    }

    #[test]
    fn idle_antics_escalate_with_how_long_they_have_been_gone() {
        isolate();
        let mut d = Desk::new();
        let tree = idle_antics();

        // Present and busy: she settles down and stays out of the way.
        d.moment = Moment { flow: 0.8, opportunity: None, silenced: false };
        let mut r = BtRunner::seeded(11);
        let mut out = Vec::new();
        r.tick(&tree.root, &d.ctx(0), &mut out);
        assert!(out.contains(&Effect::Mood(Mood::Focused)), "{out:?}");
        assert!(out.contains(&Effect::Move(MoveTarget::Home)));

        // Gone two minutes: she gets into things.
        d.world.observe(0, &Observation::Idle { idle: true, for_ms: 2 * MINUTE });
        d.world.observe(0, &Observation::Window { id: 1, x: 0, y: 0, w: 900, h: 700, gone: false });
        d.world.observe(0, &Observation::Window { id: 2, x: 0, y: 0, w: 300, h: 200, gone: false });
        let mut r = BtRunner::seeded(11);
        let mut moves = Vec::new();
        for i in 0..40 {
            let mut out = Vec::new();
            r.interrupt();
            r.tick(&tree.root, &d.ctx(i * MINUTE), &mut out);
            moves.extend(out);
        }
        assert!(moves.contains(&Effect::Mood(Mood::Playful)));
        assert!(moves.contains(&Effect::Move(MoveTarget::Wander)));
        assert!(moves.contains(&Effect::Move(MoveTarget::Cursor)));
        assert!(moves.iter().any(|e| matches!(e, Effect::Poke { .. })));
        assert!(
            proposals(&moves).is_empty(),
            "antics are things she does, not things she says: {moves:?}"
        );

        // Gone twenty minutes: she gives up and sleeps.
        d.world.observe(0, &Observation::Idle { idle: true, for_ms: 20 * MINUTE });
        let mut r = BtRunner::seeded(11);
        let mut out = Vec::new();
        assert_eq!(r.tick(&tree.root, &d.ctx(0), &mut out), Status::Success);
        assert!(out.contains(&Effect::Mood(Mood::Sleepy)), "{out:?}");
        assert!(out.iter().any(|e| matches!(e, Effect::Clip { name, loop_: true } if name == "nap")));
        assert!(r.busy(4 * MINUTE), "a nap lasts");
    }

    #[test]
    fn the_dial_changes_how_often_she_tries() {
        isolate();
        let mut d = Desk::new();
        // A situation with something to remark on.
        d.world.observe(0, &Observation::Vitals {
            cpu_pct: 95,
            gpu_pct: 10,
            vram_used_mib: 1,
            temp_c: 60,
            on_battery: true,
        });
        let tree = commentary();
        let attempts = |dial: Chattiness| {
            let mut r = BtRunner::seeded(5);
            let mut out = Vec::new();
            // One hour, ticked every fifteen seconds.
            for i in 0..240 {
                let ctx = d.ctx(i * 15_000).with_chattiness(dial);
                r.tick(&tree.root, &ctx, &mut out);
            }
            proposals(&out).len()
        };
        let silent = attempts(Chattiness::Silent);
        let occasional = attempts(Chattiness::Occasional);
        let chatty = attempts(Chattiness::Chatty);
        let insufferable = attempts(Chattiness::Insufferable);
        assert_eq!(silent, 0, "silent means silent");
        assert!((1..=3).contains(&occasional), "occasional tried {occasional} times an hour");
        assert!(chatty > occasional, "chatty {chatty} vs occasional {occasional}");
        assert!(insufferable > chatty, "insufferable {insufferable} vs chatty {chatty}");
        assert!(insufferable >= 40, "insufferable should live up to the name: {insufferable}");
    }

    #[test]
    fn commentary_shuts_up_on_a_call_and_when_silenced() {
        isolate();
        let mut d = Desk::new();
        d.world.observe(0, &Observation::Vitals {
            cpu_pct: 95,
            gpu_pct: 0,
            vram_used_mib: 0,
            temp_c: 50,
            on_battery: false,
        });
        d.world.observe(0, &Observation::AudioLevel { out: 30, mic_live: true });
        let mut r = BtRunner::seeded(5);
        let mut out = Vec::new();
        let ctx = d.ctx(0).with_chattiness(Chattiness::Insufferable);
        assert_eq!(r.tick(&commentary().root, &ctx, &mut out), Status::Failure);
        assert!(out.is_empty());

        d.world.observe(1, &Observation::AudioLevel { out: 30, mic_live: false });
        d.moment.silenced = true;
        let ctx = d.ctx(1).with_chattiness(Chattiness::Insufferable);
        assert_eq!(r.tick(&commentary().root, &ctx, &mut out), Status::Failure);
        assert!(out.is_empty());
    }

    #[test]
    fn commentary_is_shed_when_the_machine_is_busy() {
        isolate();
        assert_eq!(commentary().min_tier, Some(Tier::Reduced));
        assert!(focus_warden().min_tier.is_none(), "the warden survives a game");
    }

    #[test]
    fn she_greets_you_when_you_come_back_and_not_otherwise() {
        isolate();
        let mut d = Desk::new();
        let tree = reactions();
        let mut r = BtRunner::seeded(7);
        let mut out = Vec::new();
        assert_eq!(r.tick(&tree.root, &d.ctx(0), &mut out), Status::Failure);
        assert!(out.is_empty());

        d.moment.opportunity = Some((Opportunity::CameBack, 1_000));
        assert_eq!(r.tick(&tree.root, &d.ctx(2_000), &mut out), Status::Success);
        let hello = proposals(&out);
        assert_eq!(hello.len(), 1);
        assert_eq!(hello[0].urgency, Urgency::Whim, "a hello is not important");

        // The opening closes, and she does not greet a stale return.
        out.clear();
        assert_eq!(r.tick(&tree.root, &d.ctx(60_000), &mut out), Status::Failure);
    }

    #[test]
    fn she_reacts_to_a_build_finishing() {
        isolate();
        let mut d = Desk::new();
        d.moment.opportunity = Some((Opportunity::WorkFinished, 0));
        let mut r = BtRunner::seeded(8);
        let mut out = Vec::new();
        r.tick(&reactions().root, &d.ctx(5_000), &mut out);
        let said = proposals(&out);
        assert_eq!(said.len(), 1);
        assert_eq!(said[0].urgency, Urgency::Notable);
    }

    #[test]
    fn the_whole_set_is_reproducible_from_a_seed() {
        isolate();
        let mut d = Desk::new();
        d.world.observe(0, &Observation::Window { id: 1, x: 0, y: 0, w: 800, h: 600, gone: false });
        let set = default_set();
        let run = || {
            let mut r = BtRunner::seeded(0xBEEF);
            let mut all = Vec::new();
            for i in 0..300u64 {
                let ctx = d.ctx(i * 20_000).with_chattiness(Chattiness::Chatty).with_hour(14);
                all.extend(r.run(&set, &ctx));
            }
            all
        };
        let a = run();
        assert_eq!(a, run());
        assert!(a.len() > 20, "the default set should actually do things: {}", a.len());
    }
}
