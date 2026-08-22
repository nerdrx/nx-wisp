//! Live smoke test. Not a test — a human runs this and watches.
//!
//! ```text
//!   source env.sh
//!   cargo run -p wisp-senses --example smoke -- --seconds 20
//! ```
//!
//! It starts every sense the operator has consented to, prints each
//! `Observation` as it arrives, and finishes with a summary — including the
//! achieved KWin geometry update rate, which is the number this crate exists to
//! make good.
//!
//! It writes its consent state to a temporary directory by default, so running
//! it can never disturb the operator's real choices (SPEC §4). Pass `--real`
//! to use the actual config, and `--clipboard` to switch the invasive sense on
//! for the duration of the run so the tell can be seen going up and down.
//!
//! What it changes on the machine: it loads a KWin script at runtime and
//! unloads it again at the end. No setting is written, no window is created.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use wisp_proto::{Event, EventKind, Observation, SenseId};
use wisp_senses::{kwin, Senses, SensesConfig};

struct Args {
    seconds: u64,
    real_config: bool,
    clipboard: bool,
    watch: Vec<PathBuf>,
    /// How long the KWin script coalesces geometry before one D-Bus call.
    /// 0 means "send every change" — useful for finding the transport ceiling.
    /// `None` lets the governor's budget decide, which is what the app does.
    flush_ms: Option<u32>,
}

fn parse_args() -> Args {
    let mut a = Args {
        seconds: 10,
        real_config: false,
        clipboard: false,
        watch: Vec::new(),
        flush_ms: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--seconds" | "-s" => {
                a.seconds = it.next().and_then(|v| v.parse().ok()).unwrap_or(10);
            }
            "--flush-ms" => {
                a.flush_ms = it.next().and_then(|v| v.parse().ok());
            }
            "--real" => a.real_config = true,
            "--clipboard" => a.clipboard = true,
            "--watch" => {
                if let Some(p) = it.next() {
                    a.watch.push(PathBuf::from(p));
                }
            }
            "--help" | "-h" => {
                eprintln!(
                    "smoke [--seconds N] [--flush-ms N] [--real] [--clipboard] [--watch DIR]...\n\
                     \n\
                     Prints observations from the live desktop for a few seconds.\n\
                     Uses a temporary consent directory unless --real is given."
                );
                std::process::exit(0);
            }
            other => eprintln!("ignoring unknown argument {other}"),
        }
    }
    a
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();
    let args = parse_args();

    // SPEC §4: never write into the operator's real state unless asked.
    let _tmp = if args.real_config {
        println!("using the real consent directory: {}", wisp_senses::consent::config_dir().display());
        None
    } else {
        let d = std::env::temp_dir().join(format!("nx-wisp-smoke-{}", std::process::id()));
        std::fs::create_dir_all(&d)?;
        std::env::set_var("NX_WISP_CONFIG_DIR", &d);
        println!("using a throwaway consent directory: {}", d.display());
        Some(d)
    };

    let mut senses = Senses::new();

    if args.clipboard {
        println!("enabling the clipboard sense for this run — it is INVASIVE");
        senses.ledger().set_enabled(SenseId::Clipboard, true)?;
    }

    println!("\nconsent panel (F30):");
    println!("  {:<18} {:<9} {:<8} used today", "sense", "consent", "enabled");
    for row in senses.ledger().rows() {
        println!(
            "  {:<18} {:<9?} {:<8} {}",
            row.label,
            row.consent,
            if row.enabled { "yes" } else { "no" },
            row.uses_today
        );
    }

    let cfg = SensesConfig {
        terrain: kwin::TerrainConfig { flush_ms: args.flush_ms, ..Default::default() },
        watch_dirs: args.watch.clone(),
        ..Default::default()
    };
    let stats = senses.terrain_stats();
    let mut rx = senses.subscribe();
    senses.start_all(&cfg);

    println!(
        "\nlistening for {}s — move a window, switch desktop, play something\n",
        args.seconds
    );

    let mut counts: std::collections::BTreeMap<String, u64> = Default::default();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(args.seconds);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Err(_) => break,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                println!("  ... {n} events dropped, the printer could not keep up");
            }
            Ok(Err(_)) => break,
            Ok(Ok(ev)) => {
                *counts.entry(describe_kind(&ev)).or_default() += 1;
                print_event(&ev);
            }
        }
    }

    println!("\nstopping…");
    let tier = senses.tier();
    senses.shutdown().await;

    println!("\n--- summary ---");
    for (kind, n) in &counts {
        println!("  {kind:<22} {n}");
    }

    let batches = stats.batches.load(Ordering::Relaxed);
    println!("\nKWin terrain feed:");
    println!("  batches           {batches}");
    println!("  window updates    {}", stats.window_updates.load(Ordering::Relaxed));
    println!("  focus changes     {}", stats.focus_changes.load(Ordering::Relaxed));
    println!("  KWin reconnects   {}", stats.reconnects.load(Ordering::Relaxed));
    if batches >= 2 {
        let coalesce = match args.flush_ms {
            Some(f) => format!("{f} ms, pinned"),
            None => format!("{} ms, the governor's budget at {tier:?}",
                wisp_senses::budget::terrain_flush_ms(tier)),
        };
        println!("  achieved rate     {:.1} batches/s", stats.batches_per_second());
        println!("  coalescing        {coalesce}");
    } else {
        println!("  achieved rate     n/a — nothing moved, try dragging a window");
    }
    Ok(())
}

fn describe_kind(ev: &Event) -> String {
    match &ev.kind {
        EventKind::Sensed(o) => format!("{:?}", o.sense()),
        EventKind::InvasiveActive { sense, .. } => format!("InvasiveActive({sense:?})"),
        other => format!("{other:?}"),
    }
}

fn print_event(ev: &Event) {
    let t = ev.at;
    match &ev.kind {
        EventKind::InvasiveActive { sense, active } => {
            println!(
                "{t:>7}ms  *** {sense:?} is {} — the character must show a visible tell ***",
                if *active { "LIVE" } else { "off" }
            );
        }
        EventKind::Sensed(o) => match o {
            Observation::Focus { app_id, title } => {
                println!("{t:>7}ms  focus     {app_id} — {}", truncate(title, 60));
            }
            Observation::Window { id, x, y, w, h, gone: false } => {
                println!("{t:>7}ms  terrain   #{id} at {x},{y} {w}x{h}");
            }
            Observation::Window { id, gone: true, .. } => {
                println!("{t:>7}ms  terrain   #{id} gone");
            }
            Observation::Idle { idle, for_ms } => {
                println!("{t:>7}ms  idle      {idle} after {for_ms}ms");
            }
            Observation::Media { player, title, artist, playing } => {
                println!(
                    "{t:>7}ms  media     [{player}] {} {} — {}",
                    if *playing { "▶" } else { "❚❚" },
                    truncate(title, 50),
                    artist
                );
            }
            Observation::AudioLevel { out, mic_live } => {
                let bar = "#".repeat((*out as usize) / 5);
                println!(
                    "{t:>7}ms  audio     {out:>3} |{bar:<20}| mic {}",
                    if *mic_live { "LIVE" } else { "off" }
                );
            }
            Observation::Notification { app, summary, body } => {
                println!("{t:>7}ms  notify    [{app}] {summary} — {}", truncate(body, 50));
            }
            Observation::Vitals { cpu_pct, gpu_pct, vram_used_mib, temp_c, on_battery } => {
                println!(
                    "{t:>7}ms  vitals    cpu {cpu_pct}% gpu {gpu_pct}% vram {vram_used_mib} MiB {temp_c}C battery {on_battery}"
                );
            }
            Observation::Workspace { index, name } => {
                println!("{t:>7}ms  desktop   {index} \"{name}\"");
            }
            Observation::Files { path, dirty } => {
                println!(
                    "{t:>7}ms  files     {path} is {}",
                    if *dirty { "dirty" } else { "clean" }
                );
            }
            Observation::Clipboard { len, kind } => {
                println!("{t:>7}ms  clipboard {len} bytes of {kind} (contents never read)");
            }
            Observation::Speech { .. } | Observation::Fleet { .. } => {
                println!("{t:>7}ms  {o:?}");
            }
        },
        other => println!("{t:>7}ms  {other:?}"),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Sense-level warnings ("ext_idle_notifier_v1 is not available", "KWin came
/// back") matter a great deal when a human is checking this against a real
/// desktop, so they go to stderr alongside the observations.
fn init_logging() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("wisp_senses=info")),
        )
        .init();
}
