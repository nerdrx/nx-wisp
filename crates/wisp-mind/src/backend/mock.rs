//! The backend the test suite runs against.
//!
//! SPEC §4: *a mock inference backend means the suite never needs a model or a
//! GPU.* Every behaviour the real backend has that the rest of the crate
//! depends on is reproduced here **deterministically**:
//!
//! * tokenisation round-trips exactly, so KV-prefix arithmetic (F15) is
//!   testable to the token;
//! * `Warm` and `Cold` unloads are distinguishable, and rewarming reports a
//!   different cost, so F12's "the way back is ~1 s, not 30" is an assertion
//!   rather than a claim in a comment;
//! * the VRAM budget is enforced, so F13's eviction path is exercised;
//! * **a grammar is obeyed.** When a request carries GBNF, the mock's reply is
//!   derived from the grammar itself ([`crate::grammar::Grammar::shortest`]),
//!   and a scripted reply that the grammar would not accept is an error. A mock
//!   that could emit what a constrained decoder cannot is a mock that hides
//!   exactly the bugs F14 exists to prevent.
//!
//! Nothing here reads the clock or the filesystem unless it is told to.

use std::collections::{BTreeMap, HashMap};

use crate::backend::{
    Backend, Caps, Flow, GenChunk, GenRequest, Generated, LoadRequest, Loaded, ModelHandle,
    Residency, Role, SlotId, StopReason, Token, UnloadMode,
};
use crate::error::{MindError, Result};
use crate::grammar::Grammar;
use wisp_gov::GpuTarget;

/// One thing the mock was asked to do, in order. Assertions read this instead
/// of guessing from side effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trace {
    Load {
        name: String,
        role: Role,
        gpu: Option<String>,
        vram_mib: u64,
    },
    Unload {
        name: String,
        mode: UnloadMode,
    },
    Rewarm {
        name: String,
        gpu: Option<String>,
    },
    Generate {
        name: String,
        slot: SlotId,
        reused: u32,
        constrained: bool,
    },
    Embed {
        name: String,
        count: usize,
    },
    KvClear {
        name: String,
        slot: SlotId,
    },
}

#[derive(Debug, Clone)]
struct MockModel {
    name: String,
    role: Role,
    residency: Residency,
    vram_mib: u64,
    resident_vram_mib: u64,
    n_ctx: u32,
    slots: BTreeMap<SlotId, Vec<Token>>,
    last_device: Option<GpuTarget>,
}

/// Words in, ids out, and back again — exactly.
#[derive(Debug, Default, Clone)]
struct Vocab {
    to_id: HashMap<String, Token>,
    to_text: Vec<String>,
}

impl Vocab {
    /// Ids start above the range llama.cpp reserves for specials, so a test
    /// that mistakes a mock id for a real one looks wrong rather than plausible.
    const FIRST: Token = 1000;

    fn id(&mut self, piece: &str) -> Token {
        if let Some(id) = self.to_id.get(piece) {
            return *id;
        }
        let id = Vocab::FIRST + self.to_text.len() as Token;
        self.to_text.push(piece.to_string());
        self.to_id.insert(piece.to_string(), id);
        id
    }

    fn text(&self, id: Token) -> Option<&str> {
        let i = (id - Vocab::FIRST) as usize;
        self.to_text.get(i).map(String::as_str)
    }

    fn len(&self) -> u32 {
        self.to_text.len() as u32
    }
}

/// Split into runs of word characters and single non-word characters. Not a
/// BPE, but stable, exactly reversible, and roughly the right granularity for
/// prefix-cache arithmetic to mean something.
fn pieces(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
        if word(b[i]) {
            while i < b.len() && word(b[i]) {
                i += 1;
            }
        } else {
            // Step by whole characters so multi-byte UTF-8 survives.
            let ch = text[i..].chars().next().expect("non-empty");
            i += ch.len_utf8();
        }
        out.push(&text[start..i]);
    }
    out
}

#[derive(Debug, Clone)]
struct Script {
    needle: Option<String>,
    reply: String,
}

/// A deterministic, scriptable [`Backend`].
pub struct MockBackend {
    next_handle: u64,
    models: BTreeMap<ModelHandle, MockModel>,
    by_name: HashMap<String, ModelHandle>,
    vocab: Vocab,
    scripts: Vec<Script>,
    vram_hints: HashMap<String, u64>,
    load_failures: HashMap<String, String>,
    pub trace: Vec<Trace>,
    embed_dim: usize,
    /// What a cold load costs, in the milliseconds it reports.
    pub cold_load_ms: u64,
    /// What coming back from [`Residency::Warm`] costs. The gap between the two
    /// is F12's whole argument.
    pub warm_load_ms: u64,
    caps: Caps,
    devices: Vec<(u32, u32)>,
}

impl Default for MockBackend {
    fn default() -> Self {
        MockBackend::new()
    }
}

impl std::fmt::Debug for MockBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockBackend")
            .field("models", &self.models.values().map(|m| &m.name).collect::<Vec<_>>())
            .field("scripts", &self.scripts.len())
            .field("trace", &self.trace.len())
            .finish()
    }
}

impl MockBackend {
    pub fn new() -> Self {
        MockBackend {
            next_handle: 1,
            models: BTreeMap::new(),
            by_name: HashMap::new(),
            vocab: Vocab::default(),
            scripts: Vec::new(),
            vram_hints: HashMap::new(),
            load_failures: HashMap::new(),
            trace: Vec::new(),
            embed_dim: 64,
            cold_load_ms: 24_000,
            warm_load_ms: 900,
            caps: Caps {
                grammars: true,
                embeddings: true,
                kv_reuse: true,
                device_select: true,
                warm_evict: true,
            },
            devices: vec![(0x1002, 0x744c), (0x1002, 0x13c0)],
        }
    }

    /// Reply with `reply` whenever the prompt contains `needle`. First match
    /// wins, so scripts are checked in the order they were added.
    pub fn script(mut self, needle: &str, reply: &str) -> Self {
        self.scripts.push(Script {
            needle: Some(needle.to_string()),
            reply: reply.to_string(),
        });
        self
    }

    /// Reply with this to anything not otherwise scripted.
    pub fn always(mut self, reply: &str) -> Self {
        self.scripts.push(Script {
            needle: None,
            reply: reply.to_string(),
        });
        self
    }

    /// How much VRAM this model claims to want. Without a hint the mock derives
    /// it from the file on disk, and from nothing at all if there is no file.
    pub fn vram_hint(mut self, name: &str, mib: u64) -> Self {
        self.vram_hints.insert(name.to_string(), mib);
        self
    }

    /// Make loading this model fail, for the "the GGUF is corrupt" path.
    pub fn fail_load(mut self, name: &str, why: &str) -> Self {
        self.load_failures
            .insert(name.to_string(), why.to_string());
        self
    }

    pub fn embed_dim(mut self, dim: usize) -> Self {
        self.embed_dim = dim;
        self
    }

    /// Pretend to be a backend with no GPU at all — the CI case, and the case
    /// where the governor has sent her to the CPU.
    pub fn headless(mut self) -> Self {
        self.devices.clear();
        self.caps.device_select = false;
        self
    }

    /// Pretend to be a backend that cannot keep weights mapped across an
    /// eviction, so the manager's fallback path gets exercised.
    pub fn without_warm_evict(mut self) -> Self {
        self.caps.warm_evict = false;
        self
    }

    pub fn residency_of(&self, name: &str) -> Option<Residency> {
        self.by_name
            .get(name)
            .and_then(|h| self.models.get(h))
            .map(|m| m.residency)
    }

    pub fn loaded_names(&self) -> Vec<&str> {
        self.models
            .values()
            .filter(|m| m.residency.is_loaded())
            .map(|m| m.name.as_str())
            .collect()
    }

    /// Total VRAM the mock believes it is holding right now.
    pub fn vram_held_mib(&self) -> u64 {
        self.models.values().map(|m| m.resident_vram_mib).sum()
    }

    pub fn clear_trace(&mut self) {
        self.trace.clear();
    }

    fn model(&self, h: ModelHandle) -> Result<&MockModel> {
        self.models
            .get(&h)
            .ok_or_else(|| MindError::Inference(format!("no such handle {}", h.0)))
    }

    fn model_mut(&mut self, h: ModelHandle) -> Result<&mut MockModel> {
        self.models
            .get_mut(&h)
            .ok_or_else(|| MindError::Inference(format!("no such handle {}", h.0)))
    }

    fn want_vram(&self, req: &LoadRequest) -> u64 {
        if let Some(m) = self.vram_hints.get(&req.name) {
            return *m;
        }
        match std::fs::metadata(&req.path) {
            Ok(md) => (md.len() / (1024 * 1024)).max(1),
            // No file, no claim. A test that cares sets a hint.
            Err(_) => 256,
        }
    }

    fn pick_reply(&self, req: &GenRequest) -> Result<String> {
        let scripted = self
            .scripts
            .iter()
            .find(|s| match &s.needle {
                Some(n) => req.prompt.contains(n.as_str()),
                None => true,
            })
            .map(|s| s.reply.clone());

        let Some(gbnf) = req.grammar.as_deref() else {
            return Ok(scripted.unwrap_or_else(|| default_reply(&req.prompt)));
        };
        let g = Grammar::parse(gbnf)?;
        match scripted {
            Some(reply) => {
                if !g.accepts(&reply) {
                    return Err(MindError::Inference(format!(
                        "the scripted reply {reply:?} is not in the grammar's language, so a \
                         constrained decoder could never have produced it — fix the test, not \
                         the mock"
                    )));
                }
                Ok(reply)
            }
            None => g.shortest().ok_or_else(|| {
                MindError::Inference(
                    "the grammar has no finite derivation, so nothing could be generated".into(),
                )
            }),
        }
    }
}

/// Stable, meaningless, and never the same for two different prompts. Enough
/// for a test that only needs *an* answer.
fn default_reply(prompt: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in prompt.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("mock reply {:016x}", h)
}

fn common_prefix(a: &[Token], b: &[Token]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

impl Backend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn caps(&self) -> Caps {
        self.caps
    }

    fn load(&mut self, req: &LoadRequest) -> Result<Loaded> {
        if let Some(why) = self.load_failures.get(&req.name) {
            return Err(MindError::LoadFailed {
                backend: "mock",
                name: req.name.clone(),
                why: why.clone(),
            });
        }
        let want = self.want_vram(req);
        let on_gpu = req.device.is_some();
        let vram = if on_gpu { want } else { 0 };
        if on_gpu && want > req.vram_budget_mib {
            return Err(MindError::OverBudget {
                want_mib: want,
                have_mib: req.vram_budget_mib,
            });
        }

        let handle = match self.by_name.get(&req.name) {
            Some(h) => *h,
            None => {
                let h = ModelHandle(self.next_handle);
                self.next_handle += 1;
                self.by_name.insert(req.name.clone(), h);
                h
            }
        };
        let existing_slots = self
            .models
            .get(&handle)
            .map(|m| m.slots.clone())
            .unwrap_or_default();
        self.models.insert(
            handle,
            MockModel {
                name: req.name.clone(),
                role: req.role,
                residency: Residency::Resident,
                vram_mib: want,
                resident_vram_mib: vram,
                n_ctx: req.context_tokens,
                slots: existing_slots,
                last_device: req.device.clone(),
            },
        );
        self.trace.push(Trace::Load {
            name: req.name.clone(),
            role: req.role,
            gpu: req.device.as_ref().map(|d| d.pci_slot.clone()),
            vram_mib: vram,
        });
        Ok(Loaded {
            handle,
            name: req.name.clone(),
            role: req.role,
            residency: Residency::Resident,
            vram_mib: vram,
            ram_mib: want,
            n_ctx: req.context_tokens,
            n_vocab: self.vocab.len().max(1),
            n_embd: self.embed_dim as u32,
            took_ms: self.cold_load_ms,
        })
    }

    fn unload(&mut self, handle: ModelHandle, mode: UnloadMode) -> Result<()> {
        let warm_ok = self.caps.warm_evict;
        let m = self.model_mut(handle)?;
        let name = m.name.clone();
        m.resident_vram_mib = 0;
        m.residency = match mode {
            UnloadMode::Warm if warm_ok => Residency::Warm,
            // A backend that cannot keep the mapping says so rather than
            // pretending: the manager then knows the way back is expensive.
            _ => {
                m.slots.clear();
                Residency::Cold
            }
        };
        self.trace.push(Trace::Unload { name, mode });
        Ok(())
    }

    fn rewarm(&mut self, handle: ModelHandle, device: Option<&GpuTarget>) -> Result<Loaded> {
        let warm_ms = self.warm_load_ms;
        let cold_ms = self.cold_load_ms;
        let embed_dim = self.embed_dim;
        let vocab_len = self.vocab.len().max(1);
        let m = self.model_mut(handle)?;
        let was = m.residency;
        let vram = if device.is_some() { m.vram_mib } else { 0 };
        m.resident_vram_mib = vram;
        m.residency = Residency::Resident;
        m.last_device = device.cloned();
        let name = m.name.clone();
        let role = m.role;
        let n_ctx = m.n_ctx;
        let ram = m.vram_mib;
        self.trace.push(Trace::Rewarm {
            name: name.clone(),
            gpu: device.map(|d| d.pci_slot.clone()),
        });
        Ok(Loaded {
            handle,
            name,
            role,
            residency: Residency::Resident,
            vram_mib: vram,
            ram_mib: ram,
            n_ctx,
            n_vocab: vocab_len,
            n_embd: embed_dim as u32,
            took_ms: if was == Residency::Warm {
                warm_ms
            } else {
                cold_ms
            },
        })
    }

    fn residency(&self, handle: ModelHandle) -> Residency {
        self.models
            .get(&handle)
            .map(|m| m.residency)
            .unwrap_or(Residency::Cold)
    }

    fn vram_mib(&self, handle: ModelHandle) -> u64 {
        self.models
            .get(&handle)
            .map(|m| m.resident_vram_mib)
            .unwrap_or(0)
    }

    fn tokenize(&self, _handle: ModelHandle, text: &str, _add_special: bool) -> Result<Vec<Token>> {
        // `&self`, so ids are assigned from a shadow copy. Determinism comes
        // from the piece text, not from insertion order, so a token id is
        // stable for a given piece within a run.
        let mut v = self.vocab.clone();
        Ok(pieces(text).into_iter().map(|p| v.id(p)).collect())
    }

    fn detokenize(&self, _handle: ModelHandle, tokens: &[Token]) -> Result<String> {
        let mut out = String::new();
        for t in tokens {
            match self.vocab.text(*t) {
                Some(s) => out.push_str(s),
                None => {
                    return Err(MindError::Inference(format!(
                        "token {t} is not in the mock vocabulary"
                    )))
                }
            }
        }
        Ok(out)
    }

    fn generate(
        &mut self,
        handle: ModelHandle,
        req: &GenRequest,
        sink: &mut dyn FnMut(GenChunk<'_>) -> Flow,
    ) -> Result<Generated> {
        if !self.residency(handle).is_loaded() {
            let role = self.model(handle)?.role;
            return Err(MindError::NotLoaded(role));
        }
        let reply = self.pick_reply(req)?;

        let prompt_pieces = pieces(&req.prompt);
        let mut prompt_tokens = Vec::with_capacity(prompt_pieces.len());
        for p in &prompt_pieces {
            let id = self.vocab.id(p);
            prompt_tokens.push(id);
        }
        let name = self.model(handle)?.name.clone();
        let cached = self
            .model(handle)?
            .slots
            .get(&req.slot)
            .cloned()
            .unwrap_or_default();
        let reused = common_prefix(&cached, &prompt_tokens) as u32;

        let mut emitted: Vec<Token> = Vec::new();
        let mut text = String::new();
        let mut stopped = StopReason::Eos;
        for (i, piece) in pieces(&reply).into_iter().enumerate() {
            if i as u32 >= req.max_tokens {
                stopped = StopReason::MaxTokens;
                break;
            }
            let id = self.vocab.id(piece);
            emitted.push(id);
            text.push_str(piece);
            if req.stop.iter().any(|s| text.ends_with(s.as_str())) {
                stopped = StopReason::StopSequence;
                break;
            }
            let flow = sink(GenChunk {
                text: piece,
                token: id,
                index: i as u32,
            });
            if flow == Flow::Stop {
                stopped = StopReason::Cancelled;
                break;
            }
        }
        if req.grammar.is_some() && stopped == StopReason::Eos {
            stopped = StopReason::GrammarComplete;
        }

        let generated = emitted.len() as u32;
        {
            let mut kept = prompt_tokens.clone();
            kept.extend_from_slice(&emitted);
            let m = self.model_mut(handle)?;
            m.slots.insert(req.slot, kept);
        }
        self.trace.push(Trace::Generate {
            name,
            slot: req.slot,
            reused,
            constrained: req.grammar.is_some(),
        });
        Ok(Generated {
            text,
            prompt_tokens: prompt_tokens.len() as u32,
            reused_prefix_tokens: reused,
            generated_tokens: generated,
            stopped,
            // Deterministic and proportional: the point is that a cached prefix
            // costs nothing, not that these are real milliseconds.
            prefill_ms: (prompt_tokens.len() as u64 - reused as u64) / 4,
            decode_ms: generated as u64 * 2,
        })
    }

    fn embed(&mut self, handle: ModelHandle, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if !self.residency(handle).is_loaded() {
            let role = self.model(handle)?.role;
            return Err(MindError::NotLoaded(role));
        }
        let name = self.model(handle)?.name.clone();
        self.trace.push(Trace::Embed {
            name,
            count: texts.len(),
        });
        let dim = self.embed_dim;
        Ok(texts
            .iter()
            .map(|t| crate::memory::embed::hash_embed(t, dim))
            .collect())
    }

    fn kv_clear(&mut self, handle: ModelHandle, slot: SlotId) -> Result<()> {
        let m = self.model_mut(handle)?;
        m.slots.remove(&slot);
        let name = m.name.clone();
        self.trace.push(Trace::KvClear { name, slot });
        Ok(())
    }

    fn kv_tokens(&self, handle: ModelHandle, slot: SlotId) -> u32 {
        self.models
            .get(&handle)
            .and_then(|m| m.slots.get(&slot))
            .map(|v| v.len() as u32)
            .unwrap_or(0)
    }

    fn enumerated_devices(&self) -> Vec<(u32, u32)> {
        self.devices.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenisation_round_trips() {
        let mut b = MockBackend::new();
        let req = LoadRequest::new("m", "/nonexistent", Role::Reflex);
        let l = b.load(&req).expect("load");
        let text = "She said: {\"say\": \"hello, world\"} — twice.";
        let toks = b.tokenize(l.handle, text, true).expect("tokenize");
        // `tokenize` takes `&self`, so ids only become real once something has
        // generated with them; do that first.
        let mut b2 = MockBackend::new();
        let l2 = b2.load(&req).expect("load");
        for p in pieces(text) {
            b2.vocab.id(p);
        }
        let toks2 = b2.tokenize(l2.handle, text, true).expect("tokenize");
        assert_eq!(toks, toks2, "ids must not depend on when they were minted");
        assert_eq!(b2.detokenize(l2.handle, &toks2).expect("detok"), text);
    }

    #[test]
    fn a_scripted_reply_outside_the_grammar_is_an_error_not_a_surprise() {
        let mut b = MockBackend::new().script("anything", "not json at all");
        let l = b
            .load(&LoadRequest::new("m", "/nonexistent", Role::Reflex))
            .expect("load");
        let g = crate::grammar::reply_grammar(&Default::default()).expect("grammar");
        let req = GenRequest::new("anything").grammar(g);
        let err = b.generate(l.handle, &req, &mut |_| Flow::Continue).unwrap_err();
        assert!(
            err.to_string().contains("constrained decoder"),
            "{err}"
        );
    }
}
