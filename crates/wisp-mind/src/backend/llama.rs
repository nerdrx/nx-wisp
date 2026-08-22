//! The real backend: `llama-cpp-2`, built with `-DGGML_VULKAN=ON`.
//!
//! Behind the `llama` cargo feature, off by default, because SPEC §4 requires
//! `cargo test -p wisp-mind` to pass on a machine with no GPU, no Vulkan loader
//! and no model — and building llama.cpp is none of those things. What ships is
//! the `vulkan` feature, which turns this on *and* pins the ggml backend:
//! **there is no ROCm on the target machine and there never will be** (SPEC §1,
//! plan §0).
//!
//! ## What "warm eviction" means here (F12)
//!
//! llama.cpp decides how many layers to offload at *model load* time, so there
//! is no API for "give the VRAM back but keep the weights". [`UnloadMode::Warm`]
//! therefore drops the context and reloads the model with `n_gpu_layers = 0`.
//! That sounds expensive and is not: the weights are `mmap`ed, the pages are
//! still resident in the page cache, and keeping a CPU-side mapping alive is
//! what *stops* them being evicted. Measured cost is a page-table walk rather
//! than an 18 GiB read — which is the whole difference F12 is claiming, and
//! [`Loaded::took_ms`] reports it rather than asserting it.
//!
//! [`UnloadMode::Cold`] drops both. T3 means the card belongs to whatever the
//! operator started, and holding a mapping open would keep 18 GiB of page cache
//! that a game would rather have.
//!
//! ## Self-reference
//!
//! `LlamaContext<'a>` borrows its `LlamaModel`, so a struct holding both is
//! self-referential. Rather than take a dependency on `ouroboros`, the model is
//! boxed (a stable address) and the context's lifetime is transmuted to
//! `'static`. The safety argument is in [`Slot`], and it rests on one thing the
//! compiler *can* check: field declaration order is drop order, so the context
//! is always destroyed before the model it points at.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::{Arc, OnceLock};

use llama_cpp_2::context::params::{LlamaContextParams, LlamaPoolingType};
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::llama_backend::LlamaBackend as LlamaGlobal;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
#[allow(deprecated)]
use llama_cpp_2::model::{AddBos, LlamaModel, Special};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::token::LlamaToken;
use llama_cpp_2::{LlamaBackendDevice, LlamaBackendDeviceType};
use wisp_gov::{GpuTarget, GpuKind};

use crate::backend::{
    Backend, Caps, Flow, GenChunk, GenRequest, Generated, LoadRequest, Loaded, ModelHandle,
    Residency, Role, SlotId, StopReason, Token, UnloadMode,
};
use crate::error::{MindError, Result};

/// `llama_backend_init` is process-global and must happen exactly once.
fn global() -> &'static Arc<LlamaGlobal> {
    static G: OnceLock<Arc<LlamaGlobal>> = OnceLock::new();
    G.get_or_init(|| {
        let mut b = LlamaGlobal::init().expect("llama_backend_init");
        // llama.cpp is chatty on stderr, and SPEC §3.4 says nothing in this
        // crate reaches the operator. Its logs go through `tracing` or nowhere
        // — except when somebody is debugging a backend problem and asks.
        if std::env::var_os("NX_WISP_LLAMA_LOGS").is_none() {
            b.void_logs();
        }
        Arc::new(b)
    })
}

/// One loaded model.
///
/// # Safety
///
/// `ctx` holds a `LlamaContext<'static>` whose true lifetime is that of
/// `model`. Three things keep that sound, and all three are structural rather
/// than remembered:
///
/// 1. `model` is a `Box`, so its address does not change when the `Slot` moves.
/// 2. `ctx` is declared **before** `model`, and Rust drops struct fields in
///    declaration order, so the borrow always ends before the borrowee.
/// 3. Nothing hands a `&LlamaContext` out past a method call, so no caller can
///    observe the forged lifetime.
///
/// Every path that replaces `model` (the warm/cold transitions) sets `ctx` to
/// `None` first.
struct Slot {
    ctx: Option<LlamaContext<'static>>,
    model: Option<Box<LlamaModel>>,
    name: String,
    role: Role,
    residency: Residency,
    path: std::path::PathBuf,
    n_ctx: u32,
    gpu_layers: i32,
    embedding: bool,
    /// The card it is on, so a rewarm goes back to the same one.
    device: Option<GpuTarget>,
    vram_mib: u64,
    /// What each conversation sequence currently holds, for F15's prefix
    /// arithmetic. Keyed by [`SlotId`], which is used directly as llama.cpp's
    /// `seq_id`.
    seqs: BTreeMap<SlotId, Vec<LlamaToken>>,
}

impl Slot {
    fn drop_context(&mut self) {
        self.ctx = None;
    }
}

pub struct LlamaBackend {
    slots: BTreeMap<ModelHandle, Slot>,
    by_name: BTreeMap<String, ModelHandle>,
    next: u64,
    threads: i32,
}

impl Default for LlamaBackend {
    fn default() -> Self {
        LlamaBackend::new()
    }
}

impl std::fmt::Debug for LlamaBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlamaBackend")
            .field(
                "slots",
                &self
                    .slots
                    .values()
                    .map(|s| (&s.name, s.residency))
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl LlamaBackend {
    pub fn new() -> Self {
        // Force initialisation here rather than lazily inside `load`, so a
        // machine with no usable ggml backend fails while somebody is looking.
        let _ = global();
        LlamaBackend {
            slots: BTreeMap::new(),
            by_name: BTreeMap::new(),
            next: 1,
            // Leave one core for the compositor and the rig. She is a
            // companion, not a batch job (SPEC §0.1).
            threads: (std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .saturating_sub(1))
            .max(1) as i32,
        }
    }

    pub fn with_threads(mut self, n: i32) -> Self {
        self.threads = n.max(1);
        self
    }

    /// What ggml actually found. Empty on a machine with no GPU backend, which
    /// is a fact rather than an error.
    pub fn devices() -> Vec<LlamaBackendDevice> {
        let _ = global();
        llama_cpp_2::list_llama_ggml_backend_devices()
    }

    /// Match `wisp-gov`'s choice of card against ggml's device list.
    ///
    /// **API gap.** [`GpuTarget::index_in`] wants the `(vendor, device)` PCI
    /// pairs the backend enumerated, and ggml does not expose those — a device
    /// reports a name (`"Vulkan0"`), a description (`"AMD Radeon RX 7900 XTX
    /// (RADV NAVI31)"`), a type and a memory size, and nothing that maps to PCI
    /// ids. So the match is on **kind first, then closest total memory**, which
    /// separates a 24 GiB discrete card from a 2 GiB integrated one without
    /// ambiguity on any machine that has both. `GpuTarget::env_hints()` (which
    /// sets `MESA_VK_DEVICE_SELECT`) is the belt-and-braces path and is applied
    /// by the binary before the process touches Vulkan.
    pub fn device_index_for(target: &GpuTarget, devices: &[LlamaBackendDevice]) -> Option<usize> {
        let want_integrated = target.kind == GpuKind::Integrated;
        let wanted_bytes = target.vram_total_mib.saturating_mul(1024 * 1024);
        devices
            .iter()
            .filter(|d| {
                matches!(
                    d.device_type,
                    LlamaBackendDeviceType::Gpu | LlamaBackendDeviceType::IntegratedGpu
                )
            })
            .filter(|d| (d.device_type == LlamaBackendDeviceType::IntegratedGpu) == want_integrated)
            .min_by_key(|d| (d.memory_total as i64 - wanted_bytes as i64).unsigned_abs())
            .or_else(|| {
                // No card of the right kind. Rather than silently use the wrong
                // one — which at T2 would mean running on the card a game is
                // using — fall back only when there is exactly one candidate.
                let gpus: Vec<_> = devices
                    .iter()
                    .filter(|d| {
                        matches!(
                            d.device_type,
                            LlamaBackendDeviceType::Gpu | LlamaBackendDeviceType::IntegratedGpu
                        )
                    })
                    .collect();
                if gpus.len() == 1 {
                    Some(gpus[0])
                } else {
                    None
                }
            })
            .map(|d| d.index)
    }

    fn slot(&self, h: ModelHandle) -> Result<&Slot> {
        self.slots
            .get(&h)
            .ok_or_else(|| MindError::Inference(format!("no such handle {}", h.0)))
    }

    fn slot_mut(&mut self, h: ModelHandle) -> Result<&mut Slot> {
        self.slots
            .get_mut(&h)
            .ok_or_else(|| MindError::Inference(format!("no such handle {}", h.0)))
    }

    /// Load the weights and build a context. The one place that does the
    /// lifetime forgery.
    fn open(
        name: &str,
        path: &std::path::Path,
        gpu_layers: i32,
        device: Option<&GpuTarget>,
        n_ctx: u32,
        embedding: bool,
        threads: i32,
    ) -> Result<(Box<LlamaModel>, LlamaContext<'static>, u64)> {
        if !crate::manager::looks_like_gguf(path) {
            return Err(MindError::LoadFailed {
                backend: "llama.cpp",
                name: name.to_string(),
                why: format!("{} is not a GGUF file", path.display()),
            });
        }

        let mut params = LlamaModelParams::default().with_use_mmap(true);
        let layers = if gpu_layers < 0 { u32::MAX } else { gpu_layers as u32 };
        params = params.with_n_gpu_layers(layers);
        let mut vram_mib = 0;
        if let Some(t) = device {
            if layers > 0 {
                let devices = Self::devices();
                match Self::device_index_for(t, &devices) {
                    Some(i) => {
                        params = params.with_devices(&[i]).map_err(|e| MindError::LoadFailed {
                            backend: "llama.cpp",
                            name: name.to_string(),
                            why: format!("ggml device {i}: {e}"),
                        })?;
                        vram_mib = t.vram_budget_mib;
                    }
                    None => {
                        // The governor picked a card ggml cannot see. Running
                        // on the CPU is the honest answer; running on whatever
                        // card happens to be first is not.
                        tracing::warn!(
                            model = name,
                            pci = %t.pci_slot,
                            "ggml does not expose the card the governor chose; staying on the CPU"
                        );
                        params = params.with_n_gpu_layers(0);
                    }
                }
            }
        } else {
            params = params.with_n_gpu_layers(0);
        }

        let model = LlamaModel::load_from_file(global(), path, &params).map_err(|e| {
            MindError::LoadFailed {
                backend: "llama.cpp",
                name: name.to_string(),
                why: e.to_string(),
            }
        })?;
        let model = Box::new(model);

        let n_ctx = n_ctx.max(256).min(model.n_ctx_train().max(256));
        let mut cparams = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(n_ctx))
            .with_n_batch(n_ctx.min(2048))
            .with_n_threads(threads)
            .with_n_threads_batch(threads)
            // F15: one sequence per conversation slot, plus the persona slot.
            .with_n_seq_max(8)
            // Without this llama.cpp divides `n_ctx` evenly between the
            // sequences, so eight slots would give each conversation an eighth
            // of the context and a persona prompt would not fit in its own
            // cache. Unified means the sequences share one pool, which is what
            // the KV planner in `crate::kv` already assumes.
            .with_kv_unified(true);
        if embedding {
            cparams = cparams
                .with_embeddings(true)
                .with_pooling_type(LlamaPoolingType::Mean);
        }

        // SAFETY: see `Slot`. The context borrows `*model`, whose address is
        // stable because it is boxed and never moved out of the slot, and the
        // slot drops `ctx` before `model`.
        let ctx = {
            let model_ref: &LlamaModel = &model;
            let ctx = model_ref
                .new_context(global(), cparams)
                .map_err(|e| MindError::LoadFailed {
                    backend: "llama.cpp",
                    name: name.to_string(),
                    why: e.to_string(),
                })?;
            unsafe { std::mem::transmute::<LlamaContext<'_>, LlamaContext<'static>>(ctx) }
        };
        Ok((model, ctx, vram_mib))
    }

    fn sampler(&self, slot: &Slot, req: &GenRequest) -> Result<LlamaSampler> {
        let model = slot
            .model
            .as_deref()
            .ok_or(MindError::NotLoaded(slot.role))?;
        let mut chain = Vec::new();
        // **F14.** The grammar goes first, so every later sampler only ever
        // sees tokens the grammar still permits. This is the difference between
        // constraining the decoder and hoping about the output.
        if let Some(gbnf) = &req.grammar {
            chain.push(
                LlamaSampler::grammar(model, gbnf, "root")
                    .map_err(|e| MindError::Grammar(format!("llama.cpp rejected it: {e}")))?,
            );
        }
        let s = req.sampling;
        if s.repeat_penalty > 1.0 {
            chain.push(LlamaSampler::penalties(64, s.repeat_penalty, 0.0, 0.0));
        }
        if s.temperature <= f32::EPSILON {
            chain.push(LlamaSampler::greedy());
        } else {
            if s.top_k > 0 {
                chain.push(LlamaSampler::top_k(s.top_k as i32));
            }
            if s.top_p < 1.0 {
                chain.push(LlamaSampler::top_p(s.top_p, 1));
            }
            chain.push(LlamaSampler::temp(s.temperature));
            chain.push(LlamaSampler::dist(s.seed as u32));
        }
        Ok(LlamaSampler::chain_simple(chain))
    }
}

fn common_prefix(a: &[LlamaToken], b: &[LlamaToken]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x.0 == y.0).count()
}

impl Backend for LlamaBackend {
    fn name(&self) -> &'static str {
        "llama.cpp"
    }

    fn caps(&self) -> Caps {
        Caps {
            grammars: true,
            embeddings: true,
            kv_reuse: true,
            device_select: !Self::devices().is_empty(),
            warm_evict: true,
        }
    }

    fn load(&mut self, req: &LoadRequest) -> Result<Loaded> {
        let started = std::time::Instant::now();
        let (model, ctx, vram_mib) = Self::open(
            &req.name,
            &req.path,
            req.gpu_layers,
            req.device.as_ref(),
            req.context_tokens,
            req.embedding,
            self.threads,
        )?;
        let n_ctx = ctx.n_ctx();
        let n_vocab = model.n_vocab().max(0) as u32;
        let n_embd = model.n_embd().max(0) as u32;
        let ram_mib = model.size() / (1024 * 1024);

        let handle = match self.by_name.get(&req.name) {
            Some(h) => *h,
            None => {
                let h = ModelHandle(self.next);
                self.next += 1;
                self.by_name.insert(req.name.clone(), h);
                h
            }
        };
        // Replacing an existing slot drops its context and model in the right
        // order, because `Slot`'s fields are declared in that order.
        self.slots.insert(
            handle,
            Slot {
                ctx: Some(ctx),
                model: Some(model),
                name: req.name.clone(),
                role: req.role,
                residency: Residency::Resident,
                path: req.path.clone(),
                n_ctx,
                gpu_layers: req.gpu_layers,
                embedding: req.embedding,
                device: req.device.clone(),
                vram_mib,
                seqs: BTreeMap::new(),
            },
        );
        Ok(Loaded {
            handle,
            name: req.name.clone(),
            role: req.role,
            residency: Residency::Resident,
            vram_mib,
            ram_mib,
            n_ctx,
            n_vocab,
            n_embd,
            took_ms: started.elapsed().as_millis() as u64,
        })
    }

    fn unload(&mut self, handle: ModelHandle, mode: UnloadMode) -> Result<()> {
        let threads = self.threads;
        let slot = self.slot_mut(handle)?;
        slot.drop_context();
        slot.seqs.clear();
        slot.vram_mib = 0;
        match mode {
            UnloadMode::Cold => {
                slot.model = None;
                slot.residency = Residency::Cold;
            }
            UnloadMode::Warm => {
                // Give the VRAM back but keep a CPU-side mapping, which is what
                // pins the weights in the page cache and makes the way back
                // cheap. See the module docs.
                let (name, path, n_ctx, embedding) = (
                    slot.name.clone(),
                    slot.path.clone(),
                    slot.n_ctx,
                    slot.embedding,
                );
                slot.model = None;
                match Self::open(&name, &path, 0, None, n_ctx, embedding, threads) {
                    Ok((model, ctx, _)) => {
                        let slot = self.slot_mut(handle)?;
                        slot.ctx = Some(ctx);
                        slot.model = Some(model);
                        slot.residency = Residency::Warm;
                    }
                    Err(e) => {
                        // SPEC §3.1: a downgrade cannot fail. Failing to *keep*
                        // the mapping is not a failure to free the VRAM, which
                        // has already happened; it just makes the way back
                        // expensive.
                        tracing::warn!(error = %e, "could not keep a CPU mapping; going cold");
                        let slot = self.slot_mut(handle)?;
                        slot.residency = Residency::Cold;
                    }
                }
            }
        }
        Ok(())
    }

    fn rewarm(&mut self, handle: ModelHandle, device: Option<&GpuTarget>) -> Result<Loaded> {
        let started = std::time::Instant::now();
        let threads = self.threads;
        let slot = self.slot(handle)?;
        let (name, path, n_ctx, embedding, gpu_layers, role) = (
            slot.name.clone(),
            slot.path.clone(),
            slot.n_ctx,
            slot.embedding,
            slot.gpu_layers,
            slot.role,
        );
        let device = device.cloned().or_else(|| slot.device.clone());

        {
            let slot = self.slot_mut(handle)?;
            slot.drop_context();
            slot.model = None;
            slot.seqs.clear();
        }
        let (model, ctx, vram_mib) = Self::open(
            &name,
            &path,
            gpu_layers,
            device.as_ref(),
            n_ctx,
            embedding,
            threads,
        )?;
        let n_vocab = model.n_vocab().max(0) as u32;
        let n_embd = model.n_embd().max(0) as u32;
        let ram_mib = model.size() / (1024 * 1024);
        let n_ctx = ctx.n_ctx();
        let slot = self.slot_mut(handle)?;
        slot.ctx = Some(ctx);
        slot.model = Some(model);
        slot.residency = Residency::Resident;
        slot.device = device;
        slot.vram_mib = vram_mib;
        Ok(Loaded {
            handle,
            name,
            role,
            residency: Residency::Resident,
            vram_mib,
            ram_mib,
            n_ctx,
            n_vocab,
            n_embd,
            took_ms: started.elapsed().as_millis() as u64,
        })
    }

    fn residency(&self, handle: ModelHandle) -> Residency {
        self.slots
            .get(&handle)
            .map(|s| s.residency)
            .unwrap_or(Residency::Cold)
    }

    fn vram_mib(&self, handle: ModelHandle) -> u64 {
        self.slots.get(&handle).map(|s| s.vram_mib).unwrap_or(0)
    }

    fn tokenize(&self, handle: ModelHandle, text: &str, add_special: bool) -> Result<Vec<Token>> {
        let slot = self.slot(handle)?;
        let model = slot
            .model
            .as_deref()
            .ok_or(MindError::NotLoaded(slot.role))?;
        let bos = if add_special {
            AddBos::Always
        } else {
            AddBos::Never
        };
        Ok(model
            .str_to_token(text, bos)
            .map_err(|e| MindError::Inference(e.to_string()))?
            .into_iter()
            .map(|t| t.0)
            .collect())
    }

    fn detokenize(&self, handle: ModelHandle, tokens: &[Token]) -> Result<String> {
        let slot = self.slot(handle)?;
        let model = slot
            .model
            .as_deref()
            .ok_or(MindError::NotLoaded(slot.role))?;
        let mut bytes = Vec::new();
        for t in tokens {
            #[allow(deprecated)]
            bytes.extend(
                model
                    .token_to_bytes(LlamaToken(*t), Special::Tokenize)
                    .map_err(|e| MindError::Inference(e.to_string()))?,
            );
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn generate(
        &mut self,
        handle: ModelHandle,
        req: &GenRequest,
        sink: &mut dyn FnMut(GenChunk<'_>) -> Flow,
    ) -> Result<Generated> {
        let prompt_tokens: Vec<LlamaToken> = match &req.prompt_tokens {
            Some(t) => t.iter().map(|x| LlamaToken(*x)).collect(),
            None => self
                .tokenize(handle, &req.prompt, true)?
                .into_iter()
                .map(LlamaToken)
                .collect(),
        };
        if prompt_tokens.is_empty() {
            return Err(MindError::Inference("an empty prompt".into()));
        }

        let (role, n_ctx) = {
            let slot = self.slot(handle)?;
            if slot.residency != Residency::Resident {
                return Err(MindError::NotLoaded(slot.role));
            }
            (slot.role, slot.n_ctx as usize)
        };
        if prompt_tokens.len() + 8 > n_ctx {
            return Err(MindError::Inference(format!(
                "a {}-token prompt does not fit a {n_ctx}-token context",
                prompt_tokens.len()
            )));
        }
        let mut sampler = self.sampler(self.slot(handle)?, req)?;

        let seq = req.slot.0 as i32;
        let mut batch = LlamaBatch::new(n_ctx.min(4096), 1);
        let prefill_started = std::time::Instant::now();

        // **F15.** Keep whatever of this sequence's KV already agrees with the
        // prompt and prefill only the rest. llama.cpp needs at least one token
        // to evaluate, so the whole prompt is never reused.
        let reuse = {
            let slot = self.slot_mut(handle)?;
            let cached = slot.seqs.get(&req.slot).cloned().unwrap_or_default();
            let mut reuse = common_prefix(&cached, &prompt_tokens);
            if reuse == prompt_tokens.len() {
                reuse = prompt_tokens.len() - 1;
            }
            let ctx = slot.ctx.as_mut().ok_or(MindError::NotLoaded(role))?;
            // Drop everything after the agreed prefix; keep the prefix itself.
            ctx.clear_kv_cache_seq(Some(seq as u32), Some(reuse as u32), None)
                .map_err(|e| MindError::Inference(e.to_string()))?;
            for (i, t) in prompt_tokens[reuse..].iter().enumerate() {
                let pos = (reuse + i) as i32;
                let last = reuse + i == prompt_tokens.len() - 1;
                batch
                    .add(*t, pos, &[seq], last)
                    .map_err(|e| MindError::Inference(e.to_string()))?;
            }
            ctx.decode(&mut batch)
                .map_err(|e| MindError::Inference(e.to_string()))?;
            reuse
        };
        let prefill_ms = prefill_started.elapsed().as_millis() as u64;

        let decode_started = std::time::Instant::now();
        let mut produced: Vec<LlamaToken> = Vec::new();
        let mut text = String::new();
        // Tokens are bytes, not characters: a multi-byte codepoint can straddle
        // two of them, so partial UTF-8 is held back rather than lossily
        // replaced with a `?` the operator would see.
        let mut pending: Vec<u8> = Vec::new();
        let mut stopped = StopReason::MaxTokens;
        let mut pos = prompt_tokens.len() as i32;

        for i in 0..req.max_tokens {
            // Everything is re-borrowed each iteration. The two fields are
            // disjoint, so `model` and `ctx` can be held at once, but neither
            // may outlive the step.
            let slot = self
                .slots
                .get_mut(&handle)
                .ok_or(MindError::NotLoaded(role))?;
            let model = slot.model.as_deref().ok_or(MindError::NotLoaded(role))?;
            let ctx = slot.ctx.as_mut().ok_or(MindError::NotLoaded(role))?;

            // `sample` **also accepts**: `llama_sampler_sample` applies the
            // chain, takes the selected token, and calls
            // `llama_sampler_accept` on it before returning. Accepting again
            // here would advance the grammar twice per token — the second
            // advance fails, the stacks empty, and llama.cpp throws. That cost
            // an afternoon; it is written down so it does not cost another one.
            let token = sampler.sample(ctx, batch.n_tokens() - 1);

            if model.is_eog_token(token) {
                stopped = if req.grammar.is_some() {
                    // A constrained decode that reaches end-of-generation has
                    // produced a complete sentence in the grammar's language.
                    StopReason::GrammarComplete
                } else {
                    StopReason::Eos
                };
                break;
            }
            produced.push(token);
            #[allow(deprecated)]
            pending.extend(
                model
                    .token_to_bytes(token, Special::Plaintext)
                    .map_err(|e| MindError::Inference(e.to_string()))?,
            );
            let piece = match std::str::from_utf8(&pending) {
                Ok(s) => {
                    let s = s.to_string();
                    pending.clear();
                    s
                }
                Err(e) if e.valid_up_to() > 0 => {
                    let upto = e.valid_up_to();
                    let s = String::from_utf8_lossy(&pending[..upto]).into_owned();
                    pending.drain(..upto);
                    s
                }
                // A codepoint that is not finished yet: wait for the next token
                // rather than emitting half of it.
                Err(_) => String::new(),
            };

            batch.clear();
            batch
                .add(token, pos, &[seq], true)
                .map_err(|e| MindError::Inference(e.to_string()))?;
            ctx.decode(&mut batch)
                .map_err(|e| MindError::Inference(e.to_string()))?;
            pos += 1;

            if !piece.is_empty() {
                text.push_str(&piece);
                if req.stop.iter().any(|s| text.ends_with(s.as_str())) {
                    stopped = StopReason::StopSequence;
                    break;
                }
                if sink(GenChunk {
                    text: &piece,
                    token: token.0,
                    index: i,
                }) == Flow::Stop
                {
                    stopped = StopReason::Cancelled;
                    break;
                }
            }
        }

        let mut kept = prompt_tokens.clone();
        kept.extend_from_slice(&produced);
        let slot = self.slot_mut(handle)?;
        slot.seqs.insert(req.slot, kept);

        Ok(Generated {
            text,
            prompt_tokens: prompt_tokens.len() as u32,
            reused_prefix_tokens: reuse as u32,
            generated_tokens: produced.len() as u32,
            stopped,
            prefill_ms,
            decode_ms: decode_started.elapsed().as_millis() as u64,
        })
    }

    fn embed(&mut self, handle: ModelHandle, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        let n_ctx = self.slot(handle)?.n_ctx as usize;
        for (i, text) in texts.iter().enumerate() {
            let tokens: Vec<LlamaToken> = self
                .tokenize(handle, text, true)?
                .into_iter()
                .map(LlamaToken)
                .collect();
            if tokens.is_empty() {
                out.push(Vec::new());
                continue;
            }
            let slot = self.slot_mut(handle)?;
            if !slot.embedding {
                return Err(MindError::Unsupported(
                    "embed with a model that was not loaded in embedding mode",
                ));
            }
            let role = slot.role;
            let ctx = slot.ctx.as_mut().ok_or(MindError::NotLoaded(role))?;
            ctx.clear_kv_cache();
            let mut batch = LlamaBatch::new(n_ctx.min(tokens.len().max(8) * 2), 1);
            for (j, t) in tokens.iter().take(n_ctx).enumerate() {
                batch
                    .add(*t, j as i32, &[0], j == tokens.len() - 1)
                    .map_err(|e| MindError::Inference(e.to_string()))?;
            }
            ctx.decode(&mut batch)
                .map_err(|e| MindError::Inference(e.to_string()))?;
            let v = ctx
                .embeddings_seq_ith(0)
                .map_err(|e| MindError::Inference(format!("embedding {i}: {e}")))?
                .to_vec();
            out.push(v);
        }
        Ok(out)
    }

    fn kv_clear(&mut self, handle: ModelHandle, slot_id: SlotId) -> Result<()> {
        let slot = self.slot_mut(handle)?;
        slot.seqs.remove(&slot_id);
        let role = slot.role;
        if let Some(ctx) = slot.ctx.as_mut() {
            ctx.clear_kv_cache_seq(Some(slot_id.0), None, None)
                .map_err(|e| MindError::Inference(e.to_string()))?;
        } else {
            return Err(MindError::NotLoaded(role));
        }
        Ok(())
    }

    fn kv_tokens(&self, handle: ModelHandle, slot_id: SlotId) -> u32 {
        self.slots
            .get(&handle)
            .and_then(|s| s.seqs.get(&slot_id))
            .map(|v| v.len() as u32)
            .unwrap_or(0)
    }
}

// `LlamaModel` and `LlamaContext` are `Send` (llama.cpp serialises internally
// and this crate only ever touches one from behind
// `crate::manager::SharedBackend`'s mutex).
unsafe impl Send for LlamaBackend {}
