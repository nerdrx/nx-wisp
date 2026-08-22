//! **SPEC §3.4, checked mechanically.**
//!
//! > Nothing reaches the operator except as an `Utterance` submitted to
//! > `wisp-attn`, which holds the token bucket. `wisp-mind` may not speak
//! > directly.
//!
//! Every other test in this suite checks that the *right* thing comes out of
//! [`wisp_mind::mind::Mind::take_outbox`]. This one checks that nothing comes
//! out anywhere else, by reading the crate's own source. It is a blunt
//! instrument on purpose: the failure it guards against is a `println!` added
//! during a debugging session and forgotten, and no amount of behavioural
//! testing catches that — the offending line only runs on the machine where it
//! was written, at four in the morning.
//!
//! The same sweep covers SPEC §0.2: this crate is allowed exactly two kinds of
//! egress — pinned model downloads and the operator-enabled Claude Code CLI —
//! so anything that looks like a third is a finding.

use std::path::{Path, PathBuf};

fn sources() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    walk(&root, &mut out);
    assert!(out.len() > 10, "did not find the crate's own sources");
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).expect("read src").flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Lines of real code, with comments and doc comments dropped — those are
/// allowed to *mention* anything.
fn code_lines(path: &Path) -> Vec<(usize, String)> {
    let text = std::fs::read_to_string(path).expect("read source");
    let mut out = Vec::new();
    let mut in_block_comment = false;
    for (i, raw) in text.lines().enumerate() {
        let mut line = raw.trim().to_string();
        if in_block_comment {
            match line.find("*/") {
                Some(j) => {
                    in_block_comment = false;
                    line = line[j + 2..].trim().to_string();
                }
                None => continue,
            }
        }
        if let Some(j) = line.find("/*") {
            in_block_comment = !line[j..].contains("*/");
            line = line[..j].trim().to_string();
        }
        if line.starts_with("//") {
            continue;
        }
        if let Some(j) = line.find("//") {
            // Crude, and deliberately so: a `//` inside a string literal makes
            // this drop too much, which can only ever produce a false *pass*
            // for that one line, never a false failure.
            line = line[..j].trim().to_string();
        }
        if line.is_empty() {
            continue;
        }
        out.push((i + 1, line));
    }
    out
}

#[test]
fn nothing_in_this_crate_can_print() {
    // `wisp-mind` reaches the operator through `wisp-attn` or not at all.
    // Diagnostics go to `tracing`, which the binary routes to the log file.
    const BANNED: [&str; 6] = [
        "println!",
        "eprintln!",
        "print!(",
        "eprint!(",
        "std::io::stdout",
        "std::io::stderr",
    ];
    let mut found = Vec::new();
    for path in sources() {
        for (n, line) in code_lines(&path) {
            for b in BANNED {
                if line.contains(b) {
                    found.push(format!("{}:{n}: {line}", path.display()));
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "SPEC §3.4 — she does not speak, she proposes. Found:\n{}",
        found.join("\n")
    );
}

#[test]
fn nothing_in_this_crate_can_notify_or_draw() {
    // The visible tell of SPEC §0.3 is a callback the binary wires to the rig;
    // this crate never touches a surface, a notification bus, or a terminal.
    const BANNED: [&str; 6] = [
        "notify_rust",
        "libnotify",
        "org.freedesktop.Notifications",
        "wl_surface",
        "wgpu::",
        "zwlr_layer_shell",
    ];
    let mut found = Vec::new();
    for path in sources() {
        for (n, line) in code_lines(&path) {
            for b in BANNED {
                if line.contains(b) {
                    found.push(format!("{}:{n}: {line}", path.display()));
                }
            }
        }
    }
    assert!(found.is_empty(), "found:\n{}", found.join("\n"));
}

#[test]
fn the_only_egress_is_the_model_fetcher_and_the_opt_in_cli() {
    // SPEC §0.2. `ureq` may appear only in `fetch.rs`; a process spawn may
    // appear only where a documented external tool is invoked.
    let mut network = Vec::new();
    for path in sources() {
        let name = path
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        for (n, line) in code_lines(&path) {
            if line.contains("ureq") && name != "fetch.rs" {
                network.push(format!("{}:{n}: {line}", path.display()));
            }
            if (line.contains("TcpStream") || line.contains("UdpSocket"))
                || line.contains("reqwest")
            {
                network.push(format!("{}:{n}: {line}", path.display()));
            }
            if line.contains("Command::new") && name != "escalate.rs" {
                network.push(format!(
                    "{}:{n}: spawns a process outside the documented CLI hop: {line}",
                    path.display()
                ));
            }
        }
    }
    assert!(
        network.is_empty(),
        "SPEC §0.2 allows model downloads and operator-enabled tools, nothing else:\n{}",
        network.join("\n")
    );
}

#[test]
fn every_test_in_this_suite_isolates_its_config_dir() {
    // SPEC §4, applied to the suite itself. Every integration test file either
    // builds a `Fixture` (which holds an `Isolated`) or has no state to isolate.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut offenders = Vec::new();
    for e in std::fs::read_dir(&root).expect("read tests").flatten() {
        let p = e.path();
        if !p.extension().is_some_and(|x| x == "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&p).expect("read test");
        let touches_state = text.contains("Memory::open")
            || text.contains("Mind::builder")
            || text.contains("with_state_file")
            || text.contains("models_dir");
        let isolates = text.contains("Fixture") || text.contains("Isolated");
        if touches_state && !isolates {
            offenders.push(p.display().to_string());
        }
    }
    assert!(
        offenders.is_empty(),
        "these tests touch state without isolating NX_WISP_CONFIG_DIR:\n{}",
        offenders.join("\n")
    );
}
