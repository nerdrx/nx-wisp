//! **F16 + SPEC §3.7** — the tool registry, and consent as an enforced rule
//! rather than a documented intention.

mod common;

use std::sync::{Arc, Mutex};

use common::Fixture;
use serde_json::json;
use wisp_fleet::tools::NxTools;
use wisp_mind::error::MindError;
use wisp_mind::events::EventSink;
use wisp_mind::memory::{embed::HashEmbedder, Memory, WallClock};
use wisp_mind::tools::builtin::{
    Builtins, FileSearch, MediaAction, MediaSink, MemoryHandle, Timers,
};
use wisp_mind::tools::{ok, sync_tool, ToolRegistry};
use wisp_proto::{Consent, EventKind};

fn memory_handle(f: &Fixture) -> MemoryHandle {
    MemoryHandle::new(
        Arc::new(Mutex::new(Memory::in_memory().expect("memory"))),
        Box::new(HashEmbedder::default()),
        f.clock.clone(),
    )
}

fn registry(f: &Fixture) -> (ToolRegistry, Builtins) {
    let b = Builtins::new(memory_handle(f));
    let mut r = ToolRegistry::new().with_events(f.events.clone());
    for (name, res) in r.register_all(b.descriptors()) {
        res.unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    (r, b)
}

#[tokio::test]
async fn an_ambient_tool_runs_without_being_asked_twice() {
    let f = Fixture::new();
    let (r, _b) = registry(&f);
    let out = r
        .invoke("timer_set", json!({"label": "tea", "minutes": 3}))
        .await
        .expect("ambient tools just run");
    assert!(out.ok, "{}", out.summary);
    assert!(out.summary.contains("3 minutes"), "{}", out.summary);
    assert_eq!(f.log.tool_calls(), vec![("timer_set".to_string(), true)]);
}

#[tokio::test]
async fn an_explicit_tool_refuses_until_the_operator_switches_it_on() {
    let f = Fixture::new();
    let (mut r, _b) = registry(&f);

    let err = r
        .invoke("media_control", json!({"action": "pause"}))
        .await
        .unwrap_err();
    match err {
        MindError::ConsentRequired { name, consent } => {
            assert_eq!(name, "media_control");
            assert_eq!(consent, Consent::Explicit);
        }
        other => panic!("expected a consent refusal, got {other}"),
    }
    // SPEC §0.4: the refusal is in the trace, not only the successes.
    assert_eq!(
        f.log.tool_calls(),
        vec![("media_control".to_string(), false)]
    );

    // She is not even told it exists until it is on.
    assert!(!r.available().iter().any(|d| d.name == "media_control"));
    assert!(r.enable("media_control", true).expect("enable"));
    assert!(r.available().iter().any(|d| d.name == "media_control"));

    // Now it runs — and reports honestly that there is no player here.
    let out = r
        .invoke("media_control", json!({"action": "pause"}))
        .await
        .expect("enabled");
    assert!(out.unavailable, "{}", out.summary);
}

#[tokio::test]
async fn an_ambient_tool_cannot_be_switched_off_by_pretending_to_enable_it() {
    let f = Fixture::new();
    let (mut r, _b) = registry(&f);
    // Ambient means ambient. `enable` is a no-op, not an error and not a toggle.
    assert!(!r.enable("timer_list", true).expect("noop"));
    assert!(!r.enable("timer_list", false).expect("noop"));
    assert!(r.invoke("timer_list", json!({})).await.is_ok());
}

#[tokio::test]
async fn an_invasive_tool_needs_a_visible_tell_and_not_just_permission() {
    let f = Fixture::new();
    let mut r = ToolRegistry::new().with_events(f.events.clone());
    r.register(sync_tool(
        "screen_read",
        "Look at the screen.",
        Consent::Invasive,
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
        |_| ok("I looked."),
    ))
    .expect("register");
    r.enable("screen_read", true).expect("enable");

    // Enabled, but SPEC §0.3's tell is not wired, so it still refuses.
    let err = r.invoke("screen_read", json!({})).await.unwrap_err();
    assert!(matches!(err, MindError::ConsentRequired { .. }), "{err}");

    // With a tell, it runs — and the tell goes up before and down after.
    let seen: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let mut r = ToolRegistry::new()
        .with_events(f.events.clone())
        .with_tell(Arc::new(move |name: &str, on: bool| {
            sink.lock().expect("tell").push((name.to_string(), on));
        }));
    r.register(sync_tool(
        "screen_read",
        "Look at the screen.",
        Consent::Invasive,
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
        |_| ok("I looked."),
    ))
    .expect("register");
    r.enable("screen_read", true).expect("enable");
    let out = r.invoke("screen_read", json!({})).await.expect("runs");
    assert!(out.ok);
    assert_eq!(
        *seen.lock().expect("tell"),
        vec![("screen_read".into(), true), ("screen_read".into(), false)],
        "the tell must be up for exactly as long as the tool is running"
    );
    // And the fact that something invasive was live is an event in its own
    // right (SPEC §0.3).
    let flags: Vec<bool> = f
        .log
        .all()
        .into_iter()
        .filter_map(|e| match e {
            EventKind::InvasiveActive { active, .. } => Some(active),
            _ => None,
        })
        .collect();
    assert_eq!(flags, vec![true, false]);
}

#[tokio::test]
async fn arguments_that_the_grammar_would_not_have_produced_are_refused() {
    let f = Fixture::new();
    let (r, _b) = registry(&f);
    for bad in [
        json!({"minutes": 3}),                       // no label
        json!({"label": "tea", "hours": 2}),         // undeclared key
        json!({"label": 7, "minutes": 3}),           // wrong type
        json!({"label": "tea", "minutes": "three"}), // wrong type
    ] {
        let err = r.invoke("timer_set", bad.clone()).await.unwrap_err();
        assert!(
            matches!(err, MindError::BadArguments { .. }),
            "{bad} should have been refused, got {err}"
        );
    }
    // The validator and the decoder's constraint are the same grammar, so
    // anything the model could have emitted is accepted here.
    r.validate("timer_set", &json!({"label": "tea", "minutes": 3}))
        .expect("valid");
}

#[tokio::test]
async fn a_tool_that_does_not_exist_is_refused_and_recorded() {
    let f = Fixture::new();
    let (r, _b) = registry(&f);
    let err = r.invoke("rm_rf", json!({})).await.unwrap_err();
    assert!(matches!(err, MindError::NoSuchTool(_)), "{err}");
    assert_eq!(f.log.tool_calls(), vec![("rm_rf".to_string(), false)]);
}

#[test]
fn a_tool_whose_schema_cannot_be_constrained_is_not_registered_at_all() {
    let mut r = ToolRegistry::new();
    let err = r
        .register(sync_tool(
            "vague",
            "Takes a duration, whatever that is.",
            Consent::Ambient,
            json!({"type": "object", "properties": {"when": {"type": "duration"}}}),
            |_| ok("nothing"),
        ))
        .unwrap_err();
    assert!(err.to_string().contains("duration"), "{err}");
    assert!(r.is_empty(), "it must not have been half-registered");
}

#[test]
fn the_fleet_tools_register_alongside_the_local_ones_and_keep_their_consent() {
    let f = Fixture::new();
    let (mut r, _b) = registry(&f);
    let n = r.len();
    let nx = NxTools::new("/nonexistent/nx");
    for (name, res) in r.register_all(nx.descriptors()) {
        res.unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    assert_eq!(r.len(), n + 4);
    assert_eq!(
        r.get("nx_stack_start").expect("registered").consent,
        Consent::Explicit
    );
    assert_eq!(
        r.get("nx_status").expect("registered").consent,
        Consent::Ambient
    );
    // And the merged grammar covers both sources.
    let src = r
        .grammar(&wisp_mind::grammar::GrammarOptions::TOOL_ONLY)
        .expect("grammar");
    let g = wisp_mind::grammar::Grammar::parse(&src).expect("parses");
    assert!(g.accepts(r#"{"name":"nx_status","arguments":{}}"#));
    assert!(g.accepts(r#"{"name":"timer_list","arguments":{}}"#));
    // The ones that are switched off are absent from the language entirely.
    assert!(!g.accepts(r#"{"name":"nx_stack_start","arguments":{"stack":"vr"}}"#));
}

#[test]
fn the_operators_choices_survive_a_restart_and_land_in_the_isolated_config_dir() {
    let f = Fixture::new();
    let path = wisp_mind::dirs::config_dir().join("mind").join("tools.json");
    {
        let b = Builtins::new(memory_handle(&f));
        let mut r = ToolRegistry::new().with_state_file(&path);
        for (_, res) in r.register_all(b.descriptors()) {
            res.expect("register");
        }
        r.enable("media_control", true).expect("enable");
    }
    assert!(path.exists(), "consent must be written under NX_WISP_CONFIG_DIR");
    assert!(
        path.starts_with(f.models_dir.parent().expect("data").parent().expect("config")),
        "and nowhere near the operator's real profile"
    );

    let b = Builtins::new(memory_handle(&f));
    let mut r = ToolRegistry::new().with_state_file(&path);
    for (_, res) in r.register_all(b.descriptors()) {
        res.expect("register");
    }
    r.load().expect("load");
    assert!(r.is_enabled("media_control"));
    assert!(!r.is_enabled("file_search"));
}

#[tokio::test]
async fn file_search_says_so_when_it_has_nowhere_to_look() {
    let f = Fixture::new();
    let (mut r, _b) = registry(&f);
    r.enable("file_search", true).expect("enable");
    let out = r
        .invoke("file_search", json!({"query": "shader"}))
        .await
        .expect("runs");
    assert!(out.unavailable, "{}", out.summary);
    assert!(out.summary.contains("not given me any folders"), "{}", out.summary);
}

#[tokio::test]
async fn file_search_finds_things_where_it_is_allowed_to_look() {
    let f = Fixture::new();
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("shader_cache.log"), b"x").expect("write");
    let b = Builtins::new(memory_handle(&f))
        .with_files(FileSearch::under(vec![dir.path().to_path_buf()]));
    let mut r = ToolRegistry::new().with_events(f.events.clone());
    for (_, res) in r.register_all(b.descriptors()) {
        res.expect("register");
    }
    r.enable("file_search", true).expect("enable");
    let out = r
        .invoke("file_search", json!({"query": "shader"}))
        .await
        .expect("runs");
    assert!(out.ok, "{}", out.summary);
    assert!(out.summary.contains("shader_cache.log"), "{}", out.summary);
}

#[tokio::test]
async fn notes_go_into_memory_and_recall_finds_them_again() {
    let f = Fixture::new();
    let (r, _b) = registry(&f);
    r.invoke(
        "note_write",
        json!({"text": "the operator's brand colour is #7700FF"}),
    )
    .await
    .expect("note");

    let out = r
        .invoke("recall", json!({"query": "what colour do they like"}))
        .await
        .expect("recall");
    assert!(out.ok);
    assert!(out.summary.contains("7700FF"), "{}", out.summary);
    // Strength is reported, so she can tell a fresh memory from a fading one.
    let hits = out.json.expect("json");
    assert!(hits["hits"][0]["strength"].as_f64().expect("strength") > 0.9);
}

/// A media player that is actually there, for the path the operator's machine
/// takes.
struct FakePlayer(Arc<Mutex<Vec<MediaAction>>>);

impl MediaSink for FakePlayer {
    fn control(&self, action: MediaAction) -> Result<String, String> {
        self.0.lock().expect("player").push(action);
        Ok(format!("Done: {action:?}"))
    }
}

#[tokio::test]
async fn media_control_reaches_the_injected_player_and_nothing_else() {
    let f = Fixture::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let b = Builtins::new(memory_handle(&f))
        .with_media(Arc::new(FakePlayer(Arc::clone(&seen))));
    let mut r = ToolRegistry::new().with_events(f.events.clone());
    for (_, res) in r.register_all(b.descriptors()) {
        res.expect("register");
    }
    r.enable("media_control", true).expect("enable");
    let out = r
        .invoke("media_control", json!({"action": "next"}))
        .await
        .expect("runs");
    assert!(out.ok, "{}", out.summary);
    assert_eq!(*seen.lock().expect("player"), vec![MediaAction::Next]);

    // An action outside the enum is not in the grammar and not in the registry.
    let err = r
        .invoke("media_control", json!({"action": "eject"}))
        .await
        .unwrap_err();
    assert!(matches!(err, MindError::BadArguments { .. }), "{err}");
    assert_eq!(seen.lock().expect("player").len(), 1, "nothing extra reached the player");
}

#[test]
fn timers_have_no_clock_of_their_own() {
    // The event loop owns the clock (SPEC §0.1: the governor must be able to
    // see every wakeup), so a timer only fires when it is polled.
    let mut t = Timers::new();
    t.set("stand up", 60_000, 0);
    assert!(t.due(59_999).is_empty());
    assert_eq!(t.due(60_000).len(), 1);
}

#[test]
fn a_silent_sink_is_a_valid_sink_so_nothing_needs_a_recorder_to_work() {
    let f = Fixture::new();
    let b = Builtins::new(memory_handle(&f));
    let mut r = ToolRegistry::new().with_events(EventSink::silent());
    for (_, res) in r.register_all(b.descriptors()) {
        res.expect("register");
    }
    assert!(!r.is_empty());
    let _ = WallClock::system();
}
