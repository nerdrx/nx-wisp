//! F46 — the `nx` wrappers, with and without an `nx` to wrap.
//!
//! The important case is the *absent* one: NX Hub is optional, and "there is no
//! CLI here" has to be an ordinary, quiet, recorded outcome rather than an
//! error path. The present case is exercised against a stub `nx` that prints
//! what the real one prints, so these tests never touch the operator's fleet.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use wisp_fleet::tools::NxTools;
use wisp_proto::Consent;

fn isolate() -> tempfile::TempDir {
    // SPEC §4.
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("NX_WISP_CONFIG_DIR", dir.path());
    dir
}

/// A stand-in `nx` that prints `stdout`, then exits with `code`.
fn stub_nx(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("nx");
    std::fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

#[tokio::test]
async fn with_no_nx_installed_every_tool_fails_safe_and_is_still_recorded() {
    let dir = isolate();
    let tools = NxTools::new(dir.path().join("definitely-not-installed"));
    assert!(!tools.available());

    for name in ["nx_status", "nx_stack_list", "nx_doctor"] {
        let out = tools.invoke(name, json!({})).await;
        assert!(!out.ok);
        assert!(out.unavailable, "{name} must report absence, not failure");
        assert!(out.summary.contains("not installed"), "{}", out.summary);
        assert!(out.json.is_none());
    }
    let out = tools.invoke("nx_stack_start", json!({"stack": "vr"})).await;
    assert!(out.unavailable);

    // SPEC §0.4: it happened, so it is in the record — including the attempts
    // that could not run.
    let log = tools.recent();
    assert_eq!(log.len(), 4);
    assert!(log.iter().all(|i| i.unavailable && !i.ok));
    assert_eq!(log[3].argv, vec!["stack", "run", "vr", "--json"]);
}

#[tokio::test]
async fn a_status_answer_is_summarised_and_handed_over_whole() {
    let dir = isolate();
    let nx = stub_nx(
        dir.path(),
        r#"#!/bin/sh
cat <<'JSON'
{"ok":true,
 "bus":{"host":"127.0.0.1","port":9021,"listening":true,"online":true},
 "clients":[{"app":"pulsenx","fields":{"hr":72,"connected":true}},
            {"app":"wivrn-nx","fields":{"session":false}}]}
JSON
"#,
    );
    let tools = NxTools::new(&nx);
    let out = tools.invoke("nx_status", json!({})).await;

    assert!(out.ok);
    assert!(!out.unavailable);
    assert_eq!(out.summary, "On the bus: pulsenx, wivrn-nx.");
    // The model gets the real thing, not our paraphrase of it.
    assert_eq!(out.json.unwrap()["clients"][0]["fields"]["hr"], json!(72));
}

#[tokio::test]
async fn starting_a_stack_wraps_the_cli_verb_that_actually_exists() {
    let dir = isolate();
    // `nx stack start` is not a thing — the CLI verb is `run`. The wrapper
    // translates, because "start my VR stack" is what she will be asked.
    let nx = stub_nx(dir.path(), "#!/bin/sh\necho \"$@\" >&2\necho '{\"ok\":true}'\n");
    let tools = NxTools::new(&nx);
    let out = tools.invoke("nx_stack_start", json!({"stack": "vr"})).await;
    assert!(out.ok, "{}", out.summary);
    assert_eq!(tools.recent()[0].argv, vec!["stack", "run", "vr", "--json"]);
}

#[tokio::test]
async fn a_failing_cli_reports_its_own_words_rather_than_ours() {
    let dir = isolate();
    let nx = stub_nx(dir.path(), "#!/bin/sh\necho 'nx: unknown stack \"vr\"' >&2\nexit 2\n");
    let tools = NxTools::new(&nx);
    let out = tools.invoke("nx_stack_start", json!({"stack": "vr"})).await;
    assert!(!out.ok);
    assert!(!out.unavailable, "it ran; it just said no");
    assert_eq!(out.exit_code, Some(2));
    assert_eq!(out.summary, "nx: unknown stack \"vr\"");
}

#[tokio::test]
async fn a_cli_that_hangs_is_given_up_on() {
    let dir = isolate();
    let nx = stub_nx(dir.path(), "#!/bin/sh\nsleep 30\n");
    let tools = NxTools::new(&nx).with_timeout(Duration::from_millis(250));
    let out = tools.invoke("nx_status", json!({})).await;
    assert!(!out.ok);
    assert!(out.summary.contains("took too long"));
}

#[tokio::test]
async fn the_recorder_sees_every_invocation() {
    let dir = isolate();
    let nx = stub_nx(dir.path(), "#!/bin/sh\necho '{\"stacks\":[{\"id\":\"vr\",\"name\":\"VR\"}]}'\n");
    let tools = NxTools::new(&nx);
    let seen = Arc::new(Mutex::new(Vec::new()));
    {
        let seen = Arc::clone(&seen);
        tools.on_record(move |i| seen.lock().unwrap().push(i));
    }

    let out = tools.invoke("nx_stack_list", json!({})).await;
    assert_eq!(out.summary, "Stacks: vr (VR).");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].name, "nx_stack_list");
    assert_eq!(seen[0].argv, vec!["stack", "ls", "--json"]);
    assert!(seen[0].ok);
}

#[tokio::test]
async fn a_stack_name_the_model_invented_cannot_become_a_flag() {
    let dir = isolate();
    let nx = stub_nx(dir.path(), "#!/bin/sh\necho '{}'\n");
    let tools = NxTools::new(&nx);
    for bad in ["--offline", "-f", "vr && reboot", "$(whoami)", "../../etc/passwd"] {
        let out = tools.invoke("nx_stack_start", json!({"stack": bad})).await;
        assert!(!out.ok, "{bad} should have been refused");
        assert!(!out.unavailable);
        assert!(tools.recent().last().unwrap().argv.is_empty(), "it never ran");
    }
}

#[tokio::test]
async fn the_descriptors_are_what_wisp_mind_needs_and_nothing_more() {
    let dir = isolate();
    let nx = stub_nx(dir.path(), "#!/bin/sh\necho '{\"ok\":true}'\n");
    let tools = NxTools::new(&nx);
    let descriptors = tools.descriptors();

    // Read-only local queries may run unprompted; anything that starts an app
    // or can touch the network needs the operator to have enabled it.
    let consent = |name: &str| {
        descriptors.iter().find(|d| d.name == name).unwrap().consent
    };
    assert_eq!(consent("nx_status"), Consent::Ambient);
    assert_eq!(consent("nx_stack_list"), Consent::Ambient);
    assert_eq!(consent("nx_stack_start"), Consent::Explicit);
    assert_eq!(consent("nx_doctor"), Consent::Explicit);

    // And a descriptor can be invoked without knowing what is behind it.
    let status = descriptors.iter().find(|d| d.name == "nx_status").unwrap();
    let out = (status.invoke)(json!({})).await;
    assert!(out.ok);
    assert_eq!(tools.recent().len(), 1, "invoking through the descriptor still records");
}

#[tokio::test]
async fn doctor_does_not_go_near_the_network_unless_asked() {
    let dir = isolate();
    // The stub echoes its argv into the JSON so the test can see the flags.
    let nx = stub_nx(
        dir.path(),
        "#!/bin/sh\nprintf '{\"errors\":[],\"argv\":\"%s\"}\\n' \"$*\"\n",
    );
    let tools = NxTools::new(&nx);

    let out = tools.invoke("nx_doctor", json!({})).await;
    assert!(out.ok);
    assert_eq!(out.summary, "Everything checks out.");
    assert!(
        out.json.unwrap()["argv"].as_str().unwrap().contains("--offline"),
        "SPEC §0.2: no egress she was not asked for"
    );

    let out = tools.invoke("nx_doctor", json!({"offline": false})).await;
    assert!(!out.json.unwrap()["argv"].as_str().unwrap().contains("--offline"));
}
