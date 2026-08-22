//! **F12 — two-tier model management**, and the [`wisp_proto::Governed`] that
//! F13's VRAM budget acts through.
//!
//! > *A resident reflex model (~1.5–3 GiB) always in VRAM for routing,
//! > classification, one-liners; a deliberate model (~18 GiB Q4) loaded on
//! > demand for real conversation.*
//!
//! ## The three residencies, and why the middle one exists
//!
//! ```text
//!            load / rewarm                       ensure()
//!   Cold  ──────────────────▶  Warm  ──────────────────▶  Resident
//!     ▲                          ▲                            │
//!     │      cold_unload         │       warm_evict           │
//!     └──────────────────────────┴────────────────────────────┘
//! ```
//!
//! An 18 GiB Q4 MoE takes tens of seconds to read off disk. If T2 threw it away
//! completely, alt-tabbing out of a game and back would cost half a minute of
//! silence, and the governor would be a feature people turn off. So T2 drops the
//! *context* and the *GPU offload* — the expensive, contended resources — and
//! keeps the weights mmapped, where the page cache is holding them anyway.
//! Coming back is a re-offload, not a re-read.
//!
//! T3/T4 is different: [`Tier::may_hold_model`] is false, the card belongs to
//! whatever the operator started, and the memory goes back for real.
//!
//! ## Who decides what
//!
//! This module decides *nothing* about tiers. `wisp-gov` produces a
//! [`DeviceChoice`] (which card, or none) and a [`VramBudget`] (how much, and
//! whether somebody must free memory before returning to the event loop), and
//! this is the thing that obeys them. SPEC §3.1 says downgrades are synchronous:
//! [`ModelManager::set_tier`] does the eviction inline and returns only when the
//! memory is gone.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use wisp_gov::{DeviceChoice, GpuTarget, VramBudget};
use wisp_proto::{Cost, EventKind, Governed, Tier, TierReason};

use crate::backend::{Backend, LoadRequest, ModelHandle, Residency, Role, UnloadMode};
use crate::error::{MindError, Result};
use crate::events::EventSink;
use crate::models::{ModelEntry, ModelRegistry};

/// Everything the manager needs from config.
///
/// Field-for-field the same shape as `wisp::config::ModelSettings`, which was
/// written down before this crate existed so the operator's config file would
/// not have to change under them — with one addition, `embed`, which that struct
/// does not have yet. See the crate docs' list of API gaps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelSettings {
    pub models_dir: PathBuf,
    pub reflex: String,
    pub deliberate: String,
    pub embed: String,
    pub context_tokens: u32,
    /// `-1` is "as many layers as fit".
    pub gpu_layers: i32,
    pub temperature: f32,
    pub max_tokens: u32,
    /// SPEC §0.2a. Ships `false`.
    pub allow_downloads: bool,
    pub registry: PathBuf,
}

impl Default for ModelSettings {
    fn default() -> Self {
        ModelSettings {
            models_dir: crate::dirs::models_dir(),
            reflex: "reflex".to_string(),
            deliberate: "deliberate".to_string(),
            embed: "embed".to_string(),
            context_tokens: 4096,
            gpu_layers: -1,
            temperature: 0.7,
            max_tokens: 256,
            allow_downloads: false,
            registry: crate::dirs::models_dir().join("registry.json"),
        }
    }
}

impl ModelSettings {
    pub fn want(&self, role: Role) -> &str {
        match role {
            Role::Reflex => &self.reflex,
            Role::Deliberate => &self.deliberate,
            Role::Embed => &self.embed,
        }
    }
}

/// What one role's model is doing right now.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotState {
    pub role: Role,
    pub name: String,
    pub path: PathBuf,
    pub residency: Residency,
    pub vram_mib: u64,
    /// Whether the last load actually made it onto a GPU. `false` is a normal
    /// outcome — CPU inference at T2 on a laptop with a 512 MiB integrated card
    /// is the *correct* answer, not a degraded one.
    pub on_gpu: bool,
    /// What the last transition into [`Residency::Resident`] cost.
    pub last_load_ms: u64,
}

#[derive(Debug, Clone)]
struct Slot {
    entry: ModelEntry,
    handle: Option<ModelHandle>,
    residency: Residency,
    vram_mib: u64,
    on_gpu: bool,
    last_load_ms: u64,
}

/// The backend, shared.
///
/// It has to be: [`ModelManager`] loads and evicts, [`crate::memory`] embeds
/// through it, the tool registry's `recall` reaches it from inside an async
/// closure, and the escalation ladder decodes on it. One `Arc<Mutex<..>>` is
/// honest about what is really going on — inference is serialised anyway, since
/// there is one GPU and one operator — and it is what lets
/// [`crate::memory::embed::ModelEmbedder`] be `'static` and `Send` instead of
/// borrowing the manager for its lifetime.
pub type SharedBackend = Arc<Mutex<Box<dyn Backend>>>;

/// Lock, recovering from a poisoned mutex rather than propagating the panic.
/// A backend that panicked once must not take the governor down with it.
pub fn lock_backend(b: &SharedBackend) -> MutexGuard<'_, Box<dyn Backend>> {
    b.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct ModelManager {
    backend: SharedBackend,
    registry: ModelRegistry,
    settings: ModelSettings,
    slots: BTreeMap<Role, Slot>,
    tier: Tier,
    device: DeviceChoice,
    budget: VramBudget,
    events: EventSink,
}

impl std::fmt::Debug for ModelManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelManager")
            .field("backend", &lock_backend(&self.backend).name())
            .field("tier", &self.tier)
            .field("slots", &self.states())
            .finish()
    }
}

/// A choice that touches nothing. What the manager starts with, before
/// `wisp-gov` has said anything.
fn nothing_yet() -> DeviceChoice {
    DeviceChoice {
        render: None,
        inference: None,
        dgpu_untouched: true,
        note: "the governor has not chosen a device yet".to_string(),
    }
}

fn no_budget() -> VramBudget {
    VramBudget {
        dgpu_mib: 0,
        igpu_mib: 0,
        evict_now: false,
        note: "the governor has not set a budget yet".to_string(),
    }
}

impl ModelManager {
    pub fn new(backend: Box<dyn Backend>, registry: ModelRegistry, settings: ModelSettings) -> Self {
        ModelManager::shared(Arc::new(Mutex::new(backend)), registry, settings)
    }

    pub fn shared(
        backend: SharedBackend,
        registry: ModelRegistry,
        settings: ModelSettings,
    ) -> Self {
        ModelManager {
            backend,
            registry,
            settings,
            slots: BTreeMap::new(),
            // Nothing may be assumed about the machine before the governor has
            // looked at it, and T1 is the tier that assumes the least while
            // still letting her exist.
            tier: Tier::Full,
            device: nothing_yet(),
            budget: no_budget(),
            events: EventSink::silent(),
        }
    }

    pub fn with_events(mut self, events: EventSink) -> Self {
        self.events = events;
        self
    }

    /// A handle to the backend, for everything that is not loading or
    /// evicting.
    pub fn backend(&self) -> SharedBackend {
        Arc::clone(&self.backend)
    }

    /// Do something with the backend, holding the lock for exactly that long.
    pub fn with_backend<R>(&self, f: impl FnOnce(&mut dyn Backend) -> R) -> R {
        let mut g = lock_backend(&self.backend);
        f(g.as_mut())
    }

    pub fn backend_name(&self) -> &'static str {
        lock_backend(&self.backend).name()
    }
    pub fn registry(&self) -> &ModelRegistry {
        &self.registry
    }
    pub fn settings(&self) -> &ModelSettings {
        &self.settings
    }
    pub fn tier(&self) -> Tier {
        self.tier
    }
    pub fn device_choice(&self) -> &DeviceChoice {
        &self.device
    }
    pub fn budget(&self) -> &VramBudget {
        &self.budget
    }

    /// What the governor decided. Applied immediately, because
    /// [`VramBudget::evict_now`] means somebody has to free memory *before*
    /// returning to the event loop.
    pub fn apply(&mut self, device: DeviceChoice, budget: VramBudget) {
        let must_evict = budget.evict_now;
        self.device = device;
        self.budget = budget;
        if must_evict {
            self.enforce_budget();
        }
    }

    pub fn residency(&self, role: Role) -> Residency {
        self.slots
            .get(&role)
            .map(|s| s.residency)
            .unwrap_or(Residency::Cold)
    }

    pub fn handle(&self, role: Role) -> Option<ModelHandle> {
        self.slots
            .get(&role)
            .filter(|s| s.residency.is_loaded())
            .and_then(|s| s.handle)
    }

    pub fn states(&self) -> Vec<SlotState> {
        self.slots
            .values()
            .map(|s| SlotState {
                role: s.entry.role,
                name: s.entry.name.clone(),
                path: s.entry.local_path(&self.settings.models_dir),
                residency: s.residency,
                vram_mib: s.vram_mib,
                on_gpu: s.on_gpu,
                last_load_ms: s.last_load_ms,
            })
            .collect()
    }

    /// Total VRAM she is holding on the discrete card. The number the cost
    /// meter reports and the one `evict_now` has to be able to drive to zero.
    pub fn vram_held_mib(&self) -> u64 {
        self.slots
            .values()
            .filter(|s| s.on_gpu)
            .map(|s| s.vram_mib)
            .sum()
    }

    /// Is this role's model actually on disk? The question "is this rung
    /// available" (F17) has to mean *could be loaded*, not *is named in the
    /// registry* — a rung that would fail is not a rung.
    pub fn can_load(&self, role: Role) -> bool {
        self.entry_for(role)
            .map(|e| e.local_path(&self.settings.models_dir).is_file())
            .unwrap_or(false)
    }

    /// Which registry entry a role resolves to, without loading anything.
    pub fn entry_for(&self, role: Role) -> Result<&ModelEntry> {
        self.registry.resolve(role, self.settings.want(role))
    }

    // --- the state machine -------------------------------------------------

    /// Get this role loaded and ready to decode.
    ///
    /// Never blocks on a download: a model that is not on disk is
    /// [`MindError::ModelMissing`], and fetching it is [`crate::fetch`]'s job
    /// and the operator's decision.
    pub fn ensure(&mut self, role: Role) -> Result<ModelHandle> {
        if !self.tier.may_hold_model() {
            return Err(MindError::NotAllowedAtTier { tier: self.tier });
        }
        // Already there.
        if let Some(s) = self.slots.get(&role) {
            if s.residency.is_loaded() {
                if let Some(h) = s.handle {
                    return Ok(h);
                }
            }
        }

        let entry = self.entry_for(role)?.clone();
        let path = entry.local_path(&self.settings.models_dir);
        if !path.is_file() {
            return Err(MindError::ModelMissing {
                name: entry.name.clone(),
                path,
            });
        }

        let (device, gpu_layers) = self.placement(&entry);

        // Warm: the weights are still mapped, so this is a re-offload.
        if let Some(s) = self.slots.get(&role) {
            if s.residency == Residency::Warm {
                if let Some(h) = s.handle {
                    let loaded = lock_backend(&self.backend).rewarm(h, device.as_ref())?;
                    let slot = self.slots.get_mut(&role).expect("checked above");
                    slot.residency = Residency::Resident;
                    slot.vram_mib = loaded.vram_mib;
                    slot.on_gpu = device.is_some();
                    slot.last_load_ms = loaded.took_ms;
                    self.events.emit(EventKind::Model {
                        name: entry.name.clone(),
                        loaded: true,
                        vram_mib: loaded.vram_mib,
                    });
                    return Ok(h);
                }
            }
        }

        let mut req = LoadRequest::new(&entry.name, &path, role).context(self.context_for(&entry));
        req.gpu_layers = gpu_layers;
        req.vram_budget_mib = device
            .as_ref()
            .map(|d| d.vram_budget_mib)
            .unwrap_or(u64::MAX);
        req.device = device.clone();
        req.embedding = role == Role::Embed;

        let loaded = lock_backend(&self.backend).load(&req)?;
        self.slots.insert(
            role,
            Slot {
                entry: entry.clone(),
                handle: Some(loaded.handle),
                residency: Residency::Resident,
                vram_mib: loaded.vram_mib,
                on_gpu: device.is_some(),
                last_load_ms: loaded.took_ms,
            },
        );
        self.events.emit(EventKind::Model {
            name: entry.name,
            loaded: true,
            vram_mib: loaded.vram_mib,
        });
        Ok(loaded.handle)
    }

    /// Drop the context and the offload; keep the weights mapped. T2's move.
    pub fn warm_evict(&mut self, role: Role) {
        self.evict(role, UnloadMode::Warm)
    }

    /// Give everything back. T3/T4's move.
    pub fn cold_unload(&mut self, role: Role) {
        self.evict(role, UnloadMode::Cold)
    }

    fn evict(&mut self, role: Role, mode: UnloadMode) {
        let Some(slot) = self.slots.get(&role) else {
            return;
        };
        if slot.residency == Residency::Cold {
            return;
        }
        if mode == UnloadMode::Warm && slot.residency == Residency::Warm {
            return;
        }
        let Some(handle) = slot.handle else { return };
        let name = slot.entry.name.clone();

        // Downgrades are infallible (SPEC §3.1). A backend that cannot unload
        // is a bug worth a loud log, never a reason to leave the governor
        // hanging or to propagate an error into a path that has no `?`.
        let residency = {
            let mut b = lock_backend(&self.backend);
            if let Err(e) = b.unload(handle, mode) {
                tracing::error!(model = %name, ?mode, error = %e, "unload failed");
            }
            b.residency(handle)
        };
        let slot = self.slots.get_mut(&role).expect("checked above");
        slot.residency = residency;
        slot.vram_mib = 0;
        slot.on_gpu = false;
        self.events.emit(EventKind::Model {
            name,
            loaded: false,
            vram_mib: 0,
        });
    }

    /// Bring everything the tier allows back, cheapest first. Upgrades are lazy
    /// (SPEC §3.1), so nothing calls this automatically — it exists for
    /// "she just came back from a game and the next question should be fast".
    pub fn prewarm(&mut self, roles: &[Role]) -> Vec<(Role, Result<ModelHandle>)> {
        roles.iter().map(|r| (*r, self.ensure(*r))).collect()
    }

    // --- placement ---------------------------------------------------------

    /// Which card, and how many layers, given the governor's budget.
    ///
    /// If the model does not fit in what she is allowed, she does *not* fail
    /// and does not quietly exceed the budget: she runs on the CPU. That is the
    /// case `wisp_gov::device`'s own docs describe — "the reflex model stays on
    /// the CPU rather than thrashing a 512 MiB carve-out".
    fn placement(&self, entry: &ModelEntry) -> (Option<GpuTarget>, i32) {
        let Some(target) = self.device.inference.clone() else {
            return (None, 0);
        };
        let ceiling = target
            .vram_budget_mib
            .min(self.budget.for_kind(target.kind))
            .saturating_sub(self.vram_held_mib());
        if entry.vram_mib == 0 || entry.vram_mib <= ceiling {
            return (Some(target), self.settings.gpu_layers);
        }
        tracing::info!(
            model = %entry.name,
            wants_mib = entry.vram_mib,
            ceiling_mib = ceiling,
            "does not fit the governor's allowance; running on the CPU instead"
        );
        (None, 0)
    }

    fn context_for(&self, entry: &ModelEntry) -> u32 {
        let want = self.settings.context_tokens.max(512);
        match entry.context_max {
            0 => want,
            max => want.min(max),
        }
    }

    /// Evict until what she holds fits the budget, most expendable first.
    fn enforce_budget(&mut self) {
        let allowance = self.budget.dgpu_mib.max(self.budget.igpu_mib);
        if allowance == 0 {
            for role in eviction_order() {
                self.evict(role, UnloadMode::Warm);
            }
            return;
        }
        for role in eviction_order() {
            if self.vram_held_mib() <= allowance {
                return;
            }
            self.evict(role, UnloadMode::Warm);
        }
    }
}

/// Most expendable first: the big one, then the embedder, then the reflex model
/// she needs to say anything at all.
fn eviction_order() -> Vec<Role> {
    let mut roles = Role::ALL.to_vec();
    roles.sort_by_key(|r| r.eviction_rank());
    roles
}

impl Governed for ModelManager {
    /// SPEC §3.1: must not block, must not fail.
    fn set_tier(&mut self, tier: Tier, reason: &TierReason) {
        let from = self.tier;
        self.tier = tier;
        if from == tier {
            return;
        }
        match tier {
            // T3/T4: `may_hold_model()` is false. Everything goes back, now.
            Tier::Lobotomised | Tier::Dormant => {
                for role in eviction_order() {
                    self.cold_unload(role);
                }
            }
            // T2: the deliberate model is evicted from VRAM but kept mmapped so
            // the way back is ~1 s. The embedder follows it if it was on the
            // card; the reflex model stays, inside the reduced allowance.
            Tier::Reduced => {
                self.warm_evict(Role::Deliberate);
                self.warm_evict(Role::Embed);
                self.enforce_budget();
            }
            // Upgrades are lazy: nothing is loaded here. The next `ensure` will
            // find a warm model and pay a second for it, and a tier that
            // bounced (alt-tab out of a game and straight back) will not have
            // cost a load at all.
            Tier::Full | Tier::Feral => {}
        }
        tracing::debug!(?from, to = ?tier, ?reason, held_mib = self.vram_held_mib(), "mind tier applied");
    }

    /// Worst case, for the cost meter. Deliberately the *worst* case: at T1 she
    /// is permitted to have the 30B resident, and the accounting has to say so
    /// even when she happens not to.
    fn cost_at(tier: Tier) -> Cost {
        match tier {
            // Background cognition permitted: everything resident, and
            // consolidation running.
            Tier::Feral => Cost {
                ram_mib: 900,
                vram_mib: 20_400,
                cpu_centi_pct: 900,
            },
            Tier::Full => Cost {
                ram_mib: 700,
                vram_mib: 20_400,
                cpu_centi_pct: 400,
            },
            // Reflex only, capped; the deliberate model's weights are still
            // mapped, which costs address space and page cache, not VRAM.
            Tier::Reduced => Cost {
                ram_mib: 500,
                vram_mib: 2_048,
                cpu_centi_pct: 150,
            },
            // Nothing on the card. What is left is the deferred queue and the
            // memory database's handle.
            Tier::Lobotomised => Cost {
                ram_mib: 90,
                vram_mib: 0,
                cpu_centi_pct: 30,
            },
            Tier::Dormant => Cost::FREE,
        }
    }
}

/// Does this look like a GGUF? Used before handing a path to llama.cpp, so a
/// truncated download reports as a bad file rather than as a segfault.
pub fn looks_like_gguf(path: impl AsRef<Path>) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic).is_ok() && &magic == b"GGUF"
}
