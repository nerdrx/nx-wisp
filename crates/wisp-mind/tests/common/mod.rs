//! Shared fixtures.
//!
//! **SPEC §4: `NX_WISP_CONFIG_DIR` must be set to a temp dir by every test.**
//! [`Fixture`] is the only way anything in this suite gets a config directory,
//! and it holds `wisp_mind::testing::Isolated` for its whole lifetime, so there
//! is no path by which a fixture writes into the operator's real memory. It has
//! bitten this suite before (NX Orbit, 2026-08-20).

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use wisp_mind::backend::{mock::MockBackend, Backend, Role};
use wisp_mind::events::{Collected, EventSink};
use wisp_mind::manager::{ModelManager, ModelSettings};
use wisp_mind::memory::{embed::HashEmbedder, Memory, WallClock};
use wisp_mind::mind::{Mind, MindBuilder};
use wisp_mind::models::ModelRegistry;
use wisp_mind::testing::Isolated;

pub const DAY_MS: i64 = 86_400_000;

pub struct Fixture {
    _tmp: tempfile::TempDir,
    _iso: Isolated,
    pub models_dir: PathBuf,
    pub registry: ModelRegistry,
    pub events: EventSink,
    pub log: Collected,
    pub clock: WallClock,
    pub clock_cell: Arc<std::sync::atomic::AtomicI64>,
}

impl Fixture {
    pub fn new() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let iso = Isolated::at(tmp.path());
        let models_dir = iso.models_dir();
        let (events, log) = EventSink::collector();
        let (clock, clock_cell) = WallClock::stepped(1_700_000_000_000);
        Fixture {
            _tmp: tmp,
            _iso: iso,
            models_dir,
            registry: ModelRegistry::builtin(),
            events,
            log,
            clock,
            clock_cell,
        }
    }

    /// Put a plausible-looking GGUF where the registry says the model lives.
    /// Content is irrelevant: the mock backend never reads it, and the real one
    /// is behind a cargo feature this suite does not enable.
    pub fn place(&self, role: Role) -> PathBuf {
        let e = self
            .registry
            .default_for(role)
            .expect("the built-in registry has a default for every role");
        let p = e.local_path(&self.models_dir);
        std::fs::create_dir_all(&self.models_dir).expect("models dir");
        std::fs::write(&p, b"GGUF\0not-really").expect("write model");
        p
    }

    pub fn place_all(&self) {
        for r in Role::ALL {
            self.place(r);
        }
    }

    pub fn settings(&self) -> ModelSettings {
        ModelSettings {
            models_dir: self.models_dir.clone(),
            context_tokens: 2048,
            ..ModelSettings::default()
        }
    }

    /// Advance the wall clock the memory store ages against.
    pub fn advance(&self, ms: i64) -> i64 {
        self.clock_cell
            .fetch_add(ms, std::sync::atomic::Ordering::Relaxed)
            + ms
    }

    pub fn now(&self) -> i64 {
        self.clock.now()
    }

    /// A backend that claims plausible sizes for the three registry models, so
    /// the VRAM budget has something to push against.
    pub fn backend(&self) -> MockBackend {
        let mut b = MockBackend::new();
        for r in Role::ALL {
            if let Some(e) = self.registry.default_for(r) {
                b = b.vram_hint(&e.name, e.vram_mib);
            }
        }
        b
    }

    pub fn manager(&self, backend: MockBackend) -> ModelManager {
        ModelManager::new(
            Box::new(backend) as Box<dyn Backend>,
            self.registry.clone(),
            self.settings(),
        )
        .with_events(self.events.clone())
    }

    pub fn mind(&self, backend: MockBackend) -> MindBuilder {
        Mind::builder(Box::new(backend) as Box<dyn Backend>)
            .registry(self.registry.clone())
            .settings(self.settings())
            .memory(Memory::in_memory().expect("memory"))
            .embedder(Box::new(HashEmbedder::default()))
            .clock(self.clock.clone())
            .events(self.events.clone())
            .tool_state_file(wisp_mind::dirs::config_dir().join("mind").join("tools.json"))
    }
}

impl Default for Fixture {
    fn default() -> Self {
        Fixture::new()
    }
}

/// The governor's answers, without a governor. Built from `wisp_gov::fakes`, so
/// nothing here hardcodes the operator's desktop.
pub mod gov {
    use wisp_gov::{fakes::Machine, DeviceChoice, GovConfig, VramBudget};
    use wisp_proto::Tier;

    /// Device choice and VRAM budget for a tier on a two-card desktop.
    pub fn desktop(tier: Tier, previous: Option<&VramBudget>) -> (DeviceChoice, VramBudget) {
        let snap = Machine::desktop().build();
        let cfg = GovConfig::default();
        let device = wisp_gov::device::select_for(tier, &snap, &cfg);
        let budget = wisp_gov::vram::budget(tier, &snap, &cfg, 0, previous);
        (device, budget)
    }

    /// The same on a laptop with a 6 GiB card and a 512 MiB integrated one.
    pub fn laptop(tier: Tier, previous: Option<&VramBudget>) -> (DeviceChoice, VramBudget) {
        let snap = Machine::laptop().build();
        let cfg = GovConfig::default();
        (
            wisp_gov::device::select_for(tier, &snap, &cfg),
            wisp_gov::vram::budget(tier, &snap, &cfg, 0, previous),
        )
    }
}
