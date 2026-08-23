//! **F14.** The tests that decide whether grammar-constrained tool calling is
//! real or decorative.
//!
//! The shape of every one of them is the same: build the grammar from a schema,
//! then ask the grammar whether a document is in its language. That is exactly
//! the question a constrained decoder answers at every token, so a document the
//! grammar rejects is a document the model provably cannot emit.

use serde_json::{json, Value};
use wisp_fleet::tools::NxTools;
use wisp_proto::ToolDescriptor;
use wisp_mind::grammar::{
    enum_grammar, reply_grammar, schema_grammar, tool_grammar, Grammar, GrammarOptions, ToolCall,
};

fn g(schema: Value) -> Grammar {
    let src = schema_grammar(&schema).expect("schema converts");
    Grammar::parse(&src).unwrap_or_else(|e| panic!("{e}\n---\n{src}"))
}

/// Both spellings a model might produce: minified, and with the whitespace a
/// chat model habitually inserts.
fn both(v: &Value) -> [String; 2] {
    [
        serde_json::to_string(v).expect("compact"),
        serde_json::to_string_pretty(v).expect("pretty"),
    ]
}

fn accepts_all(gr: &Grammar, v: &Value) {
    for s in both(v) {
        assert!(gr.accepts(&s), "should accept:\n{s}\n---\n{}", gr.source());
    }
}

#[test]
fn a_flat_object_with_required_and_optional_fields() {
    let gr = g(json!({
        "type": "object",
        "properties": {
            "minutes": {"type": "integer"},
            "label": {"type": "string"},
            "repeat": {"type": "boolean"}
        },
        "required": ["minutes"],
        "additionalProperties": false
    }));

    accepts_all(&gr, &json!({"minutes": 5}));
    accepts_all(&gr, &json!({"minutes": 5, "label": "tea"}));
    accepts_all(&gr, &json!({"minutes": 5, "repeat": true}));
    accepts_all(&gr, &json!({"minutes": 5, "label": "tea", "repeat": false}));

    // The required field is not optional.
    assert!(!gr.accepts(r#"{"label":"tea"}"#));
    // A field nobody declared is not reachable.
    assert!(!gr.accepts(r#"{"minutes":5,"colour":"blue"}"#));
    // Nor is the wrong type.
    assert!(!gr.accepts(r#"{"minutes":"five"}"#));
    // Nor a stray comma, which is the classic small-model failure.
    assert!(!gr.accepts(r#"{"minutes":5,}"#));
    assert!(!gr.accepts(r#"{,"minutes":5}"#));
}

#[test]
fn an_object_where_everything_is_optional_still_cannot_start_with_a_comma() {
    let gr = g(json!({
        "type": "object",
        "properties": {
            "a": {"type": "string"},
            "b": {"type": "string"},
            "c": {"type": "string"}
        },
        "additionalProperties": false
    }));

    accepts_all(&gr, &json!({}));
    for one in ["a", "b", "c"] {
        accepts_all(&gr, &json!({ one: "x" }));
    }
    accepts_all(&gr, &json!({"a": "x", "c": "z"}));
    accepts_all(&gr, &json!({"a": "x", "b": "y", "c": "z"}));

    assert!(!gr.accepts(r#"{,"a":"x"}"#));
    assert!(!gr.accepts(r#"{"a":"x",}"#));
    // Declared order is the only order: permitting all n! orderings would make
    // the grammar factorial for no gain.
    assert!(!gr.accepts(r#"{"c":"z","a":"x"}"#));
}

#[test]
fn enums_are_closed() {
    let gr = g(json!({
        "type": "object",
        "properties": {"action": {"enum": ["play", "pause", "next"]}},
        "required": ["action"]
    }));
    accepts_all(&gr, &json!({"action": "play"}));
    accepts_all(&gr, &json!({"action": "next"}));
    assert!(!gr.accepts(r#"{"action":"stop"}"#));
    assert!(!gr.accepts(r#"{"action":"Play"}"#));
    assert!(!gr.accepts(r#"{"action":"pla"}"#));
}

#[test]
fn arrays_including_the_empty_one_and_the_minimum_length_one() {
    let gr = g(json!({
        "type": "object",
        "properties": {"tags": {"type": "array", "items": {"type": "string"}}},
        "required": ["tags"]
    }));
    accepts_all(&gr, &json!({"tags": []}));
    accepts_all(&gr, &json!({"tags": ["a"]}));
    accepts_all(&gr, &json!({"tags": ["a", "b", "c"]}));
    assert!(!gr.accepts(r#"{"tags":["a",]}"#));
    assert!(!gr.accepts(r#"{"tags":[1]}"#));

    let least_one = g(json!({
        "type": "array", "items": {"type": "integer"}, "minItems": 1
    }));
    assert!(least_one.accepts("[1]"));
    assert!(least_one.accepts("[1, 2]"));
    assert!(!least_one.accepts("[]"));
}

#[test]
fn nested_objects_and_arrays_of_objects() {
    let gr = g(json!({
        "type": "object",
        "properties": {
            "query": {"type": "string"},
            "where": {
                "type": "object",
                "properties": {
                    "root": {"type": "string"},
                    "depth": {"type": "integer"},
                    "kinds": {"type": "array", "items": {"enum": ["file", "dir"]}}
                },
                "required": ["root"],
                "additionalProperties": false
            }
        },
        "required": ["query"],
        "additionalProperties": false
    }));

    accepts_all(&gr, &json!({"query": "shader"}));
    accepts_all(
        &gr,
        &json!({"query": "shader", "where": {"root": "/home", "depth": 3}}),
    );
    accepts_all(
        &gr,
        &json!({"query": "x", "where": {"root": "/", "kinds": ["file", "dir"]}}),
    );
    // The nested object's own required field is enforced at depth.
    assert!(!gr.accepts(r#"{"query":"x","where":{"depth":3}}"#));
    assert!(!gr.accepts(r#"{"query":"x","where":{"root":"/","kinds":["socket"]}}"#));
}

#[test]
fn refs_and_unions_resolve() {
    let gr = g(json!({
        "$defs": {
            "point": {
                "type": "object",
                "properties": {"x": {"type": "number"}, "y": {"type": "number"}},
                "required": ["x", "y"],
                "additionalProperties": false
            }
        },
        "type": "object",
        "properties": {
            "at": {"$ref": "#/$defs/point"},
            "name": {"type": ["string", "null"]}
        },
        "required": ["at"],
        "additionalProperties": false
    }));
    accepts_all(&gr, &json!({"at": {"x": 1, "y": -2.5}}));
    accepts_all(&gr, &json!({"at": {"x": 0, "y": 0}, "name": "here"}));
    accepts_all(&gr, &json!({"at": {"x": 0, "y": 0}, "name": null}));
    assert!(!gr.accepts(r#"{"at":{"x":1}}"#));
    assert!(!gr.accepts(r#"{"at":{"x":1,"y":2},"name":7}"#));
}

#[test]
fn numbers_are_json_numbers_and_not_whatever_the_model_felt_like() {
    let gr = g(json!({"type": "number"}));
    for ok in ["0", "-1", "3.5", "1e9", "-2.5E-3", "10"] {
        assert!(gr.accepts(ok), "{ok}");
    }
    for bad in ["01", "+1", ".5", "1.", "NaN", "Infinity", "0x10"] {
        assert!(!gr.accepts(bad), "{bad}");
    }
}

// ---------------------------------------------------------------------------
// The registry as a whole
// ---------------------------------------------------------------------------

#[test]
fn the_fleet_tools_produce_a_grammar_that_correlates_name_with_arguments() {
    let tools = NxTools::new("/nonexistent/nx");
    let ds = tools.descriptors();
    let src = tool_grammar(ds.iter(), &GrammarOptions::TOOL_ONLY).expect("grammar");
    let gr = Grammar::parse(&src).unwrap_or_else(|e| panic!("{e}\n{src}"));

    // Written out rather than round-tripped through `serde_json`, because a
    // `serde_json::Map` sorts its keys and would put `arguments` before `name`.
    // The grammar deliberately fixes `name` first: the model commits to a tool
    // and only then is constrained to *that* tool's argument schema. There is
    // no useful grammar in which the arguments come first.
    for ok in [
        r#"{"name":"nx_stack_start","arguments":{"stack":"vr"}}"#,
        r#"{"name": "nx_stack_start", "arguments": {"stack": "vr"}}"#,
        "{\n  \"name\": \"nx_stack_start\",\n  \"arguments\": {\n    \"stack\": \"vr\"\n  }\n}",
        r#"{"name":"nx_status","arguments":{}}"#,
        r#"{"name": "nx_status", "arguments": { }}"#,
        r#"{"name":"nx_doctor","arguments":{"offline":true}}"#,
        r#"{"name":"nx_doctor","arguments":{}}"#,
    ] {
        assert!(gr.accepts(ok), "should accept:\n{ok}\n---\n{src}");
    }
    let call = json!({"name": "nx_stack_start", "arguments": {"stack": "vr"}});

    // The whole point: `nx_status` has no `stack` argument, and no amount of
    // sampling can give it one.
    assert!(!gr.accepts(r#"{"name":"nx_status","arguments":{"stack":"vr"}}"#));
    // A tool that does not exist is not in the language either.
    assert!(!gr.accepts(r#"{"name":"rm_rf","arguments":{}}"#));
    // And `arguments` is not optional.
    assert!(!gr.accepts(r#"{"name":"nx_status"}"#));

    // Anything the grammar accepts parses back into the type we act on, with
    // no error path to get wrong.
    let parsed: ToolCall = serde_json::from_str(&serde_json::to_string(&call).expect("json"))
        .expect("a constrained decoder's output always parses");
    assert_eq!(parsed.name, "nx_stack_start");
}

#[test]
fn speech_is_structured_too_so_nothing_can_leak_out_as_free_text() {
    let src = reply_grammar(&GrammarOptions::default()).expect("grammar");
    let gr = Grammar::parse(&src).expect("parses");
    assert!(gr.accepts(r#"{"say": "your shaders are done"}"#));
    // SPEC §3.4: she does not get to emit prose directly. Even her speech comes
    // back inside a shape `wisp-mind` has to hand to `wisp-attn` as an
    // `Utterance` before anyone hears it.
    assert!(!gr.accepts("your shaders are done"));
    assert!(!gr.accepts(r#"Sure! Here you go: {"say":"hi"}"#));
}

#[test]
fn the_shortest_string_a_grammar_accepts_is_something_it_accepts() {
    let tools = NxTools::new("/nonexistent/nx");
    let ds = tools.descriptors();
    for opts in [GrammarOptions::TOOL_ONLY, GrammarOptions::default()] {
        let src = tool_grammar(ds.iter(), &opts).expect("grammar");
        let gr = Grammar::parse(&src).expect("parses");
        let s = gr.shortest().expect("a finite derivation exists");
        assert!(gr.accepts(&s), "shortest {s:?} was not accepted");
        // And it is valid JSON, because every alternative in the grammar is.
        serde_json::from_str::<Value>(&s).unwrap_or_else(|e| panic!("{s:?}: {e}"));
    }
}

#[test]
fn a_closed_label_set_leaves_no_room_for_maybe() {
    let src = enum_grammar(&["answer", "escalate", "unsure"]).expect("grammar");
    let gr = Grammar::parse(&src).expect("parses");
    assert!(gr.accepts("\"escalate\""));
    assert!(!gr.accepts("\"probably escalate\""));
    assert!(!gr.accepts("escalate"));
}

#[test]
fn a_prefix_check_answers_the_question_a_decoder_asks_every_token() {
    let src = reply_grammar(&GrammarOptions::default()).expect("grammar");
    let gr = Grammar::parse(&src).expect("parses");
    for p in ["", "{", "{\"say", "{\"say\": \"hel"] {
        assert!(gr.accepts_prefix(p), "should still be reachable: {p:?}");
    }
    assert!(!gr.accepts_prefix("["), "a decoder must never get here");
    assert!(!gr.accepts_prefix("{\"nope"));
}

#[test]
fn a_tool_whose_schema_cannot_be_constrained_is_refused_rather_than_offered_loosely() {
    // `additionalProperties: false` with no properties at all is a legal, empty
    // object — fine. A pattern-only string is not something GBNF can express
    // here, and offering it unconstrained would quietly defeat F14.
    let bad = json!({"type": "object", "properties": {"x": {"type": "duration"}}});
    let err = schema_grammar(&bad).unwrap_err();
    assert!(err.to_string().contains("duration"), "{err}");
}

#[test]
fn descriptors_from_two_sources_can_share_one_grammar() {
    // Nothing about the grammar builder knows where a descriptor came from,
    // which is what lets `wisp-mind` merge its own tools with `wisp-fleet`'s.
    let mine = ToolDescriptor {
        name: "timer_set",
        description: "Set a timer.",
        consent: wisp_proto::Consent::Ambient,
        parameters: json!({
            "type": "object",
            "properties": {"minutes": {"type": "integer"}, "label": {"type": "string"}},
            "required": ["minutes"],
            "additionalProperties": false
        }),
        invoke: std::sync::Arc::new(|_| {
            Box::pin(async {
                wisp_proto::ToolOutcome {
                    ok: true,
                    unavailable: false,
                    summary: String::new(),
                    json: None,
                    exit_code: None,
                }
            })
        }),
    };
    let tools = NxTools::new("/nonexistent/nx");
    let mut all = tools.descriptors();
    all.push(mine);
    let src = tool_grammar(all.iter(), &GrammarOptions::TOOL_ONLY).expect("grammar");
    let gr = Grammar::parse(&src).expect("parses");
    assert!(gr.accepts(r#"{"name":"timer_set","arguments":{"minutes":5}}"#));
    assert!(gr.accepts(r#"{"name":"nx_status","arguments":{}}"#));
    assert!(!gr.accepts(r#"{"name":"timer_set","arguments":{"stack":"vr"}}"#));
}
