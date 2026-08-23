//! The whole crate, end to end, with no GPU and no model.
//!
//! These are the tests that would notice if F12–F19 were individually correct
//! and collectively wrong: SPEC §3.4 (she never speaks, she proposes), §3.5
//! (T3/T4 queues rather than runs), F15 (the persona prefix is prefilled once)
//! and F17 (she says she does not know rather than making something up).

mod common;

use common::{gov, Fixture};
use wisp_mind::backend::Role;
use wisp_mind::escalate::{Ask, ClaudeCli, Ladder};
use wisp_mind::mind::{Mind, Thought, TurnConfig};
use wisp_proto::{Governed, Observation, Tier, TierReason, Urgency};

/// The self-assessment turn runs first and its prompt contains the ask, so this
/// script must be registered before any ask-specific one — the mock matches in
/// order, like a router.
const ASSESS: &str = "Decide whether you can answer";

fn quiet() -> TierReason {
    TierReason::Idle
}

fn mind_with(f: &Fixture, scripts: &[(&str, &str)]) -> Mind {
    let mut b = f.backend().script(ASSESS, "\"answer\"");
    for (needle, reply) in scripts {
        b = b.script(needle, reply);
    }
    let mut m = f
        .mind(b)
        .turn(TurnConfig {
            recall_k: 2,
            ..TurnConfig::default()
        })
        .build()
        .expect("mind");
    let (d, budget) = gov::desktop(Tier::Full, None);
    m.apply_governor(d, budget);
    m.set_tier(Tier::Full, &quiet());
    m
}

#[tokio::test]
async fn she_never_speaks_she_only_proposes() {
    let f = Fixture::new();
    f.place_all();
    let mut m = mind_with(
        &f,
        &[("how many windows", r#"{"say":"Forty-seven. Again."}"#)],
    );

    let t = m
        .think(Ask::from_operator("how many windows do I have open"), 1, 1_000)
        .await
        .expect("think");
    match &t {
        Thought::Proposed(u) => {
            assert_eq!(u.text, "Forty-seven. Again.");
            // The operator asked, so it does not pay the attention budget.
            assert_eq!(u.urgency, Urgency::Answer);
            // And it carries the face she is wearing, for the rig.
            assert_eq!(u.expression.as_deref(), Some(m.mood().expression()));
        }
        other => panic!("expected a proposal, got {other:?}"),
    }

    // SPEC §3.4: it is sitting in the outbox waiting for `wisp-attn`, and
    // nothing has been said.
    let out = m.take_outbox();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].text, "Forty-seven. Again.");
    assert_eq!(m.outbox_len(), 0, "taking it empties it");

    // And the proposal is in the flight recorder as a proposal.
    assert!(f
        .log
        .all()
        .iter()
        .any(|e| matches!(e, wisp_proto::EventKind::Proposed(u) if u.text == "Forty-seven. Again.")));
}

#[tokio::test]
async fn she_calls_a_tool_and_reports_what_it_said() {
    let f = Fixture::new();
    f.place_all();
    let mut m = mind_with(
        &f,
        &[
            // Order matters: the mock matches in order, and hop two's prompt
            // still contains the operator's original words as well as the tool
            // result. The more specific script therefore has to come first, the
            // way a router puts its specific route above its catch-all.
            ("timer_set returned", r#"{"say":"Three minutes, starting now."}"#),
            (
                "set a timer",
                r#"{"name":"timer_set","arguments":{"label":"tea","minutes":3}}"#,
            ),
        ],
    );

    let t = m
        .think(Ask::from_operator("set a timer for tea, three minutes"), 1, 1_000)
        .await
        .expect("think");
    // The tool ran on the first hop and she spoke on the second, so the turn
    // ends as a proposal.
    match &t {
        Thought::Proposed(u) => assert!(u.text.contains("Three minutes")),
        other => panic!("expected a proposal after the tool hop, got {other:?}"),
    }
    assert_eq!(f.log.tool_calls(), vec![("timer_set".to_string(), true)]);
    // The timer really exists.
    assert_eq!(m.builtins().due_timers(f.now() + 4 * 60_000).len(), 1);
}

#[tokio::test]
async fn a_tool_she_may_not_use_is_not_in_the_language_she_can_speak() {
    let f = Fixture::new();
    f.place_all();
    // She tries to call something she has not been given.
    let mut m = mind_with(
        &f,
        &[
            ("media_control returned", r#"{"say":"Paused."}"#),
            (
                "pause the music",
                r#"{"name":"media_control","arguments":{"action":"pause"}}"#,
            ),
        ],
    );
    // `media_control` is Explicit and off, so it is not in the grammar — and the
    // mock refuses to emit something a constrained decoder could not have.
    let err = m
        .think(Ask::from_operator("pause the music"), 1, 1_000)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("constrained decoder"),
        "the grammar should have made that unreachable: {err}"
    );

    // Switch it on and the same reply becomes possible.
    m.tools_mut().enable("media_control", true).expect("enable");
    let t = m
        .think(Ask::from_operator("pause the music"), 2, 2_000)
        .await
        .expect("think");
    match &t {
        Thought::Proposed(u) => assert_eq!(u.text, "Paused."),
        other => panic!("expected the tool to run and then a reply, got {other:?}"),
    }
    assert_eq!(f.log.tool_calls().last().map(|(n, ok)| (n.as_str(), *ok)), Some(("media_control", false)),
        "there is no player here, so the call ran and honestly failed");
}

#[tokio::test]
async fn at_t3_a_question_is_queued_and_replayed_when_she_comes_back() {
    let f = Fixture::new();
    f.place_all();
    let mut m = mind_with(&f, &[("what broke", r#"{"say":"The build did."}"#)]);

    // A headset starts.
    let (d, b) = gov::desktop(Tier::Lobotomised, None);
    m.apply_governor(d, b);
    m.set_tier(Tier::Lobotomised, &TierReason::VrSession);

    let t = m
        .think(Ask::from_operator("what broke"), 1, 10_000)
        .await
        .expect("think");
    match t {
        Thought::Deferred { depth, .. } => assert_eq!(depth, 1),
        other => panic!("SPEC §3.5: T3 queues, it does not run. Got {other:?}"),
    }
    assert!(m.take_outbox().is_empty(), "she said nothing at T3");
    assert_eq!(f.log.deferred().len(), 1);

    // Nothing replays while she is still down there.
    assert!(m.replay_deferred(20_000).is_empty());

    // The headset goes away.
    let (d, b) = gov::desktop(Tier::Full, None);
    m.apply_governor(d, b);
    m.set_tier(Tier::Full, &quiet());
    let replay = m.replay_deferred(30_000);
    assert_eq!(replay.ready.len(), 1);
    assert_eq!(replay.ready[0].what, "what broke");
    assert!(replay.dropped.is_empty());
    assert_eq!(f.log.replayed(), vec![("what broke".to_string(), false)]);
}

#[tokio::test]
async fn a_thought_that_went_stale_while_she_was_down_is_dropped_and_recorded() {
    let f = Fixture::new();
    f.place_all();
    let mut m = mind_with(&f, &[("anything", r#"{"say":"ok"}"#)]);
    let (d, b) = gov::desktop(Tier::Lobotomised, None);
    m.apply_governor(d, b);
    m.set_tier(Tier::Lobotomised, &TierReason::VrSession);

    // Her own idle remark, not the operator's question.
    m.think(Ask::her_own("the shader cache is rebuilding"), 1, 0)
        .await
        .expect("defer");

    // Twenty minutes of Elite Dangerous later, nobody cares about the shader
    // cache.
    let (d, b) = gov::desktop(Tier::Full, None);
    m.apply_governor(d, b);
    m.set_tier(Tier::Full, &quiet());
    let replay = m.replay_deferred(20 * 60_000);
    assert!(replay.ready.is_empty());
    assert_eq!(replay.dropped.len(), 1);
    assert_eq!(
        f.log.replayed(),
        vec![("the shader cache is rebuilding".to_string(), true)],
        "a drop is recorded as a drop, never silently resurrected"
    );
    // And it does not come back on the next replay either.
    assert!(m.replay_deferred(21 * 60_000).is_empty());
}

#[tokio::test]
async fn being_silenced_throws_away_both_the_queue_and_the_outbox() {
    let f = Fixture::new();
    f.place_all();
    let mut m = mind_with(&f, &[("hello", r#"{"say":"Hello."}"#)]);
    m.think(Ask::from_operator("hello"), 1, 1_000)
        .await
        .expect("think");
    assert_eq!(m.outbox_len(), 1);

    m.set_tier(Tier::Dormant, &TierReason::Pinned);
    assert_eq!(m.outbox_len(), 0, "T4 is not 'later', it is 'no'");
    assert!(m.replay_deferred(2_000).is_empty());
    for role in Role::ALL {
        assert_eq!(
            m.manager().residency(role),
            wisp_mind::backend::Residency::Cold
        );
    }
}

#[tokio::test]
async fn the_persona_prefix_is_prefilled_once_and_never_again() {
    let f = Fixture::new();
    f.place_all();
    let mut m = mind_with(
        &f,
        &[
            ("first question", r#"{"say":"one"}"#),
            ("second question", r#"{"say":"two"}"#),
            ("third question", r#"{"say":"three"}"#),
        ],
    );

    m.think(Ask::from_operator("first question"), 7, 1_000)
        .await
        .expect("think");
    let after_one = m.kv().stats().clone();
    assert_eq!(
        after_one.persona_seeds, 1,
        "the very first turn seeds from the persona slot"
    );
    assert!(after_one.reused_tokens > 0);

    m.think(Ask::from_operator("second question"), 7, 2_000)
        .await
        .expect("think");
    m.think(Ask::from_operator("third question"), 7, 3_000)
        .await
        .expect("think");

    let s = m.kv().stats();
    assert!(
        s.hit_rate() > 0.6,
        "F15: most of every prompt should come out of the cache, got {} ({} reused / {} prefilled)",
        s.hit_rate(),
        s.reused_tokens,
        s.prefilled_tokens
    );
    // And the prefix never moved, so it was never invalidated.
    assert!(!m.kv().persona_tokens().is_empty());
}

#[tokio::test]
async fn a_changing_mood_does_not_cost_the_cache() {
    let f = Fixture::new();
    f.place_all();
    let mut m = mind_with(&f, &[("anything at all", r#"{"say":"ok"}"#)]);
    m.think(Ask::from_operator("anything at all"), 1, 1_000)
        .await
        .expect("think");
    let persona = m.kv().persona_tokens().to_vec();

    // Something alarming happens; the mood changes; F19 modulates the prompt.
    m.observe(
        &Observation::Vitals {
            cpu_pct: 30,
            gpu_pct: 99,
            vram_used_mib: 22_000,
            temp_c: 96,
            on_battery: false,
        },
        2_000,
    );
    assert_ne!(
        m.mood(),
        wisp_mind::mood::Mood::Calm,
        "the mood should have moved"
    );

    m.think(Ask::from_operator("anything at all"), 1, 3_000)
        .await
        .expect("think");
    assert_eq!(
        m.kv().persona_tokens(),
        persona.as_slice(),
        "F15 and F19 must not be in conflict: the mood lives after the cached prefix"
    );
}

#[tokio::test]
async fn when_she_is_out_of_her_depth_she_says_so_instead_of_making_something_up() {
    let f = Fixture::new();
    // Only the small model exists — no 30B on disk, and no Claude Code.
    f.place(Role::Reflex);
    // And the small model, asked, admits it is out of its depth.
    let backend = f
        .backend()
        .script(ASSESS, "\"escalate\"")
        .script("prove", r#"{"say":"I have no idea, honestly"}"#);
    let mut m = f
        .mind(backend)
        .ladder(Ladder {
            cli: ClaudeCli::new("/nonexistent/claude"),
            // Even switched on, an absent CLI degrades silently.
            big_brain_enabled: true,
        })
        .build()
        .expect("mind");
    let (d, b) = gov::desktop(Tier::Full, None);
    m.apply_governor(d, b);
    m.set_tier(Tier::Full, &quiet());

    let hard = "prove that this scheduler is starvation-free; explain the root cause \
                step by step. why? how come? ```panicked at``` compare the trade-off";
    let t = m
        .think(Ask::from_operator(hard), 1, 1_000)
        .await
        .expect("think");
    match &t {
        Thought::OutOfDepth(u) => {
            // The message must be honest AND carry its remedy — post-0.7.1
            // wording names the exact command that would add the next rung.
            assert!(
                u.text.contains("models fetch")
                    || u.text.contains("model.big_brain")
                    || u.text.contains("do not know")
                    || u.text.contains("do not see Claude"),
                "{}",
                u.text
            );
            assert_ne!(
                u.text, "I have no idea, honestly",
                "she must not have run the answering turn at all"
            );
            assert_eq!(u.urgency, Urgency::Answer);
        }
        other => panic!("she should have admitted it, got {other:?}"),
    }
    // The honest answer is still only a proposal.
    assert_eq!(m.take_outbox().len(), 1);
}

#[tokio::test]
async fn with_no_model_at_all_she_says_she_cannot_rather_than_crashing() {
    let f = Fixture::new();
    // Nothing placed on disk.
    let mut m = mind_with(&f, &[]);
    let t = m
        .think(Ask::from_operator("what is going on"), 1, 1_000)
        .await
        .expect("think");
    assert!(matches!(t, Thought::OutOfDepth(_)), "{t:?}");
    assert_eq!(m.take_outbox().len(), 1);
}

#[tokio::test]
async fn a_conversation_is_remembered_and_recalled_next_time() {
    let f = Fixture::new();
    f.place_all();
    let mut m = mind_with(
        &f,
        &[
            (
                "brand colour",
                r#"{"say":"Violet. #7700FF, specifically."}"#,
            ),
            ("remind me", r#"{"say":"You told me it was violet."}"#),
        ],
    );
    m.think(Ask::from_operator("what is my brand colour"), 1, 1_000)
        .await
        .expect("think");

    // The turn went into the episodic log, so it comes back as context.
    let hits = m
        .memory()
        .recall("brand colour violet", 3)
        .expect("recall");
    assert!(
        hits.iter().any(|h| h.memo.text.contains("7700FF")),
        "{hits:?}"
    );
}

#[tokio::test]
async fn her_own_idle_thoughts_do_not_get_answer_urgency() {
    let f = Fixture::new();
    f.place_all();
    let mut m = mind_with(&f, &[("mutter", r#"{"say":"hm."}"#)]);
    let t = m.think(Ask::her_own("mutter"), 1, 1_000).await.expect("think");
    let u = t.utterance().expect("an utterance");
    assert_eq!(
        u.urgency,
        Urgency::Notable,
        "only the operator's questions are free"
    );
    assert!(u.urgency.cost() > 0, "idle chatter pays the budget");
}

#[tokio::test]
async fn a_timer_coming_due_becomes_a_proposal_and_not_a_notification() {
    let f = Fixture::new();
    f.place_all();
    let mut m = mind_with(&f, &[]);
    m.tools()
        .invoke(
            "timer_set",
            serde_json::json!({"label": "tea", "minutes": 3}),
        )
        .await
        .expect("timer");

    // Nothing yet.
    m.tick(1_000);
    assert_eq!(m.outbox_len(), 0);

    f.advance(4 * 60_000);
    m.tick(2_000);
    let out = m.take_outbox();
    assert_eq!(out.len(), 1);
    assert!(out[0].text.contains("tea"), "{}", out[0].text);
    assert_eq!(out[0].urgency, Urgency::Answer);
}

#[tokio::test]
async fn the_governor_can_take_the_models_away_mid_conversation() {
    let f = Fixture::new();
    f.place_all();
    let mut m = mind_with(&f, &[("still there", r#"{"say":"yes"}"#)]);
    m.think(Ask::from_operator("still there"), 1, 1_000)
        .await
        .expect("think");
    assert!(m.manager().vram_held_mib() > 0);

    // A game starts, then a headset.
    for (tier, reason) in [
        (
            Tier::Reduced,
            TierReason::HeavyProcess {
                name: "gamescope".into(),
            },
        ),
        (Tier::Lobotomised, TierReason::VrSession),
    ] {
        let (d, b) = gov::desktop(tier, None);
        m.apply_governor(d, b);
        m.set_tier(tier, &reason);
    }
    assert_eq!(m.manager().vram_held_mib(), 0);
    // The conversation caches went with the contexts, but the persona prefix is
    // text and survives, so coming back does not re-prefill it.
    assert!(m.kv().conversations().is_empty());
    assert!(!m.kv().persona_tokens().is_empty());
}
