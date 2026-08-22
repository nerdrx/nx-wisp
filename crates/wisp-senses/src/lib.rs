//! wisp-senses — see SPEC.md §2 for what this crate owns.
//!
//! Everything she notices about the machine, and the consent layer that decides
//! whether she is allowed to notice it.
//!
//! # Shape
//!
//! Each sense is a plugin: a zero-or-small-config type implementing [`Sense`]
//! (which fixes its `SenseId`, and therefore its `Consent`, at the type level)
//! and usually [`SensePlugin`] (which gives it a task to run). A sense reaches
//! the bus only through a [`SenseHandle`], and the only way to get one is
//! [`ConsentLedger::grant`], which refuses when consent is not satisfied. See
//! the [`consent`] module for why that is the whole design.
//!
//! # Platform
//!
//! Linux, Wayland, KDE Plasma 6, KWin ≥ 6.0 — permanently, per SPEC §1. There
//! are no cfg branches for anything else and none may be added. In particular:
//!
//! - Window information comes from a **KWin script over D-Bus** ([`kwin`]).
//!   Neither `zwlr_foreign_toplevel_manager_v1` nor `ext_foreign_toplevel_list_v1`
//!   is advertised by KWin 6.7.4, and neither carries geometry in any case.
//! - Idle comes from `ext_idle_notifier_v1`, clipboard from
//!   `ext_data_control_manager_v1`, both of which KWin does advertise.

pub mod audio;
pub mod budget;
pub mod clipboard;
pub mod clock;
pub mod consent;
pub mod files;
pub mod idle;
pub mod kwin;
pub mod media;
pub mod notifications;
pub mod vitals;
pub mod workspace;

#[cfg(test)]
pub(crate) mod testing;

pub use clock::Clock;
pub use consent::{
    ConsentError, ConsentLedger, ConsentRow, PublishError, Sense, SenseCtx, SenseHandle,
    SensePlugin, Shutdown, ShutdownSignal, ALL_SENSES, BUS_CAPACITY,
};

use std::sync::Arc;

use tokio::sync::broadcast;
use wisp_proto::{Cost, Event, Governed, SenseId, Tier, TierReason};

/// The senses, assembled. Owns the one bus of SPEC §3.2 and the consent ledger,
/// and starts each sense only if the operator has permitted it.
pub struct Senses {
    ledger: ConsentLedger,
    signal: ShutdownSignal,
    shutdown: Shutdown,
    /// The governor's verdict, published to every running sense. A `watch` so
    /// `set_tier` is synchronous, infallible and never blocks (SPEC §3.1).
    tier_tx: tokio::sync::watch::Sender<Tier>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    terrain_stats: Arc<kwin::TerrainStats>,
}

impl Senses {
    /// Load consent from `NX_WISP_CONFIG_DIR` (or XDG) and build the bus.
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(BUS_CAPACITY);
        Self::with_bus(tx, Clock::new())
    }

    pub fn with_bus(bus: broadcast::Sender<Event>, clock: Clock) -> Self {
        let ledger = ConsentLedger::load(bus, clock);
        let (signal, shutdown) = ShutdownSignal::new();
        let (tier_tx, _) = tokio::sync::watch::channel(Tier::Full);
        Senses {
            ledger,
            signal,
            shutdown,
            tier_tx,
            tasks: Vec::new(),
            terrain_stats: Arc::new(kwin::TerrainStats::default()),
        }
    }

    /// The context every sense is started with.
    fn ctx(&self) -> SenseCtx {
        SenseCtx::new(self.shutdown.clone(), self.tier_tx.subscribe())
    }

    pub fn tier(&self) -> Tier {
        *self.tier_tx.borrow()
    }

    pub fn ledger(&self) -> &ConsentLedger {
        &self.ledger
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.ledger.subscribe()
    }

    pub fn terrain_stats(&self) -> Arc<kwin::TerrainStats> {
        self.terrain_stats.clone()
    }

    /// Start one sense. Returns the consent error rather than panicking, so
    /// "the operator has this switched off" is an ordinary, expected outcome.
    pub fn start<S: SensePlugin>(&mut self, sense: S) -> Result<(), ConsentError> {
        let handle = self.ledger.grant::<S>()?;
        let ctx = self.ctx();
        self.tasks.push(sense.spawn(handle, ctx));
        Ok(())
    }

    /// Start a sense, logging and swallowing "not enabled". Convenience for the
    /// binary's start-up, where every ambient sense is attempted.
    pub fn try_start<S: SensePlugin>(&mut self, sense: S) {
        match self.start(sense) {
            Ok(()) => tracing::info!(sense = ?S::ID, "sense started"),
            Err(ConsentError::NotEnabled(id, c)) => {
                tracing::info!(sense = ?id, consent = ?c, "sense not enabled by the operator")
            }
        }
    }

    /// The terrain feed is special: one KWin script serves two senses, so it
    /// gets its own starter rather than going through [`Senses::start`].
    pub fn start_terrain(&mut self, cfg: kwin::TerrainConfig) {
        let senses = kwin::TerrainSenses {
            focus: self.ledger.grant::<kwin::ActiveWindowSense>().ok(),
            geometry: self.ledger.grant::<kwin::WindowGeometrySense>().ok(),
        };
        if senses.is_empty() {
            tracing::info!("neither window sense is enabled; KWin script not installed");
            return;
        }
        let stats = self.terrain_stats.clone();
        let ctx = self.ctx();
        self.tasks.push(tokio::spawn(async move {
            if let Err(e) = kwin::run(senses, cfg, stats, ctx).await {
                tracing::error!(error = %e, "terrain feed failed");
            }
        }));
    }

    /// Every ambient sense the operator has left switched on, plus any explicit
    /// or invasive one they have opted into.
    pub fn start_all(&mut self, cfg: &SensesConfig) {
        self.start_terrain(cfg.terrain.clone());
        self.try_start(idle::IdleSense::default());
        self.try_start(workspace::WorkspaceSense);
        self.try_start(media::MediaSense);
        self.try_start(notifications::NotificationSense);
        self.try_start(audio::AudioSense::default());
        self.try_start(vitals::VitalsSense::new(cfg.vitals.clone()));
        self.try_start(files::FilesSense::new(cfg.watch_dirs.clone()));
        self.try_start(clipboard::ClipboardSense);
    }

    /// Stop everything and wait for it. Invasive tells come down as the handles
    /// drop, so the character stops showing them before this returns.
    pub async fn shutdown(mut self) {
        self.signal.fire();
        for t in std::mem::take(&mut self.tasks) {
            let _ = t.await;
        }
    }
}

impl Default for Senses {
    fn default() -> Self {
        Self::new()
    }
}

/// SPEC §3.1. The senses are cheap by construction, but not free: the terrain
/// feed is the one that scales with what the operator is doing, so it is what
/// gets shed first.
impl Governed for Senses {
    fn set_tier(&mut self, tier: Tier, reason: &TierReason) {
        // Synchronous, infallible, non-blocking: a `watch` send never waits on a
        // receiver. Each sense picks the change up on its own loop and simply
        // looks less often — nothing here has a queue to defer work into, so a
        // downgrade always sheds (SPEC §3.1). The budgets are in `budget`.
        tracing::info!(?tier, ?reason, "senses tier");
        let _ = self.tier_tx.send(tier);
    }

    fn cost_at(tier: Tier) -> Cost {
        match tier {
            // Terrain at ~110 batches/s while a window is being dragged,
            // vitals every 5s, audio every 500ms.
            Tier::Feral | Tier::Full => Cost { ram_mib: 6, vram_mib: 0, cpu_centi_pct: 40 },
            // Terrain ~60/s, vitals every 15s.
            Tier::Reduced => Cost { ram_mib: 5, vram_mib: 0, cpu_centi_pct: 20 },
            // Terrain ~20/s, vitals every 30s.
            Tier::Lobotomised => Cost { ram_mib: 4, vram_mib: 0, cpu_centi_pct: 5 },
            // The KWin script is unloaded; only the event-driven senses remain,
            // and they cost nothing at all while the machine is quiet.
            Tier::Dormant => Cost { ram_mib: 4, vram_mib: 0, cpu_centi_pct: 0 },
        }
    }
}

/// Everything the operator can configure about the senses.
#[derive(Debug, Clone, Default)]
pub struct SensesConfig {
    pub terrain: kwin::TerrainConfig,
    pub vitals: vitals::VitalsConfig,
    /// Project directories she watches for F26.
    pub watch_dirs: Vec<std::path::PathBuf>,
}

/// The `SenseId`s this crate actually implements. `Microphone` and `Screen`
/// belong to `wisp-voice` and the eyes; their consent rows exist here (F30 owns
/// the whole panel) but nothing in this crate ever grants them.
pub const IMPLEMENTED: [SenseId; 9] = [
    SenseId::ActiveWindow,
    SenseId::WindowGeometry,
    SenseId::Workspace,
    SenseId::Idle,
    SenseId::Media,
    SenseId::Audio,
    SenseId::Notifications,
    SenseId::Vitals,
    SenseId::Clipboard,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempConfig;

    #[test]
    fn every_implemented_sense_has_a_consent_row() {
        for id in IMPLEMENTED {
            assert!(ALL_SENSES.contains(&id), "{id:?} has no consent panel row");
        }
    }

    #[test]
    fn the_panel_covers_every_sense_id_in_the_spec() {
        // If wisp-proto gains a SenseId, this fails until F30 grows a row for
        // it — which is the point. A sense the operator cannot see is a bug.
        assert_eq!(ALL_SENSES.len(), 12);
        let mut seen = std::collections::BTreeSet::new();
        for id in ALL_SENSES {
            assert!(seen.insert(format!("{id:?}")), "{id:?} listed twice");
        }
    }

    #[test]
    fn senses_start_with_ambient_on_and_invasive_off() {
        let _tmp = TempConfig::new();
        let s = Senses::new();
        assert!(s.ledger().is_enabled(SenseId::Vitals));
        assert!(!s.ledger().is_enabled(SenseId::Clipboard));
    }

    #[test]
    fn cost_never_rises_with_a_downgrade() {
        let tiers = [Tier::Feral, Tier::Full, Tier::Reduced, Tier::Lobotomised, Tier::Dormant];
        for w in tiers.windows(2) {
            let a = <Senses as Governed>::cost_at(w[0]);
            let b = <Senses as Governed>::cost_at(w[1]);
            assert!(b.cpu_centi_pct <= a.cpu_centi_pct, "{:?} -> {:?}", w[0], w[1]);
            assert!(b.ram_mib <= a.ram_mib);
        }
        assert_eq!(<Senses as Governed>::cost_at(Tier::Dormant).vram_mib, 0);
    }
}
