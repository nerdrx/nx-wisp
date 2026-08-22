//! **F18 — memory, and the fact that it forgets.**
//!
//! > *Episodic log of what happened, semantic recall, nightly summarization at
//! > idle, and **decay** — old trivia fades unless reinforced. Forgetting is a
//! > feature.*
//!
//! A feature has to be testable, so the first test in this file watches a
//! memory actually fade, and the second watches an identical one survive
//! because she kept being asked about it. Nothing here waits for real time: the
//! store takes `now` from the caller, so six weeks pass in a microsecond.

mod common;

use common::{Fixture, DAY_MS};
use wisp_mind::backend::{mock::MockBackend, Backend, LoadRequest, Role};
use wisp_mind::memory::embed::{Embedder, HashEmbedder};
use wisp_mind::memory::{Memory, MemoryConfig, MemoryKind, NewMemory};
use wisp_proto::Tier;

const T0: i64 = 1_700_000_000_000;

fn store() -> (Memory, Box<dyn Embedder>) {
    (
        Memory::in_memory().expect("memory"),
        Box::new(HashEmbedder::default()),
    )
}

// ---------------------------------------------------------------------------
// Decay
// ---------------------------------------------------------------------------

#[test]
fn a_memory_actually_fades() {
    let _f = Fixture::new(); // SPEC §4, even though this store is in memory.
    let (mut m, mut e) = store();

    let id = m
        .remember(
            e.as_mut(),
            NewMemory::episodic("the window title had 47 tabs in it").salience(0.2),
            T0,
        )
        .expect("remember");

    // Fresh.
    let now = m.strength_of(id, T0).expect("strength");
    assert!(now > 0.99, "a new memory is whole: {now}");

    // The curve, sampled. This is the assertion the feature is: it goes down,
    // and it keeps going down.
    let mut last = now;
    for days in [1, 4, 8, 16, 32, 64] {
        let s = m
            .strength_of(id, T0 + days * DAY_MS)
            .expect("strength");
        assert!(
            s < last,
            "after {days} days it should be weaker than before: {s} >= {last}"
        );
        last = s;
    }
    assert!(
        last < 0.02,
        "two months on, a passing detail should be all but gone: {last}"
    );

    // And a sweep really deletes it.
    let gone = m.forget(T0 + 64 * DAY_MS).expect("forget");
    assert_eq!(gone.len(), 1);
    assert_eq!(gone[0].id, id);
    assert!(m.get(id).expect("get").is_none(), "it is really gone");
    assert_eq!(m.count().expect("count"), 0);
}

#[test]
fn what_she_is_asked_about_survives_what_she_is_not() {
    let _f = Fixture::new();
    let (mut m, mut e) = store();

    let forgotten = m
        .remember(
            e.as_mut(),
            NewMemory::episodic("a notification about a package delivery").salience(0.2),
            T0,
        )
        .expect("remember");
    let kept = m
        .remember(
            e.as_mut(),
            NewMemory::episodic("they hate being interrupted while shaders compile")
                .salience(0.2),
            T0,
        )
        .expect("remember");

    // Every fortnight, the same subject comes up again.
    for week in [2, 4, 6, 8] {
        // Exactly one, so only the memory she actually used is reinforced —
        // that is the whole mechanism, and a wider net would reinforce both and
        // prove nothing.
        let hits = m
            .recall(e.as_mut(), "shaders compiling interruptions", 1, T0 + week * 7 * DAY_MS)
            .expect("recall");
        assert_eq!(
            hits.first().map(|h| h.memo.id),
            Some(kept),
            "week {week}: she should still find it, got {hits:?}"
        );
    }

    let then = T0 + 64 * DAY_MS;
    let strong = m.strength_of(kept, then).expect("strength");
    let weak = m.strength_of(forgotten, then).expect("strength");
    assert!(
        strong > weak * 10.0,
        "being used is what keeps a memory: {strong} vs {weak}"
    );

    let gone = m.forget(then).expect("forget");
    let gone_ids: Vec<i64> = gone.iter().map(|g| g.id).collect();
    assert!(gone_ids.contains(&forgotten), "the trivia should be gone");
    assert!(!gone_ids.contains(&kept), "the useful one should not be");
}

#[test]
fn salience_is_what_makes_a_half_life() {
    let _f = Fixture::new();
    let (mut m, mut e) = store();
    let trivial = m
        .remember(e.as_mut(), NewMemory::episodic("a beep").salience(0.0), T0)
        .expect("remember");
    let important = m
        .remember(
            e.as_mut(),
            NewMemory::episodic("the GPU hit 97°C").salience(1.0),
            T0,
        )
        .expect("remember");

    let then = T0 + 14 * DAY_MS;
    assert!(
        m.strength_of(important, then).expect("s") > m.strength_of(trivial, then).expect("s") * 3.0,
        "something that mattered should outlive a beep by a wide margin"
    );
}

#[test]
fn a_note_never_fades_because_it_was_written_down_on_purpose() {
    let _f = Fixture::new();
    let (mut m, mut e) = store();
    let id = m
        .remember(
            e.as_mut(),
            NewMemory::note("the signing key lives outside every git repository"),
            T0,
        )
        .expect("remember");

    let a_year = T0 + 365 * DAY_MS;
    assert_eq!(m.strength_of(id, a_year).expect("strength"), 1.0);
    assert!(
        m.forget(a_year).expect("forget").is_empty(),
        "a decay sweep must never eat a note"
    );
    let hits = m
        .recall(e.as_mut(), "where is the signing key", 3, a_year)
        .expect("recall");
    assert!(hits.iter().any(|h| h.memo.id == id), "{hits:?}");
}

#[test]
fn a_faded_memory_is_reported_as_faded_rather_than_quietly_presented_as_fresh() {
    let _f = Fixture::new();
    let (mut m, mut e) = store();
    m.remember(
        e.as_mut(),
        NewMemory::episodic("the shader cache was rebuilt").salience(0.5),
        T0,
    )
    .expect("remember");

    let fresh = m
        .recall(e.as_mut(), "shader cache rebuild", 1, T0)
        .expect("recall");
    assert!(fresh[0].strength > 0.99);

    // Recall reinforced it, so age from the moment it was last used.
    let later = T0 + 20 * DAY_MS;
    let old = m
        .recall(e.as_mut(), "shader cache rebuild", 1, later)
        .expect("recall");
    assert!(!old.is_empty());
    assert!(
        old[0].strength < 0.9,
        "she should know it is an old memory: {}",
        old[0].strength
    );
    // Similarity is unaffected by age — the two numbers mean different things
    // and are reported separately.
    assert!((old[0].similarity - fresh[0].similarity).abs() < 1e-3);
}

// ---------------------------------------------------------------------------
// Recall
// ---------------------------------------------------------------------------

#[test]
fn recall_finds_the_thing_it_is_about_and_not_the_others() {
    let _f = Fixture::new();
    let (mut m, mut e) = store();
    for (text, sal) in [
        ("wivrn started streaming to the headset", 0.4),
        ("the coffee machine descaling light came on", 0.2),
        ("shader compilation stalled for four minutes", 0.5),
        ("they renamed the branch to nx-wisp-main", 0.3),
    ] {
        m.remember(e.as_mut(), NewMemory::episodic(text).salience(sal), T0)
            .expect("remember");
    }
    let hits = m
        .recall(e.as_mut(), "why did shader compiling stall", 2, T0)
        .expect("recall");
    assert!(!hits.is_empty());
    assert!(
        hits[0].memo.text.contains("shader"),
        "got {:?}",
        hits.iter().map(|h| &h.memo.text).collect::<Vec<_>>()
    );
}

#[test]
fn recall_without_an_embedding_model_still_finds_things_by_words() {
    let _f = Fixture::new();
    let (mut m, mut e) = store();
    m.remember(
        e.as_mut(),
        NewMemory::note("the WiVRn bitrate ladder tops out at 200 Mbps"),
        T0,
    )
    .expect("remember");
    // The T3 path: no embedder at all.
    let hits = m.recall_lexical("bitrate", 5).expect("lexical");
    assert_eq!(hits.len(), 1);
    assert!(hits[0].text.contains("200 Mbps"));
}

#[test]
fn a_store_written_by_one_embedder_is_not_scored_against_another() {
    let _f = Fixture::new();
    let mut m = Memory::in_memory().expect("memory");
    let mut narrow: Box<dyn Embedder> = Box::new(HashEmbedder::new(64));
    let mut wide: Box<dyn Embedder> = Box::new(HashEmbedder::new(256));

    m.remember(
        narrow.as_mut(),
        NewMemory::episodic("the headset dropped to 60 Hz"),
        T0,
    )
    .expect("remember");

    // Same words, different embedder: it is invisible rather than wrongly
    // scored, and `recall_lexical` is the honest fallback for the old rows.
    let hits = m.recall(wide.as_mut(), "headset dropped", 5, T0).expect("recall");
    assert!(hits.is_empty(), "{hits:?}");
    assert_eq!(m.recall_lexical("headset", 5).expect("lexical").len(), 1);

    // New rows under the new embedder work normally.
    m.remember(wide.as_mut(), NewMemory::episodic("the headset reconnected"), T0)
        .expect("remember");
    let hits = m.recall(wide.as_mut(), "headset", 5, T0).expect("recall");
    assert_eq!(hits.len(), 1);
}

// ---------------------------------------------------------------------------
// Consolidation
// ---------------------------------------------------------------------------

fn loaded_mock() -> (MockBackend, wisp_mind::backend::ModelHandle) {
    let mut b = MockBackend::new().script(
        "Summarise what happened",
        r#"{"summary":"a long compile, a headset session, and nothing broke"}"#,
    );
    let l = b
        .load(&LoadRequest::new("m", "/nonexistent", Role::Deliberate))
        .expect("load");
    (b, l.handle)
}

#[test]
fn consolidation_refuses_to_run_while_the_operator_is_using_the_machine() {
    let _f = Fixture::new();
    let (mut m, mut e) = store();
    let (mut b, h) = loaded_mock();
    for i in 0..5 {
        m.remember(
            e.as_mut(),
            NewMemory::episodic(format!("thing {i} happened")),
            T0,
        )
        .expect("remember");
    }
    let then = T0 + DAY_MS;
    for tier in [Tier::Full, Tier::Reduced, Tier::Lobotomised, Tier::Dormant] {
        let err = m
            .consolidate(tier, &mut b, h, e.as_mut(), then)
            .unwrap_err();
        assert!(err.is_tier_refusal(), "{tier:?}: {err}");
    }
    // T0 Feral: the machine is idle and nobody is here.
    let c = m
        .consolidate(Tier::Feral, &mut b, h, e.as_mut(), then)
        .expect("consolidates at rest");
    assert_eq!(c.summarised, 5);
    assert!(c.created.is_some());
    assert!(c.summary.expect("summary").contains("headset"));
}

#[test]
fn a_summary_outlives_the_things_it_summarised() {
    let _f = Fixture::new();
    let mut m = Memory::in_memory()
        .expect("memory")
        .with_config(MemoryConfig::default());
    let mut e: Box<dyn Embedder> = Box::new(HashEmbedder::default());
    let (mut b, h) = loaded_mock();

    let sources: Vec<i64> = (0..6)
        .map(|i| {
            m.remember(
                e.as_mut(),
                NewMemory::episodic(format!("at {i}:00 the compile was still running"))
                    .salience(0.3),
                T0,
            )
            .expect("remember")
        })
        .collect();

    let then = T0 + DAY_MS;
    let c = m
        .consolidate(Tier::Feral, &mut b, h, e.as_mut(), then)
        .expect("consolidate");
    let summary_id = c.created.expect("a summary was written");

    // Sources are not deleted — SPEC §0.4 wants the real trace to survive — but
    // they are marked, and they now fade twice as fast.
    for s in &sources {
        let memo = m.get(*s).expect("get").expect("still there");
        assert_eq!(memo.consolidated_into, Some(summary_id));
        assert!(memo.salience < 0.3);
    }

    let far = then + 40 * DAY_MS;
    let summary_strength = m.strength_of(summary_id, far).expect("strength");
    let source_strength = m.strength_of(sources[0], far).expect("strength");
    assert!(
        summary_strength > source_strength,
        "the summary is what should still be there: {summary_strength} vs {source_strength}"
    );
    assert_eq!(
        m.get(summary_id).expect("get").expect("there").kind,
        MemoryKind::Semantic
    );
}

#[test]
fn consolidation_does_not_bother_with_a_handful_of_rows() {
    let _f = Fixture::new();
    let (mut m, mut e) = store();
    let (mut b, h) = loaded_mock();
    m.remember(e.as_mut(), NewMemory::episodic("one thing"), T0)
        .expect("remember");
    let c = m
        .consolidate(Tier::Feral, &mut b, h, e.as_mut(), T0 + DAY_MS)
        .expect("no-op");
    assert_eq!(c.summarised, 0);
    assert!(c.created.is_none());
}

#[test]
fn the_store_survives_being_closed_and_reopened() {
    let f = Fixture::new();
    let path = f.models_dir.parent().expect("data dir").join("memory.sqlite3");
    let mut e: Box<dyn Embedder> = Box::new(HashEmbedder::default());
    let id = {
        let mut m = Memory::open(&path).expect("open");
        m.remember(
            e.as_mut(),
            NewMemory::note("KWin's D-Bus scripting interface is a hard dependency"),
            T0,
        )
        .expect("remember")
    };
    let mut m = Memory::open(&path).expect("reopen");
    assert_eq!(m.count().expect("count"), 1);
    // The vector index was rebuilt from the file, so semantic recall works
    // across a restart and not just lexical fallback.
    let hits = m.recall(e.as_mut(), "kwin dbus scripting", 3, T0).expect("recall");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].memo.id, id);
}

#[test]
fn the_linear_index_is_still_the_right_choice_at_the_size_it_will_actually_be() {
    let _f = Fixture::new();
    let (mut m, mut e) = store();
    // A year of a busy operator's memorable events, roughly.
    for i in 0..4_000 {
        m.remember(
            e.as_mut(),
            NewMemory::episodic(format!("event number {i} about topic {}", i % 97))
                .salience(0.5),
            T0 + i as i64,
        )
        .expect("remember");
    }
    assert!(
        m.index_is_comfortable(),
        "past this size the exact-scan justification needs revisiting"
    );
    let started = std::time::Instant::now();
    let hits = m
        .recall(e.as_mut(), "event about topic 42", 5, T0 + 10_000)
        .expect("recall");
    let took = started.elapsed();
    assert_eq!(hits.len(), 5);
    // Generous by two orders of magnitude: this is a smoke alarm for an
    // accidentally quadratic change, not a benchmark.
    assert!(
        took < std::time::Duration::from_secs(2),
        "an exact scan over 4000 rows took {took:?}"
    );
}
