//! `nx-wisp models` — what she could think with, and fetching it.
//!
//! Models are not in the AppImage (F55): they are pinned by name, URL, size
//! and SHA-256 in a registry, and downloaded only when the operator says so.
//! Running `models fetch` IS the saying-so — SPEC §0.2(a) wants explicit
//! consent for egress, and typing the command is as explicit as it gets. The
//! config flag `model.allow_downloads` governs what the RUNNING app may do
//! unattended; it is deliberately not consulted here.

use wisp_mind::fetch::{Fetched, Fetcher, Progress};
use wisp_mind::backend::Role;
use wisp_mind::models::ModelRegistry;

use crate::config;

pub fn status(dir: &std::path::Path) -> String {
    let cfg = config::load_from(dir).config;
    let reg = ModelRegistry::load_or_builtin(&cfg.model.registry);
    let mut s = String::new();
    s.push_str("What she can think and speak with.\n\n");
    for role in [Role::Reflex, Role::Deliberate, Role::Embed] {
        for e in reg.for_role(role) {
            let here = e.looks_present(&cfg.model.models_dir);
            s.push_str(&format!(
                "  {:<11} {:<34} {:>6} MiB   {}\n",
                format!("{role:?}").to_lowercase(),
                e.name,
                e.size_mib(),
                if here { "here" } else { "not fetched" },
            ));
        }
    }
    let missing = reg.first_run_bytes() / (1024 * 1024);
    s.push_str(&format!(
        "\nA first `models fetch` moves about {missing} MiB, once, from pinned\n\
         URLs with pinned hashes. Nothing is fetched unless you run it.\n"
    ));
    s
}

pub fn fetch(dir: &std::path::Path, names: &[String]) -> i32 {
    let cfg = config::load_from(dir).config;
    let reg = ModelRegistry::load_or_builtin(&cfg.model.registry);

    let wanted: Vec<_> = if names.is_empty() {
        // The defaults for each role — what `run` will actually load.
        [Role::Reflex, Role::Deliberate, Role::Embed]
            .into_iter()
            .filter_map(|r| reg.default_for(r))
            .collect()
    } else {
        let mut v = Vec::new();
        for n in names {
            match reg.get(n) {
                Some(e) => v.push(e),
                None => {
                    eprintln!("No model called {n:?} in the registry. `nx-wisp models` lists them.");
                    return 1;
                }
            }
        }
        v
    };

    // Typing the command is the consent (SPEC §0.2a).
    let fetcher = Fetcher::real(true);
    let mut last_name = String::new();
    let mut progress = |p: Progress| {
        if p.name != last_name {
            last_name = p.name.clone();
            println!("{}  ({} MiB)", p.name, p.total_bytes / (1024 * 1024));
        }
        let pct = (p.fraction() * 100.0) as u32;
        if pct % 10 == 0 {
            print!("\r  {pct:>3}%");
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    };

    let mut failed = false;
    for (name, r) in fetcher.ensure_all(&wanted, &cfg.model.models_dir, &mut progress) {
        println!();
        match r {
            Ok(Fetched::AlreadyPresent(_)) => println!("  {name}: already here, hash verified"),
            Ok(Fetched::Downloaded { resumed_from: 0, .. }) => {
                println!("  {name}: fetched, hash verified")
            }
            Ok(Fetched::Downloaded { .. }) => {
                println!("  {name}: resumed and finished, hash verified")
            }
            Err(e) => {
                println!("  {name}: {e}");
                failed = true;
            }
        }
    }
    if failed { 1 } else { 0 }
}
