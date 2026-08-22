//! **F12 — the inference backend.**
//!
//! One trait, two implementations: [`llama::LlamaBackend`] over `llama-cpp-2`
//! built with `-DGGML_VULKAN=ON`, and [`mock::MockBackend`], which is
//! deterministic, scriptable, and needs neither a GPU nor a file on disk.
//!
//! SPEC §4: *a mock inference backend means the suite never needs a model or a
//! GPU.* That is not a convenience here, it is the load-bearing design
//! decision — the real backend is behind an off-by-default cargo feature, so
//! `cargo test -p wisp-mind` on a CI box with no Vulkan loader compiles and
//! passes without llama.cpp ever being built.
//!
//! ## Why the trait is synchronous
//!
//! Decoding is a blocking, compute-bound loop; wrapping it in `async fn` would
//! be a lie that costs a `Box::pin` per token. Streaming is expressed as a
//! callback that returns [`Flow`], so a caller that wants a `Stream` runs
//! [`Backend::generate`] on `spawn_blocking` and pushes chunks into an
//! `mpsc`. [`crate::stream`] does exactly that and is the only place in the
//! crate that knows about it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use wisp_gov::GpuTarget;

use crate::error::Result;

pub mod mock;

#[cfg(feature = "llama")]
pub mod llama;

/// A token id, in whatever vocabulary the loaded model uses. Deliberately not a
/// newtype over `u32`: llama.cpp's ids are signed and the negative values mean
/// something.
pub type Token = i32;

/// Which of the two tiers of model this is (F12), plus the embedding model that
/// serves memory (F18).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Always resident while she may hold a model at all. Routing,
    /// classification, one-liners, the escalation decision.
    Reflex,
    /// Loaded on demand for real conversation; the first thing evicted.
    Deliberate,
    /// Memory's embedder. Small, cheap, and the last thing to go, because
    /// without it recall degrades to lexical matching.
    Embed,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Reflex, Role::Deliberate, Role::Embed];

    /// Eviction order: the most expensive and least essential first.
    pub fn eviction_rank(self) -> u8 {
        match self {
            Role::Deliberate => 0,
            Role::Embed => 1,
            Role::Reflex => 2,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Reflex => "reflex",
            Role::Deliberate => "deliberate",
            Role::Embed => "embed",
        }
    }
}

/// A loaded model, from the backend's point of view. Opaque and `Copy` so the
/// manager can keep one in a struct without fighting the borrow checker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ModelHandle(pub u64);

/// One conversation's slice of the KV cache (F15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SlotId(pub u32);

impl SlotId {
    /// The slot the persona prefix lives in, and which is never evicted.
    pub const PERSONA: SlotId = SlotId(0);
}

/// How resident a model is right now. The middle state is the whole point of
/// F12: at T2 the deliberate model's *weights* stay mmapped, so coming back is
/// a page-cache walk rather than an 18 GiB read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Residency {
    /// Nothing is mapped. Coming back costs a full read from disk.
    Cold,
    /// Weights mmapped, no context, no VRAM. Coming back costs a re-offload.
    Warm,
    /// Loaded, offloaded, with a context. Ready to decode.
    Resident,
}

impl Residency {
    pub fn is_loaded(self) -> bool {
        matches!(self, Residency::Resident)
    }
}

/// Warm keeps the mmap; cold gives everything back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnloadMode {
    /// T2. Drop the context and the GPU offload, keep the weights mapped.
    Warm,
    /// T3/T4. `Tier::may_hold_model()` is false; give the memory back.
    Cold,
}

/// What a backend can do. Asked rather than assumed, so the manager can decide
/// whether embeddings come from a model or from [`crate::memory::HashEmbedder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub grammars: bool,
    pub embeddings: bool,
    /// Can it keep a KV prefix between calls (F15)?
    pub kv_reuse: bool,
    /// Can it be told which GPU to use (F61)?
    pub device_select: bool,
    pub warm_evict: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadRequest {
    /// Registry name, e.g. `qwen3-1.7b-q4km`.
    pub name: String,
    pub path: PathBuf,
    pub role: Role,
    pub context_tokens: u32,
    /// `-1` means "as many as fit", matching llama.cpp and
    /// `wisp::config::ModelSettings::gpu_layers`.
    pub gpu_layers: i32,
    /// Which card, from [`wisp_gov::device::select`]. `None` means CPU only —
    /// which is what T2 on a machine with a too-small integrated card means,
    /// not an error.
    pub device: Option<GpuTarget>,
    /// The governor's ceiling for this card right now (F13).
    pub vram_budget_mib: u64,
    /// Load in embedding mode (pooled output, no causal mask).
    pub embedding: bool,
    /// Keep the weights mmapped. Always true except in tests that want to prove
    /// the cold path.
    pub mmap: bool,
    pub seed: u64,
}

impl LoadRequest {
    pub fn new(name: impl Into<String>, path: impl Into<PathBuf>, role: Role) -> Self {
        LoadRequest {
            name: name.into(),
            path: path.into(),
            role,
            context_tokens: 4096,
            gpu_layers: -1,
            device: None,
            vram_budget_mib: u64::MAX,
            embedding: role == Role::Embed,
            mmap: true,
            seed: 0x5157_5350, // "WSP"
        }
    }
    pub fn context(mut self, n: u32) -> Self {
        self.context_tokens = n;
        self
    }
    pub fn on(mut self, device: Option<GpuTarget>) -> Self {
        self.vram_budget_mib = device
            .as_ref()
            .map(|d| d.vram_budget_mib)
            .unwrap_or(u64::MAX);
        self.device = device;
        self
    }
}

/// What came back from a load.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub handle: ModelHandle,
    pub name: String,
    pub role: Role,
    pub residency: Residency,
    /// What it actually took on the card, for [`wisp_proto::EventKind::Model`].
    pub vram_mib: u64,
    pub ram_mib: u64,
    pub n_ctx: u32,
    pub n_vocab: u32,
    pub n_embd: u32,
    /// Milliseconds the load took. The claim F12 makes — warm is ~1 s, cold is
    /// tens of seconds — is measured, not asserted.
    pub took_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sampling {
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: u32,
    pub repeat_penalty: f32,
    pub seed: u64,
}

impl Default for Sampling {
    fn default() -> Self {
        Sampling {
            temperature: 0.7,
            top_p: 0.95,
            top_k: 40,
            repeat_penalty: 1.1,
            seed: 0,
        }
    }
}

impl Sampling {
    /// Zero temperature: for classification and tool routing, where a different
    /// answer on a rerun would be a bug rather than personality.
    pub const DETERMINISTIC: Sampling = Sampling {
        temperature: 0.0,
        top_p: 1.0,
        top_k: 1,
        repeat_penalty: 1.0,
        seed: 1,
    };
}

#[derive(Debug, Clone, PartialEq)]
pub struct GenRequest {
    pub prompt: String,
    /// Pre-tokenised prompt, when the caller already has it (the KV planner
    /// does). Saves a second tokenisation of a 2000-token persona prefix.
    pub prompt_tokens: Option<Vec<Token>>,
    pub max_tokens: u32,
    pub sampling: Sampling,
    /// **F14.** GBNF from [`crate::grammar`]. When this is `Some`, the decoder
    /// is constrained and malformed output is not merely unlikely, it is
    /// unreachable.
    pub grammar: Option<String>,
    pub stop: Vec<String>,
    pub slot: SlotId,
}

impl GenRequest {
    pub fn new(prompt: impl Into<String>) -> Self {
        GenRequest {
            prompt: prompt.into(),
            prompt_tokens: None,
            max_tokens: 256,
            sampling: Sampling::default(),
            grammar: None,
            stop: Vec::new(),
            slot: SlotId(1),
        }
    }
    pub fn max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }
    pub fn grammar(mut self, g: impl Into<String>) -> Self {
        self.grammar = Some(g.into());
        self
    }
    pub fn slot(mut self, s: SlotId) -> Self {
        self.slot = s;
        self
    }
    pub fn sampling(mut self, s: Sampling) -> Self {
        self.sampling = s;
        self
    }
}

/// One decoded token on its way out.
#[derive(Debug, Clone, PartialEq)]
pub struct GenChunk<'a> {
    pub text: &'a str,
    pub token: Token,
    pub index: u32,
}

/// What the sink says back. A caller that has heard enough — because the tier
/// dropped, or because the operator started talking — returns [`Flow::Stop`]
/// and decoding ends on the next token boundary rather than at `max_tokens`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Eos,
    MaxTokens,
    StopSequence,
    /// The sink asked to stop.
    Cancelled,
    /// The grammar had nowhere left to go — a well-formed end.
    GrammarComplete,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Generated {
    pub text: String,
    pub prompt_tokens: u32,
    /// **F15's number.** How much of the prompt was already in the slot's KV
    /// cache and did not have to be prefilled. For the persona prefix this
    /// should be everything, every time after the first.
    pub reused_prefix_tokens: u32,
    pub generated_tokens: u32,
    pub stopped: StopReason,
    pub prefill_ms: u64,
    pub decode_ms: u64,
}

impl Generated {
    /// Fraction of the prompt that came free out of the cache. The headline
    /// F15 metric.
    pub fn prefix_hit_rate(&self) -> f32 {
        if self.prompt_tokens == 0 {
            return 0.0;
        }
        self.reused_prefix_tokens as f32 / self.prompt_tokens as f32
    }
}

/// The inference backend.
///
/// Object-safe on purpose: [`crate::Mind`] holds a `Box<dyn Backend>` and the
/// tests swap a [`mock::MockBackend`] in without a generic parameter leaking
/// through every type in the crate.
pub trait Backend: Send {
    fn name(&self) -> &'static str;
    fn caps(&self) -> Caps;

    fn load(&mut self, req: &LoadRequest) -> Result<Loaded>;
    /// Warm eviction must be cheap and must keep the mapping; cold must give
    /// the memory back. A backend that cannot tell the difference reports
    /// [`Caps::warm_evict`] `false` and treats both as cold.
    fn unload(&mut self, handle: ModelHandle, mode: UnloadMode) -> Result<()>;
    /// Bring a warm model back to [`Residency::Resident`]. Cheap by
    /// construction: the weights never left the page cache.
    fn rewarm(&mut self, handle: ModelHandle, device: Option<&GpuTarget>) -> Result<Loaded>;
    fn residency(&self, handle: ModelHandle) -> Residency;
    fn vram_mib(&self, handle: ModelHandle) -> u64;

    fn tokenize(&self, handle: ModelHandle, text: &str, add_special: bool) -> Result<Vec<Token>>;
    fn detokenize(&self, handle: ModelHandle, tokens: &[Token]) -> Result<String>;

    fn generate(
        &mut self,
        handle: ModelHandle,
        req: &GenRequest,
        sink: &mut dyn FnMut(GenChunk<'_>) -> Flow,
    ) -> Result<Generated>;

    fn embed(&mut self, handle: ModelHandle, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// Forget everything cached for a conversation slot.
    fn kv_clear(&mut self, handle: ModelHandle, slot: SlotId) -> Result<()>;
    /// How many tokens the backend currently holds for `slot`.
    fn kv_tokens(&self, handle: ModelHandle, slot: SlotId) -> u32;

    /// Which `(vendor, device)` pairs the backend enumerated, in **its** order,
    /// for [`wisp_gov::GpuTarget::index_in`]. Empty when there is no GPU, which
    /// is the ordinary CI case and not an error.
    fn enumerated_devices(&self) -> Vec<(u32, u32)> {
        Vec::new()
    }
}

/// Convenience: run to completion and collect the text.
pub fn generate_all(
    backend: &mut dyn Backend,
    handle: ModelHandle,
    req: &GenRequest,
) -> Result<Generated> {
    backend.generate(handle, req, &mut |_| Flow::Continue)
}
