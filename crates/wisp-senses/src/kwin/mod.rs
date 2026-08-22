//! F21 and F4's terrain data — window sensing through a companion KWin script.
//!
//! `zwlr_foreign_toplevel_manager_v1` is not advertised by KWin 6.7 and
//! `ext_foreign_toplevel_list_v1` is not either (checked against the live
//! registry, see the crate docs), and neither would carry geometry even if they
//! were. Window rectangles come from a KWin script over D-Bus. SPEC §1 makes
//! that a permanent choice, not a fallback.
//!
//! The shape of it:
//!
//! ```text
//!   KWin (QJSEngine)                        wisp-senses
//!   terrain.js ──callDBus──▶ org.nx.Wisp.Senses /…/Terrain
//!     coalesces per flush            Hello(json)  ─▶ epoch handshake
//!     window signals                 Batch(json)  ─▶ TerrainDecoder
//!                                                    │
//!                                    SenseHandle<ActiveWindow>     ─▶ bus
//!                                    SenseHandle<WindowGeometry>   ─▶ bus
//! ```

pub mod decode;
pub mod script;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use wisp_proto::{Observation, SenseId};

use crate::budget;
use crate::consent::{Sense, SenseCtx, SenseHandle};
use decode::TerrainDecoder;

// ---------------------------------------------------------------------------
// The two senses this one feed serves
// ---------------------------------------------------------------------------

/// F21 — which app and window you are focused on.
pub struct ActiveWindowSense;
impl Sense for ActiveWindowSense {
    const ID: SenseId = SenseId::ActiveWindow;
    const LABEL: &'static str = "Active window";
    const DESCRIPTION: &'static str = crate::consent::description_of(SenseId::ActiveWindow);
}

/// F4 — the rectangles she walks on.
pub struct WindowGeometrySense;
impl Sense for WindowGeometrySense {
    const ID: SenseId = SenseId::WindowGeometry;
    const LABEL: &'static str = "Window geometry";
    const DESCRIPTION: &'static str = crate::consent::description_of(SenseId::WindowGeometry);
}

/// One feed, two consents. The operator can keep the terrain and refuse the
/// window titles, or the other way round, and the router simply drops what it
/// has no handle for. Nothing downstream has to know.
#[derive(Debug, Default)]
pub struct TerrainSenses {
    pub focus: Option<SenseHandle<ActiveWindowSense>>,
    pub geometry: Option<SenseHandle<WindowGeometrySense>>,
}

impl TerrainSenses {
    pub fn is_empty(&self) -> bool {
        self.focus.is_none() && self.geometry.is_none()
    }

    /// Dispatch by the observation's own `SenseId`. This is the only place the
    /// terrain feed reaches the bus, and it cannot route an observation to the
    /// wrong handle — `SenseHandle::publish` would refuse it anyway.
    pub fn route(&self, obs: Observation) {
        match obs.sense() {
            SenseId::ActiveWindow => {
                if let Some(h) = &self.focus {
                    h.emit(obs);
                }
            }
            SenseId::WindowGeometry => {
                if let Some(h) = &self.geometry {
                    h.emit(obs);
                }
            }
            other => tracing::error!(?other, ?obs, "terrain feed produced a foreign observation"),
        }
    }
}

// ---------------------------------------------------------------------------
// Stats — the answer to "how fresh can the terrain be?"
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct TerrainStats {
    pub batches: AtomicU64,
    pub window_updates: AtomicU64,
    pub focus_changes: AtomicU64,
    pub reconnects: AtomicU64,
    /// Wall time of the first and most recent batch, in `Clock` millis, so the
    /// smoke example can report an achieved rate.
    pub first_batch_ms: AtomicU64,
    pub last_batch_ms: AtomicU64,
}

impl TerrainStats {
    pub fn batches_per_second(&self) -> f64 {
        let n = self.batches.load(Ordering::Relaxed);
        let a = self.first_batch_ms.load(Ordering::Relaxed);
        let b = self.last_batch_ms.load(Ordering::Relaxed);
        if n < 2 || b <= a {
            return 0.0;
        }
        (n - 1) as f64 * 1000.0 / (b - a) as f64
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct TerrainConfig {
    /// Pin the coalescing interval instead of letting the governor choose it.
    /// The smoke example uses this to sweep the rate; the app leaves it `None`.
    pub flush_ms: Option<u32>,
    pub script_dir: Option<std::path::PathBuf>,
}

impl TerrainConfig {
    fn flush_for(&self, tier: wisp_proto::Tier) -> u32 {
        self.flush_ms.unwrap_or_else(|| budget::terrain_flush_ms(tier))
    }
}

// ---------------------------------------------------------------------------
// The D-Bus sink the script calls into
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Wire {
    Hello(String),
    Batch(String),
}

struct TerrainSink {
    tx: mpsc::UnboundedSender<Wire>,
}

/// Both methods take one JSON string. KWin's `callDBus` derives the D-Bus
/// signature from the JS value, so anything but a string risks arriving as a
/// double and silently failing to match.
#[zbus::interface(name = "org.nx.Wisp.Terrain")]
impl TerrainSink {
    /// The script announcing itself. Returns the protocol we speak so the
    /// script has an acknowledgement to wait for.
    async fn hello(&self, json: String) -> u32 {
        let _ = self.tx.send(Wire::Hello(json));
        decode::PROTOCOL
    }

    /// A coalesced batch of window changes. Deliberately returns nothing: the
    /// script fires and forgets, so a slow decoder can never stall KWin's main
    /// thread — which is also the thread that composites.
    async fn batch(&self, json: String) {
        let _ = self.tx.send(Wire::Batch(json));
    }
}

// ---------------------------------------------------------------------------
// KWin's scripting interface
// ---------------------------------------------------------------------------

#[zbus::proxy(
    interface = "org.kde.kwin.Scripting",
    default_service = "org.kde.KWin",
    default_path = "/Scripting"
)]
pub trait KwinScripting {
    #[zbus(name = "loadScript")]
    fn load_script(&self, file_path: &str, plugin_name: &str) -> zbus::Result<i32>;
    #[zbus(name = "unloadScript")]
    fn unload_script(&self, plugin_name: &str) -> zbus::Result<bool>;
    #[zbus(name = "isScriptLoaded")]
    fn is_script_loaded(&self, plugin_name: &str) -> zbus::Result<bool>;
    #[zbus(name = "start")]
    fn start(&self) -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------
// The supervisor
// ---------------------------------------------------------------------------

/// Install the script into the running KWin, replacing any leftover copy.
///
/// Read-only with respect to the operator's session: `loadScript` is a runtime
/// call that touches no config file and no KWin setting, and `unloadScript`
/// undoes it exactly.
pub async fn install_script(
    conn: &zbus::Connection,
    cfg: &TerrainConfig,
    epoch: u64,
    flush_ms: u32,
) -> anyhow::Result<std::path::PathBuf> {
    let sc = script::ScriptConfig { epoch, flush_ms, ..Default::default() };
    let dir = cfg.script_dir.clone().unwrap_or_else(script::script_dir);
    let path = script::install(&sc, &dir)?;

    let proxy = KwinScriptingProxy::new(conn).await?;
    // A previous run of the wisp may have left one loaded. Unloading a plugin
    // that is not there is not an error, it just returns false.
    let _ = proxy.unload_script(script::PLUGIN_NAME).await;
    let id = proxy
        .load_script(&path.to_string_lossy(), script::PLUGIN_NAME)
        .await?;
    proxy.start().await?;
    tracing::info!(
        script = %path.display(), id, epoch, flush_ms,
        "terrain script loaded into KWin"
    );
    Ok(path)
}

/// Take the script back out. Called on shutdown so we leave KWin exactly as we
/// found it.
pub async fn uninstall_script(conn: &zbus::Connection) {
    if let Ok(proxy) = KwinScriptingProxy::new(conn).await {
        match proxy.unload_script(script::PLUGIN_NAME).await {
            Ok(_) => tracing::info!("terrain script unloaded from KWin"),
            Err(e) => tracing::warn!(error = %e, "could not unload the terrain script"),
        }
    }
}

/// Run the terrain feed until shutdown.
///
/// Survives a KWin restart: the D-Bus name owner change is watched, the whole
/// world is retracted (she must not stand on a window belonging to a compositor
/// that no longer exists), and the script is reinstalled into the new KWin.
pub async fn run(
    senses: TerrainSenses,
    cfg: TerrainConfig,
    stats: Arc<TerrainStats>,
    mut ctx: SenseCtx,
) -> anyhow::Result<()> {
    if senses.is_empty() {
        tracing::info!("no consent for either window sense; terrain feed not started");
        return Ok(());
    }
    let clock = senses
        .focus
        .as_ref()
        .map(|h| h.clock())
        .or_else(|| senses.geometry.as_ref().map(|h| h.clock()))
        .expect("checked non-empty");

    let (tx, mut rx) = mpsc::unbounded_channel();
    let conn = zbus::connection::Builder::session()?
        .name(script::SERVICE)?
        .serve_at(script::OBJECT, TerrainSink { tx })?
        .build()
        .await?;

    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
    let mut owner_changes = dbus
        .receive_name_owner_changed_with_args(&[(0, "org.kde.KWin")])
        .await?;

    let mut decoder = TerrainDecoder::new();
    let mut epoch = script::fresh_epoch();
    let mut flush = cfg.flush_for(ctx.tier());
    let mut installed = false;

    if budget::terrain_runs(ctx.tier()) {
        match install_script(&conn, &cfg, epoch, flush).await {
            Ok(_) => installed = true,
            // KWin may simply not be up yet. The name-owner watch brings us back.
            Err(e) => tracing::warn!(error = %e, "terrain script not installed yet; waiting for KWin"),
        }
    }

    let mut shutdown = ctx.shutdown.clone();
    loop {
        tokio::select! {
            biased;

            _ = shutdown.wait() => break,

            // SPEC §3.1: a downgrade is applied immediately. The script's
            // coalescing interval is baked into the JS, so honouring it means
            // reinstalling — which is cheap, and tier changes are rare.
            Some(tier) = ctx.tier_changed() => {
                let want = cfg.flush_for(tier);
                let should_run = budget::terrain_runs(tier);
                if !should_run && installed {
                    tracing::info!(?tier, "unloading the terrain script");
                    for obs in decoder.retract_all() {
                        senses.route(obs);
                    }
                    uninstall_script(&conn).await;
                    installed = false;
                } else if should_run && (!installed || want != flush) {
                    flush = want;
                    epoch = script::fresh_epoch();
                    tracing::info!(?tier, flush_ms = flush, "terrain budget changed");
                    for obs in decoder.retract_all() {
                        senses.route(obs);
                    }
                    match install_script(&conn, &cfg, epoch, flush).await {
                        Ok(_) => installed = true,
                        Err(e) => {
                            installed = false;
                            tracing::warn!(error = %e, "terrain reinstall failed");
                        }
                    }
                }
            }

            Some(sig) = futures_util::StreamExt::next(&mut owner_changes) => {
                let args = match sig.args() { Ok(a) => a, Err(_) => continue };
                let new_owner: Option<&str> = args.new_owner().as_ref().map(|s| s.as_str());
                for obs in decoder.retract_all() {
                    senses.route(obs);
                }
                if new_owner.is_some() && budget::terrain_runs(ctx.tier()) {
                    stats.reconnects.fetch_add(1, Ordering::Relaxed);
                    epoch = script::fresh_epoch();
                    tracing::info!(epoch, "KWin came back; reinstalling the terrain script");
                    match install_script(&conn, &cfg, epoch, flush).await {
                        Ok(_) => installed = true,
                        Err(e) => {
                            installed = false;
                            tracing::warn!(error = %e, "reinstall failed");
                        }
                    }
                } else if new_owner.is_none() {
                    installed = false;
                    tracing::warn!("KWin went away; terrain retracted");
                }
            }

            Some(msg) = rx.recv() => {
                match msg {
                    Wire::Hello(json) => {
                        match serde_json::from_str::<serde_json::Value>(&json) {
                            Ok(v) => tracing::info!(hello = %v, "terrain script said hello"),
                            Err(e) => tracing::warn!(error = %e, "malformed hello"),
                        }
                    }
                    Wire::Batch(json) => {
                        let now = clock.now();
                        match decoder.decode_str(&json) {
                            Ok(obs) => {
                                let n = stats.batches.fetch_add(1, Ordering::Relaxed);
                                if n == 0 {
                                    stats.first_batch_ms.store(now, Ordering::Relaxed);
                                }
                                stats.last_batch_ms.store(now, Ordering::Relaxed);
                                for o in obs {
                                    match o {
                                        Observation::Focus { .. } => {
                                            stats.focus_changes.fetch_add(1, Ordering::Relaxed);
                                        }
                                        Observation::Window { .. } => {
                                            stats.window_updates.fetch_add(1, Ordering::Relaxed);
                                        }
                                        _ => {}
                                    }
                                    senses.route(o);
                                }
                            }
                            Err(e) => tracing::warn!(error = %e, "bad terrain batch"),
                        }
                    }
                }
            }
        }
    }

    for obs in decoder.retract_all() {
        senses.route(obs);
    }
    if installed {
        uninstall_script(&conn).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consent::{ConsentLedger, Shutdown as _Shutdown};
    use crate::testing::TempConfig;
    use wisp_proto::{Event, EventKind};

    fn ledger(dir: &std::path::Path) -> (ConsentLedger, tokio::sync::broadcast::Receiver<Event>) {
        let (tx, rx) = tokio::sync::broadcast::channel(256);
        (ConsentLedger::load_from(dir, tx, crate::clock::Clock::new()), rx)
    }

    #[test]
    fn router_sends_each_observation_to_its_own_sense() {
        let tmp = TempConfig::new();
        let (l, mut rx) = ledger(tmp.path());
        let senses = TerrainSenses {
            focus: Some(l.grant::<ActiveWindowSense>().unwrap()),
            geometry: Some(l.grant::<WindowGeometrySense>().unwrap()),
        };
        senses.route(Observation::Focus { app_id: "kitty".into(), title: "t".into() });
        senses.route(Observation::Window { id: 1, x: 0, y: 0, w: 10, h: 10, gone: false });

        let a = rx.try_recv().unwrap();
        assert!(matches!(a.kind, EventKind::Sensed(Observation::Focus { .. })));
        let b = rx.try_recv().unwrap();
        assert!(matches!(b.kind, EventKind::Sensed(Observation::Window { .. })));

        assert_eq!(l.uses_today(SenseId::ActiveWindow), 1);
        assert_eq!(l.uses_today(SenseId::WindowGeometry), 1);
    }

    #[test]
    fn refusing_geometry_still_allows_focus() {
        let tmp = TempConfig::new();
        let (l, mut rx) = ledger(tmp.path());
        l.set_enabled(SenseId::WindowGeometry, false).unwrap();
        let senses = TerrainSenses {
            focus: Some(l.grant::<ActiveWindowSense>().unwrap()),
            geometry: l.grant::<WindowGeometrySense>().ok(),
        };
        assert!(senses.geometry.is_none(), "consent was withheld, no handle may exist");

        senses.route(Observation::Window { id: 1, x: 0, y: 0, w: 10, h: 10, gone: false });
        assert!(rx.try_recv().is_err(), "geometry leaked without consent");

        senses.route(Observation::Focus { app_id: "kitty".into(), title: "t".into() });
        assert!(matches!(
            rx.try_recv().unwrap().kind,
            EventKind::Sensed(Observation::Focus { .. })
        ));
        assert_eq!(l.uses_today(SenseId::WindowGeometry), 0);
    }

    #[test]
    fn empty_senses_is_detected() {
        assert!(TerrainSenses::default().is_empty());
    }

    #[test]
    fn stats_compute_a_rate() {
        let s = TerrainStats::default();
        s.batches.store(121, Ordering::Relaxed);
        s.first_batch_ms.store(1_000, Ordering::Relaxed);
        s.last_batch_ms.store(2_000, Ordering::Relaxed);
        assert!((s.batches_per_second() - 120.0).abs() < 0.001);
        // Not enough data must read as zero, not as infinity.
        let s2 = TerrainStats::default();
        assert_eq!(s2.batches_per_second(), 0.0);
    }

    /// End to end without a compositor: replay a captured session through the
    /// decoder and the consent router and assert on what reached the bus.
    #[test]
    fn a_captured_session_reaches_the_bus_intact() {
        let tmp = TempConfig::new();
        let (l, mut rx) = ledger(tmp.path());
        let senses = TerrainSenses {
            focus: Some(l.grant::<ActiveWindowSense>().unwrap()),
            geometry: Some(l.grant::<WindowGeometrySense>().unwrap()),
        };
        let mut d = TerrainDecoder::new();
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/kwin");
        for f in ["batch_resync.json", "batch_focus_change.json", "batch_close.json"] {
            let json = std::fs::read_to_string(format!("{dir}/{f}")).unwrap();
            for o in d.decode_str(&json).unwrap() {
                senses.route(o);
            }
        }
        let mut focus = 0;
        let mut windows = 0;
        while let Ok(ev) = rx.try_recv() {
            match ev.kind {
                EventKind::Sensed(Observation::Focus { .. }) => focus += 1,
                EventKind::Sensed(Observation::Window { .. }) => windows += 1,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(focus, 2, "initial focus plus one change");
        assert_eq!(windows, 5, "four windows, then one retraction");
        assert_eq!(l.uses_today(SenseId::ActiveWindow), 2);
        assert_eq!(l.uses_today(SenseId::WindowGeometry), 5);
    }

    #[allow(dead_code)]
    fn shutdown_type_is_reexported(_s: _Shutdown) {}
}
