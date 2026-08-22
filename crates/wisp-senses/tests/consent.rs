//! The properties F30 exists to guarantee, tested from outside the crate — so
//! they hold through the public API and not merely through module privacy.
//!
//! Every test here sets `NX_WISP_CONFIG_DIR` to a temp dir (SPEC §4). Nothing
//! in this file touches D-Bus, Wayland, PipeWire or the operator's state.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use tokio::sync::broadcast;
use wisp_proto::{Consent, Event, EventKind, Observation, SenseId};
use wisp_senses::consent::{ships_enabled, ConsentLedger};
use wisp_senses::{Clock, ConsentError, PublishError, Sense, ALL_SENSES};

// ---------------------------------------------------------------------------
// Scaffolding
// ---------------------------------------------------------------------------

fn env_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// SPEC §4, restated for the integration suite: the dev build and the installed
/// copy share `$XDG_CONFIG_HOME/nx-wisp` otherwise, and a fixture then writes
/// into the operator's real consent state.
struct Sandbox {
    dir: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
    _guard: MutexGuard<'static, ()>,
}

impl Sandbox {
    fn new() -> Self {
        let guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("NX_WISP_CONFIG_DIR");
        std::env::set_var("NX_WISP_CONFIG_DIR", dir.path());
        Sandbox { dir, previous, _guard: guard }
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(v) => std::env::set_var("NX_WISP_CONFIG_DIR", v),
            None => std::env::remove_var("NX_WISP_CONFIG_DIR"),
        }
    }
}

fn ledger(sb: &Sandbox) -> (ConsentLedger, broadcast::Receiver<Event>) {
    let (tx, rx) = broadcast::channel(256);
    (ConsentLedger::load_from(sb.path(), tx, Clock::new()), rx)
}

// Stand-ins for the real senses, so this file tests the consent machinery and
// not a D-Bus connection.
struct Vitals;
impl Sense for Vitals {
    const ID: SenseId = SenseId::Vitals;
    const LABEL: &'static str = "Vitals";
    const DESCRIPTION: &'static str = "test";
}

struct Files;
impl Sense for Files {
    // `Observation::Files` reports `SenseId::Vitals` in wisp-proto.
    const ID: SenseId = SenseId::Vitals;
    const LABEL: &'static str = "Files";
    const DESCRIPTION: &'static str = "test";
}

struct Clipboard;
impl Sense for Clipboard {
    const ID: SenseId = SenseId::Clipboard;
    const LABEL: &'static str = "Clipboard";
    const DESCRIPTION: &'static str = "test";
}

struct Mic;
impl Sense for Mic {
    const ID: SenseId = SenseId::Microphone;
    const LABEL: &'static str = "Microphone";
    const DESCRIPTION: &'static str = "test";
}

fn sensed(rx: &mut broadcast::Receiver<Event>) -> Vec<Observation> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let EventKind::Sensed(o) = ev.kind {
            out.push(o);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The guarantees
// ---------------------------------------------------------------------------

/// SPEC §3.7: "Defaults ship as: ambient on, explicit off, invasive off."
#[test]
fn shipped_defaults_match_the_spec() {
    let sb = Sandbox::new();
    let (l, _rx) = ledger(&sb);
    for id in ALL_SENSES {
        let expected = matches!(id.consent(), Consent::Ambient);
        assert_eq!(l.is_enabled(id), expected, "{id:?} ships wrong");
        assert_eq!(ships_enabled(id), expected);
    }
}

/// The load-bearing one. A sense cannot run without its consent, because the
/// thing it needs in order to publish is the thing consent hands out.
#[test]
fn an_invasive_sense_cannot_obtain_a_handle_until_the_operator_says_so() {
    let sb = Sandbox::new();
    let (l, mut rx) = ledger(&sb);

    assert_eq!(
        l.grant::<Clipboard>().unwrap_err(),
        ConsentError::NotEnabled(SenseId::Clipboard, Consent::Invasive)
    );
    assert_eq!(
        l.grant::<Mic>().unwrap_err(),
        ConsentError::NotEnabled(SenseId::Microphone, Consent::Invasive)
    );
    assert!(sensed(&mut rx).is_empty(), "a refused sense must not reach the bus at all");

    l.set_enabled(SenseId::Clipboard, true).unwrap();
    let h = l.grant::<Clipboard>().expect("consent given");
    h.publish(Observation::Clipboard { len: 9, kind: "text/plain".into() }).unwrap();
    assert_eq!(sensed(&mut rx).len(), 1);
}

/// SPEC §0.3: mic, clipboard and screen have a visible tell on the character
/// for the entire time they are live.
#[test]
fn the_invasive_tell_brackets_the_sense_exactly() {
    let sb = Sandbox::new();
    let (l, mut rx) = ledger(&sb);
    l.set_enabled(SenseId::Clipboard, true).unwrap();

    let h = l.grant::<Clipboard>().unwrap();
    let up = rx.try_recv().unwrap();
    assert_eq!(up.kind, EventKind::InvasiveActive { sense: SenseId::Clipboard, active: true });

    h.publish(Observation::Clipboard { len: 1, kind: "text/plain".into() }).unwrap();
    let _ = rx.try_recv();

    drop(h);
    let down = rx.try_recv().unwrap();
    assert_eq!(down.kind, EventKind::InvasiveActive { sense: SenseId::Clipboard, active: false });
    // The tell went up before anything was sensed and came down after.
    assert!(up.at <= down.at);
}

/// Ambient senses are not invasive and must not raise a tell — if idle sensing
/// lit her eyes up, the tell would stop meaning anything.
#[test]
fn ambient_senses_raise_no_tell() {
    let sb = Sandbox::new();
    let (l, mut rx) = ledger(&sb);
    let h = l.grant::<Vitals>().unwrap();
    h.publish(Observation::Vitals {
        cpu_pct: 5,
        gpu_pct: 0,
        vram_used_mib: 100,
        temp_c: 40,
        on_battery: false,
    })
    .unwrap();
    drop(h);
    let kinds: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).map(|e| e.kind).collect();
    assert!(
        !kinds.iter().any(|k| matches!(k, EventKind::InvasiveActive { .. })),
        "ambient sense raised a tell: {kinds:?}"
    );
}

/// The other load-bearing one: a handle can only speak for its own `SenseId`.
#[test]
fn a_handle_refuses_every_other_senses_observations() {
    let sb = Sandbox::new();
    let (l, mut rx) = ledger(&sb);
    let h = l.grant::<Vitals>().unwrap();

    let foreign = [
        Observation::Idle { idle: true, for_ms: 1 },
        Observation::Focus { app_id: "x".into(), title: "y".into() },
        Observation::Window { id: 1, x: 0, y: 0, w: 1, h: 1, gone: false },
        Observation::Media {
            player: "p".into(),
            title: "t".into(),
            artist: "a".into(),
            playing: true,
        },
        Observation::AudioLevel { out: 10, mic_live: false },
        Observation::Notification { app: "a".into(), summary: "s".into(), body: "b".into() },
        Observation::Workspace { index: 0, name: "d".into() },
        Observation::Clipboard { len: 1, kind: "text/plain".into() },
        Observation::Speech { text: "hello".into(), final_: true },
        Observation::Fleet { app: "a".into(), field: "f".into(), value: "v".into() },
    ];
    for obs in foreign {
        let attempted = obs.sense();
        assert_eq!(
            h.publish(obs.clone()).unwrap_err(),
            PublishError::WrongSense { held: SenseId::Vitals, attempted },
            "a Vitals handle published {obs:?}"
        );
    }
    assert!(sensed(&mut rx).is_empty(), "a refused observation still reached the bus");

    // Its own two are fine — `Observation::Files` is a Vitals observation.
    h.publish(Observation::Vitals {
        cpu_pct: 1,
        gpu_pct: 1,
        vram_used_mib: 1,
        temp_c: 1,
        on_battery: false,
    })
    .unwrap();
    h.publish(Observation::Files { path: "/x".into(), dirty: true }).unwrap();
    assert_eq!(sensed(&mut rx).len(), 2);
}

/// Revoking consent while a sense is running stops it immediately, without
/// needing the sense to notice or cooperate.
#[test]
fn revocation_takes_effect_on_the_next_publication() {
    let sb = Sandbox::new();
    let (l, mut rx) = ledger(&sb);
    let h = l.grant::<Vitals>().unwrap();
    let sample = Observation::Vitals {
        cpu_pct: 1,
        gpu_pct: 1,
        vram_used_mib: 1,
        temp_c: 1,
        on_battery: false,
    };
    h.publish(sample.clone()).unwrap();

    l.set_enabled(SenseId::Vitals, false).unwrap();
    assert_eq!(h.publish(sample.clone()).unwrap_err(), PublishError::Revoked(SenseId::Vitals));
    assert!(!h.still_permitted());

    // And granting it again brings it back.
    l.set_enabled(SenseId::Vitals, true).unwrap();
    h.publish(sample).unwrap();
    assert_eq!(sensed(&mut rx).len(), 2);
}

/// Two senses share `SenseId::Vitals` because `wisp-proto` maps
/// `Observation::Files` onto it. Both must work, and the ledger must not treat
/// one finishing as both finishing.
#[test]
fn two_senses_may_share_one_id() {
    let sb = Sandbox::new();
    let (l, mut rx) = ledger(&sb);
    let vitals = l.grant::<Vitals>().unwrap();
    let files = l.grant::<Files>().unwrap();

    files.publish(Observation::Files { path: "/repo".into(), dirty: true }).unwrap();
    drop(files);

    // Dropping one must not revoke the other.
    assert!(vitals.still_permitted());
    vitals
        .publish(Observation::Vitals {
            cpu_pct: 1,
            gpu_pct: 1,
            vram_used_mib: 1,
            temp_c: 1,
            on_battery: false,
        })
        .unwrap();
    assert_eq!(sensed(&mut rx).len(), 2);
    assert_eq!(l.uses_today(SenseId::Vitals), 2);
}

/// F30's "live used N times today" counter.
#[test]
fn the_counter_counts_only_what_actually_reached_the_bus() {
    let sb = Sandbox::new();
    let (l, _rx) = ledger(&sb);
    let h = l.grant::<Vitals>().unwrap();
    let sample = Observation::Vitals {
        cpu_pct: 1,
        gpu_pct: 1,
        vram_used_mib: 1,
        temp_c: 1,
        on_battery: false,
    };
    for _ in 0..5 {
        h.publish(sample.clone()).unwrap();
    }
    // Refusals do not count.
    let _ = h.publish(Observation::Idle { idle: true, for_ms: 1 });
    l.set_enabled(SenseId::Vitals, false).unwrap();
    let _ = h.publish(sample);

    assert_eq!(l.uses_today(SenseId::Vitals), 5);
    assert_eq!(l.uses_today(SenseId::Clipboard), 0);
    let row = l.rows().into_iter().find(|r| r.id == SenseId::Vitals).unwrap();
    assert_eq!(row.uses_today, 5);
}

/// The operator's choices survive a restart, and the counters with them.
#[test]
fn consent_and_counters_survive_a_restart() {
    let sb = Sandbox::new();
    {
        let (l, _rx) = ledger(&sb);
        l.set_enabled(SenseId::Clipboard, true).unwrap();
        l.set_enabled(SenseId::Notifications, false).unwrap();
        let h = l.grant::<Clipboard>().unwrap();
        h.publish(Observation::Clipboard { len: 5, kind: "text/plain".into() }).unwrap();
    }
    let (l2, _rx) = ledger(&sb);
    assert!(l2.is_enabled(SenseId::Clipboard), "an opt-in was forgotten");
    assert!(!l2.is_enabled(SenseId::Notifications), "an opt-out was forgotten");
    assert!(l2.is_enabled(SenseId::Vitals), "an untouched ambient sense changed");
    assert_eq!(l2.uses_today(SenseId::Clipboard), 1);
}

/// Corruption must fail *closed*. The safe position for an unreadable consent
/// file is the shipped default, which has the invasive senses off.
#[test]
fn a_damaged_state_file_falls_back_to_the_safe_defaults() {
    let sb = Sandbox::new();
    {
        let (l, _rx) = ledger(&sb);
        l.set_enabled(SenseId::Clipboard, true).unwrap();
    }
    let file = sb.path().join("senses.json");
    assert!(file.exists());
    std::fs::write(&file, b"\x00\x00truncated").unwrap();

    let (l2, _rx) = ledger(&sb);
    assert!(!l2.is_enabled(SenseId::Clipboard), "corruption must not leave an invasive sense on");
    assert!(!l2.is_enabled(SenseId::Microphone));
    assert!(l2.is_enabled(SenseId::Idle));
}

/// Nothing is written outside the sandbox.
#[test]
fn consent_state_lives_only_where_it_was_told_to() {
    let sb = Sandbox::new();
    assert_eq!(wisp_senses::consent::config_dir(), sb.path());
    let (l, _rx) = ledger(&sb);
    l.set_enabled(SenseId::Media, false).unwrap();

    let written: Vec<PathBuf> = std::fs::read_dir(sb.path())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert!(
        written.iter().any(|p| p.ends_with("senses.json")),
        "nothing was written where it was told to: {written:?}"
    );
    // And the temp file used for the atomic rename does not survive.
    assert!(!sb.path().join("senses.json.tmp").exists());
}

/// The consent panel is the operator's whole view of what she can see. Every
/// `SenseId` must appear on it, with prose a person can act on.
#[test]
fn the_panel_describes_every_sense_in_plain_english() {
    let sb = Sandbox::new();
    let (l, _rx) = ledger(&sb);
    let rows = l.rows();
    assert_eq!(rows.len(), ALL_SENSES.len());
    for r in &rows {
        assert!(!r.label.is_empty(), "{:?} has no label", r.id);
        assert!(r.description.len() >= 40, "{:?}'s description is a stub", r.id);
        assert!(
            r.description.ends_with('.'),
            "{:?}'s description is not a sentence",
            r.id
        );
        assert_eq!(r.consent, r.id.consent());
        assert!(!r.live, "nothing is running in this test");
    }
    // Ambient senses come first so the panel reads from least to most alarming.
    let first_invasive = rows.iter().position(|r| r.consent == Consent::Invasive).unwrap();
    assert!(rows[..first_invasive].iter().all(|r| r.consent != Consent::Invasive));
}
