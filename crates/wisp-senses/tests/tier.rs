//! SPEC §3.1 and charter §0.1 — the governor's verdict actually reaches the
//! senses, and it reaches them synchronously.
//!
//! `set_tier` must not block, must not fail, and must not queue: a sense told to
//! cost less looks less often, and the samples it did not take are gone.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use wisp_proto::{Cost, Governed, Observation, SenseId, Tier, TierReason};
use wisp_senses::{budget, Sense, SenseCtx, SenseHandle, SensePlugin, Senses, SensesConfig};

fn env_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

struct Sandbox {
    _dir: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
    _guard: MutexGuard<'static, ()>,
}

impl Sandbox {
    fn new() -> Self {
        let guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("NX_WISP_CONFIG_DIR");
        std::env::set_var("NX_WISP_CONFIG_DIR", dir.path());
        Sandbox { _dir: dir, previous, _guard: guard }
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

/// A sense that does nothing but record the tiers it was told about.
struct Spy {
    seen: Arc<Mutex<Vec<Tier>>>,
    ticks: Arc<AtomicU64>,
}

impl Sense for Spy {
    const ID: SenseId = SenseId::Vitals;
    const LABEL: &'static str = "Spy";
    const DESCRIPTION: &'static str = "test";
}

impl SensePlugin for Spy {
    fn spawn(self, handle: SenseHandle<Self>, mut ctx: SenseCtx) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.seen.lock().unwrap().push(ctx.tier());
            let mut shutdown = ctx.shutdown.clone();
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.wait() => break,
                    Some(tier) = ctx.tier_changed() => {
                        self.seen.lock().unwrap().push(tier);
                        self.ticks.fetch_add(1, Ordering::Relaxed);
                        // Publishing here proves the handle still works across
                        // a tier change.
                        handle.emit(Observation::Vitals {
                            cpu_pct: 0,
                            gpu_pct: 0,
                            vram_used_mib: 0,
                            temp_c: 0,
                            on_battery: false,
                        });
                    }
                }
            }
        })
    }
}

#[tokio::test]
async fn a_tier_change_reaches_a_running_sense() {
    let _sb = Sandbox::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let ticks = Arc::new(AtomicU64::new(0));

    let mut senses = Senses::new();
    senses.start(Spy { seen: seen.clone(), ticks: ticks.clone() }).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    for tier in [Tier::Reduced, Tier::Lobotomised, Tier::Dormant, Tier::Full] {
        senses.set_tier(tier, &TierReason::Idle);
        assert_eq!(senses.tier(), tier, "set_tier must take effect immediately");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    senses.shutdown().await;

    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![Tier::Full, Tier::Reduced, Tier::Lobotomised, Tier::Dormant, Tier::Full],
        "the sense did not see every tier the governor set"
    );
}

/// A downgrade must not block on the sense acknowledging it — the governor is
/// called from the frame loop and cannot wait for anybody.
#[tokio::test]
async fn set_tier_returns_without_waiting_for_anyone() {
    let _sb = Sandbox::new();
    let mut senses = Senses::new();
    // Nothing is running, so nobody is listening at all. That must be fine.
    let start = std::time::Instant::now();
    for _ in 0..1000 {
        senses.set_tier(Tier::Lobotomised, &TierReason::HeavyProcess { name: "game".into() });
        senses.set_tier(Tier::Full, &TierReason::Idle);
    }
    assert!(
        start.elapsed() < Duration::from_millis(200),
        "2000 tier changes took {:?}",
        start.elapsed()
    );
    senses.shutdown().await;
}

#[tokio::test]
async fn the_senses_start_at_full_and_report_it() {
    let _sb = Sandbox::new();
    let senses = Senses::new();
    assert_eq!(senses.tier(), Tier::Full);
    senses.shutdown().await;
}

/// The accounting the governor uses for its cost meter must agree with the
/// budgets the senses actually run at.
#[test]
fn cost_and_budget_agree_about_the_direction_of_travel() {
    let ladder = [Tier::Feral, Tier::Full, Tier::Reduced, Tier::Lobotomised, Tier::Dormant];
    for w in ladder.windows(2) {
        let (a, b) = (<Senses as Governed>::cost_at(w[0]), <Senses as Governed>::cost_at(w[1]));
        assert!(b.cpu_centi_pct <= a.cpu_centi_pct, "cost rose {:?} -> {:?}", w[0], w[1]);
        assert!(
            budget::terrain_flush_ms(w[1]) >= budget::terrain_flush_ms(w[0]),
            "budget loosened the wrong way {:?} -> {:?}",
            w[0],
            w[1]
        );
    }
    // She never holds VRAM. That is the mind's business, not the senses'.
    for t in ladder {
        assert_eq!(<Senses as Governed>::cost_at(t).vram_mib, 0);
    }
    assert_eq!(<Senses as Governed>::cost_at(Tier::Dormant).cpu_centi_pct, 0);
    assert_ne!(<Senses as Governed>::cost_at(Tier::Full), Cost::FREE);
}

/// Starting with an empty config must not touch D-Bus, Wayland or PipeWire in
/// ways that panic when none of them are reachable — a sense that cannot reach
/// its source logs and stops, it does not take the process down.
#[tokio::test]
async fn a_missing_backend_is_survivable() {
    let _sb = Sandbox::new();
    let mut senses = Senses::new();
    // Point every sense at something that does not exist.
    std::env::set_var("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent/nx-wisp-test");
    std::env::set_var("WAYLAND_DISPLAY", "nx-wisp-nonexistent");

    let cfg = SensesConfig {
        vitals: wisp_senses::vitals::VitalsConfig {
            sysfs_root: "/nonexistent".into(),
            proc_root: "/nonexistent".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    senses.start_all(&cfg);
    tokio::time::sleep(Duration::from_millis(150)).await;
    senses.shutdown().await;
}
