//! The end-to-end check against a real model, on real Vulkan.
//!
//! **Skipped unless `NX_WISP_LLAMA_SMOKE` points at a GGUF**, and only compiled
//! at all with `--features llama`. CI has neither a GPU nor a model, and SPEC
//! §4 makes that non-negotiable: `cargo test -p wisp-mind` must pass without
//! either. So this file exists to answer one question that a mock cannot —
//! *does the grammar actually constrain a real decoder?* — and it answers it
//! with a 100 MB model rather than an 18 GiB one.
//!
//! ```sh
//! source env.sh
//! NX_WISP_LLAMA_SMOKE=/run/media/nerdrx/Lex/claude/tools/nx-wisp-models/SmolLM2-135M-Instruct-Q4_K_M.gguf \
//!   cargo test -p wisp-mind --features vulkan --test real_llama -- --nocapture
//! ```

#![cfg(feature = "llama")]

mod common;

use std::path::PathBuf;

use common::Fixture;
use wisp_mind::backend::llama::LlamaBackend;
use wisp_mind::backend::{Backend, GenRequest, LoadRequest, Residency, Role, Sampling, SlotId};
use wisp_mind::grammar::{enum_grammar, reply_grammar, schema_grammar, Grammar, GrammarOptions};

/// The model to exercise, or `None` — in which case every test here quietly
/// passes, because "there is no model" is the ordinary state of a CI box.
fn model() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("NX_WISP_LLAMA_SMOKE")?);
    p.is_file().then_some(p)
}

macro_rules! model_or_skip {
    () => {
        match model() {
            Some(p) => p,
            None => {
                eprintln!("NX_WISP_LLAMA_SMOKE is not set to a GGUF; skipping");
                return;
            }
        }
    };
}

fn load(path: &PathBuf, ctx: u32) -> (LlamaBackend, wisp_mind::backend::ModelHandle) {
    let mut b = LlamaBackend::new();
    let devices = LlamaBackend::devices();
    eprintln!("ggml devices: {devices:#?}");
    let req = LoadRequest::new("smoke", path, Role::Reflex).context(ctx);
    let loaded = b.load(&req).expect("the model loads");
    eprintln!(
        "loaded {} in {} ms: {} ctx, {} vocab, {} embd, {} MiB RAM",
        loaded.name, loaded.took_ms, loaded.n_ctx, loaded.n_vocab, loaded.n_embd, loaded.ram_mib
    );
    (b, loaded.handle)
}

#[test]
fn a_real_model_loads_and_tokenises() {
    let path = model_or_skip!();
    let _f = Fixture::new();
    let (b, h) = load(&path, 1024);
    let text = "The shader cache is recompiling again.";
    let toks = b.tokenize(h, text, true).expect("tokenize");
    assert!(!toks.is_empty());
    let back = b.detokenize(h, &toks).expect("detokenize");
    assert!(
        back.contains("shader cache"),
        "round trip lost the text: {back:?}"
    );
}

#[test]
fn a_real_model_generates_something() {
    let path = model_or_skip!();
    let _f = Fixture::new();
    let (mut b, h) = load(&path, 1024);
    let req = GenRequest::new(
        "<|im_start|>user\nName one colour.<|im_end|>\n<|im_start|>assistant\n",
    )
    .max_tokens(24)
    .sampling(Sampling::DETERMINISTIC);
    let mut streamed = String::new();
    let out = b
        .generate(h, &req, &mut |c| {
            streamed.push_str(c.text);
            wisp_mind::backend::Flow::Continue
        })
        .expect("generate");
    eprintln!(
        "generated {:?} ({} tokens, prefill {} ms, decode {} ms, stopped {:?})",
        out.text, out.generated_tokens, out.prefill_ms, out.decode_ms, out.stopped
    );
    assert!(out.generated_tokens > 0, "it produced nothing");
    assert!(!out.text.trim().is_empty());
    assert_eq!(streamed, out.text, "the stream and the total must agree");
}

/// The one that could not be tested any other way.
#[test]
fn the_grammar_really_constrains_a_real_decoder() {
    let path = model_or_skip!();
    let _f = Fixture::new();
    let (mut b, h) = load(&path, 1024);

    // A 135M model asked politely for JSON produces prose. Constrained, it
    // cannot.
    let gbnf = reply_grammar(&GrammarOptions::default()).expect("grammar");
    let checker = Grammar::parse(&gbnf).expect("parses");
    let req = GenRequest::new(
        "<|im_start|>user\nSay hello.<|im_end|>\n<|im_start|>assistant\n",
    )
    .grammar(gbnf.clone())
    .max_tokens(64);
    let out = b.generate(h, &req, &mut |_| wisp_mind::backend::Flow::Continue)
        .expect("generate");
    eprintln!("constrained output: {:?}", out.text);
    assert!(
        checker.accepts(out.text.trim()),
        "a constrained decoder produced something outside its own grammar: {:?}",
        out.text
    );
    let v: serde_json::Value =
        serde_json::from_str(out.text.trim()).expect("and it is valid JSON");
    assert!(v.get("say").is_some(), "{v}");
}

#[test]
fn a_tool_call_from_a_real_model_is_well_formed_by_construction() {
    let path = model_or_skip!();
    let _f = Fixture::new();
    let (mut b, h) = load(&path, 1024);

    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "label": {"type": "string"},
            "minutes": {"type": "integer"}
        },
        "required": ["label", "minutes"],
        "additionalProperties": false
    });
    let gbnf = schema_grammar(&schema).expect("grammar");
    let checker = Grammar::parse(&gbnf).expect("parses");
    let out = b
        .generate(
            h,
            &GenRequest::new(
                "<|im_start|>user\nSet a timer for tea, three minutes.<|im_end|>\n\
                 <|im_start|>assistant\n",
            )
            .grammar(gbnf)
            .max_tokens(64),
            &mut |_| wisp_mind::backend::Flow::Continue,
        )
        .expect("generate");
    eprintln!("tool arguments: {:?}", out.text);
    assert!(checker.accepts(out.text.trim()), "{:?}", out.text);
    let v: serde_json::Value = serde_json::from_str(out.text.trim()).expect("valid JSON");
    assert!(v["label"].is_string(), "{v}");
    assert!(v["minutes"].is_i64(), "{v}");
}

#[test]
fn a_closed_label_set_gets_exactly_one_label_back() {
    let path = model_or_skip!();
    let _f = Fixture::new();
    let (mut b, h) = load(&path, 1024);
    let gbnf = enum_grammar(&["answer", "escalate", "unsure"]).expect("grammar");
    let out = b
        .generate(
            h,
            &GenRequest::new(wisp_mind::escalate::self_assessment_prompt(
                &wisp_mind::escalate::Ask::from_operator(
                    "derive the closed form of this recurrence",
                ),
            ))
            .grammar(gbnf)
            .max_tokens(8)
            .sampling(Sampling::DETERMINISTIC),
            &mut |_| wisp_mind::backend::Flow::Continue,
        )
        .expect("generate");
    eprintln!("self-assessment: {:?}", out.text);
    assert!(
        wisp_mind::escalate::SelfAssessment::parse(out.text.trim()).is_some(),
        "expected one of the three labels, got {:?}",
        out.text
    );
}

/// **F15**, measured rather than asserted.
#[test]
fn a_cached_prefix_makes_the_second_turn_cheaper_to_start() {
    let path = model_or_skip!();
    let _f = Fixture::new();
    let (mut b, h) = load(&path, 2048);

    // A long-ish fixed prefix, standing in for the persona prompt.
    let persona: String = std::iter::repeat(
        "You are a small creature who lives on this desktop and does not speak directly. ",
    )
    .take(24)
    .collect();

    let first = GenRequest::new(format!("{persona}\nQ: one\nA:"))
        .max_tokens(8)
        .slot(SlotId(1))
        .sampling(Sampling::DETERMINISTIC);
    let a = b.generate(h, &first, &mut |_| wisp_mind::backend::Flow::Continue)
        .expect("first");
    assert_eq!(a.reused_prefix_tokens, 0, "nothing was cached yet");

    let second = GenRequest::new(format!("{persona}\nQ: two\nA:"))
        .max_tokens(8)
        .slot(SlotId(1))
        .sampling(Sampling::DETERMINISTIC);
    let c = b.generate(h, &second, &mut |_| wisp_mind::backend::Flow::Continue)
        .expect("second");
    eprintln!(
        "prefill: {} ms cold ({} tokens) vs {} ms warm ({} reused of {})",
        a.prefill_ms, a.prompt_tokens, c.prefill_ms, c.reused_prefix_tokens, c.prompt_tokens
    );
    assert!(
        c.reused_prefix_tokens > 0,
        "F15: the shared prefix should have come out of the KV cache"
    );
    assert!(
        c.prefix_hit_rate() > 0.7,
        "hit rate was {}",
        c.prefix_hit_rate()
    );
}

/// **F12**, measured rather than asserted.
#[test]
fn warm_eviction_really_is_cheaper_than_a_cold_load() {
    let path = model_or_skip!();
    let _f = Fixture::new();
    let (mut b, h) = load(&path, 1024);

    b.unload(h, wisp_mind::backend::UnloadMode::Warm)
        .expect("warm evict");
    assert_eq!(b.residency(h), Residency::Warm);
    assert_eq!(b.vram_mib(h), 0, "the VRAM must be back");
    let warm = b.rewarm(h, None).expect("rewarm").took_ms;

    b.unload(h, wisp_mind::backend::UnloadMode::Cold)
        .expect("cold unload");
    assert_eq!(b.residency(h), Residency::Cold);
    let cold = b.rewarm(h, None).expect("reload").took_ms;

    eprintln!("warm {warm} ms vs cold {cold} ms");
    // No hard assertion on the ratio: at 100 MB both are fast and the page
    // cache makes the "cold" path warm anyway. The number is printed so a human
    // can see it on a real 18 GiB model, where the difference is the feature.
    assert!(b.residency(h).is_loaded());
}
