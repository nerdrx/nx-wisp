//! **F53 — the `wisp` CLI, driving exactly the same modules as the GUI.**
//!
//! NX Hub's pattern: there is no "CLI backend". `status` reads the same
//! [`state`] file the event loop writes, `log` and `explain` read the same
//! [`recorder`] the event loop appends to, `senses` goes through the same
//! [`wisp_senses::ConsentLedger`] the senses are gated on, and `tier` and
//! `config` write the same [`config`] file the running instance is watching.
//! Anything the CLI can do, it does by touching the real thing.
//!
//! [`state`]: crate::state
//! [`recorder`]: crate::recorder
//! [`config`]: crate::config
//!
//! # No argument-parsing dependency
//!
//! The grammar is small, closed and unlikely to grow, and this crate is the one
//! that ships in the AppImage. Hand-rolling it costs about two hundred lines and
//! buys a parser that is unit-tested against every subcommand, error messages
//! written in DESIGN.md §9's voice rather than a library's, and no dependency
//! tree. `wisp-fleet` made the same call about its WebSocket framing.
//!
//! # Voice
//!
//! Every string printed from here is English, short, concrete and sentence
//! case; errors say what happened and what to do next; there are no exclamation
//! marks and no chirpiness. That is asserted, not merely intended — see the
//! tests at the bottom and in [`crate::fmt`].

use std::path::PathBuf;
use std::time::Duration;

use wisp_proto::{Consent, SenseId, Tier};

use crate::config::{self, Config};
use crate::fmt;
use crate::recorder::{self, KindFilter};
use crate::{doctor, install, lock, state};

// ---------------------------------------------------------------------------
// The grammar
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Global {
    /// `--config-dir DIR`. Sets `NX_WISP_CONFIG_DIR` for this process, so every
    /// module resolves to the same place with no argument threading.
    pub config_dir: Option<PathBuf>,
    /// `--mock`.
    pub mock: bool,
    /// `-q` / `--quiet`: no log output on stderr.
    pub quiet: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Run(RunArgs),
    Status,
    Log(LogArgs),
    Explain,
    Senses(SensesCmd),
    Tier(TierCmd),
    Config(ConfigCmd),
    Doctor,
    Install(InstallArgs),
    Uninstall(InstallArgs),
    Version,
    Help(Option<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunArgs {
    /// `--for 30s`. Bounded runs are how CI exercises the loop.
    pub run_for: Option<Duration>,
    /// `--no-fleet` / `--fleet`.
    pub fleet: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogArgs {
    pub n: usize,
    pub kind: KindFilter,
    /// `--wall`: local clock times instead of offsets into the run.
    pub wall: bool,
}

impl Default for LogArgs {
    fn default() -> Self {
        LogArgs { n: 40, kind: KindFilter::All, wall: false }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SensesCmd {
    List,
    Enable(SenseId),
    Disable(SenseId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TierCmd {
    Show,
    Pin(Tier),
    Unpin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigCmd {
    Show,
    Path,
    Get(String),
    Set(String, String),
    Reset,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstallArgs {
    pub mode: install::Mode,
    pub dry_run: bool,
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub global: Global,
    pub command: Command,
}

/// What happened, and what to say about it. Errors carry the exit code so
/// `main` stays a one-liner.
#[derive(Debug)]
pub struct CliError {
    pub message: String,
    pub code: i32,
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

impl CliError {
    fn new(message: impl Into<String>) -> CliError {
        CliError { message: message.into(), code: 2 }
    }
    fn with_code(message: impl Into<String>, code: i32) -> CliError {
        CliError { message: message.into(), code }
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse the arguments *after* the program name.
pub fn parse<I, S>(args: I) -> Result<Invocation, CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut it = args.into_iter().map(Into::into).peekable();
    let mut global = Global::default();

    // The three global flags are accepted **wherever they appear**, before the
    // subcommand or after it. `nx-wisp run -q` and `nx-wisp --mock run` are
    // both things a person types, and rejecting one of them because of where
    // the word sits is the kind of pedantry that wastes an afternoon. A
    // subcommand's own flags stay its own, so `nx-wisp log --for 5s` is still
    // an error.
    let command = loop {
        let Some(arg) = it.next() else {
            // No subcommand at all means run her; that is what a desktop entry
            // and a systemd unit both invoke.
            break Command::Run(RunArgs::default());
        };
        if try_global(&arg, &mut it, &mut global)? {
            continue;
        }
        match arg.as_str() {
            "-h" | "--help" => break Command::Help(it.next()),
            "-V" | "--version" => break Command::Version,
            other if other.starts_with('-') => {
                return Err(unknown_flag(other, "nx-wisp"));
            }
            other => break parse_command(other, &mut it, &mut global)?,
        }
    };

    Ok(Invocation { global, command })
}

/// `true` if `arg` was a global flag and has been consumed.
fn try_global<I: Iterator<Item = String>>(
    arg: &str,
    it: &mut std::iter::Peekable<I>,
    global: &mut Global,
) -> Result<bool, CliError> {
    match arg {
        "--config-dir" => {
            let v = next_value(it, "--config-dir", "a directory")?;
            global.config_dir = Some(PathBuf::from(v));
        }
        "--mock" => global.mock = true,
        "-q" | "--quiet" => global.quiet = true,
        _ => return Ok(false),
    }
    Ok(true)
}

fn parse_command<I: Iterator<Item = String>>(
    name: &str,
    it: &mut std::iter::Peekable<I>,
    g: &mut Global,
) -> Result<Command, CliError> {
    match name {
        "run" | "start" => {
            let mut a = RunArgs::default();
            while let Some(arg) = it.next() {
                if try_global(&arg, it, g)? {
                    continue;
                }
                match arg.as_str() {
                    "--for" => {
                        let v = next_value(it, "--for", "a duration such as 30s or 5m")?;
                        a.run_for = Some(parse_duration(&v).ok_or_else(|| {
                            CliError::new(format!(
                                "--for takes a duration such as 30s, 5m or 2h; got {v:?}."
                            ))
                        })?);
                    }
                    "--fleet" => a.fleet = Some(true),
                    "--no-fleet" => a.fleet = Some(false),
                    other => return Err(unknown_flag(other, "run")),
                }
            }
            Ok(Command::Run(a))
        }
        "status" => {
            expect_end(it, "status", g)?;
            Ok(Command::Status)
        }
        "log" => {
            let mut a = LogArgs::default();
            while let Some(arg) = it.next() {
                if try_global(&arg, it, g)? {
                    continue;
                }
                match arg.as_str() {
                    "-n" | "--lines" => {
                        let v = next_value(it, "-n", "a number of lines")?;
                        a.n = v.parse().map_err(|_| {
                            CliError::new(format!("-n takes a number of lines; got {v:?}."))
                        })?;
                    }
                    "-k" | "--kind" => {
                        let v = next_value(it, "--kind", "one or more event kinds")?;
                        a.kind = KindFilter::parse(&v).map_err(CliError::new)?;
                    }
                    "--all" => a.n = usize::MAX,
                    "--wall" => a.wall = true,
                    other => return Err(unknown_flag(other, "log")),
                }
            }
            Ok(Command::Log(a))
        }
        "explain" | "why" => {
            expect_end(it, "explain", g)?;
            Ok(Command::Explain)
        }
        "senses" => {
            let sub = next_subcommand(it, g, "list")?;
            let cmd = match sub.as_str() {
                "list" | "ls" => SensesCmd::List,
                "enable" | "on" => SensesCmd::Enable(sense_arg(it, "enable")?),
                "disable" | "off" => SensesCmd::Disable(sense_arg(it, "disable")?),
                other => {
                    return Err(CliError::new(format!(
                        "`senses {other}` is not a thing. Try list, enable or disable."
                    )))
                }
            };
            expect_end(it, "senses", g)?;
            Ok(Command::Senses(cmd))
        }
        "tier" => {
            let sub = next_subcommand(it, g, "show")?;
            let cmd = match sub.as_str() {
                "show" => TierCmd::Show,
                "pin" => {
                    let v = next_value(it, "tier pin", "a tier such as T3")?;
                    TierCmd::Pin(fmt::parse_tier(&v).ok_or_else(|| {
                        CliError::new(format!(
                            "There is no tier called {v:?}. They are T0 Feral, T1 Full, \
                             T2 Reduced, T3 Lobotomised and T4 Dormant."
                        ))
                    })?)
                }
                "unpin" => TierCmd::Unpin,
                other => {
                    return Err(CliError::new(format!(
                        "`tier {other}` is not a thing. Try show, pin or unpin."
                    )))
                }
            };
            expect_end(it, "tier", g)?;
            Ok(Command::Tier(cmd))
        }
        "config" => {
            let sub = next_subcommand(it, g, "show")?;
            let cmd = match sub.as_str() {
                "show" => ConfigCmd::Show,
                "path" => ConfigCmd::Path,
                "get" => ConfigCmd::Get(next_value(it, "config get", "a setting name")?),
                "set" => {
                    // Positional, so a value that looks like a flag is still a
                    // value: `config set skin --mock` sets the skin to
                    // "--mock", which is wrong but is what was asked for.
                    let k = next_value(it, "config set", "a setting name")?;
                    let v = next_value(it, "config set", "a value")?;
                    ConfigCmd::Set(k, v)
                }
                "reset" => ConfigCmd::Reset,
                other => {
                    return Err(CliError::new(format!(
                        "`config {other}` is not a thing. Try show, get, set, path or reset."
                    )))
                }
            };
            expect_end(it, "config", g)?;
            Ok(Command::Config(cmd))
        }
        "doctor" | "check" => {
            expect_end(it, "doctor", g)?;
            Ok(Command::Doctor)
        }
        "install" | "uninstall" => {
            let mut a = InstallArgs::default();
            while let Some(arg) = it.next() {
                if try_global(&arg, it, g)? {
                    continue;
                }
                match arg.as_str() {
                    "--systemd" => a.mode = install::Mode::Systemd,
                    "--autostart" => a.mode = install::Mode::Autostart,
                    "--dry-run" | "-n" => a.dry_run = true,
                    "--force" => a.force = true,
                    other => return Err(unknown_flag(other, name)),
                }
            }
            Ok(if name == "install" { Command::Install(a) } else { Command::Uninstall(a) })
        }
        "version" => Ok(Command::Version),
        "help" => Ok(Command::Help(it.next())),
        other => Err(CliError::new(format!(
            "There is no command called {other}. Run `nx-wisp help` for the list."
        ))),
    }
}

/// The next word that is not a global flag, or `default` if there is none.
fn next_subcommand<I: Iterator<Item = String>>(
    it: &mut std::iter::Peekable<I>,
    g: &mut Global,
    default: &str,
) -> Result<String, CliError> {
    while let Some(arg) = it.next() {
        if try_global(&arg, it, g)? {
            continue;
        }
        return Ok(arg);
    }
    Ok(default.to_string())
}

fn sense_arg<I: Iterator<Item = String>>(
    it: &mut std::iter::Peekable<I>,
    verb: &str,
) -> Result<SenseId, CliError> {
    let v = next_value(it, &format!("senses {verb}"), "a sense name")?;
    config::parse_sense(&v).ok_or_else(|| {
        let names: Vec<&str> =
            wisp_senses::ALL_SENSES.iter().map(|&id| config::sense_key(id)).collect();
        CliError::new(format!(
            "There is no sense called {v}. The senses are: {}.",
            names.join(", ")
        ))
    })
}

fn next_value<I: Iterator<Item = String>>(
    it: &mut std::iter::Peekable<I>,
    flag: &str,
    want: &str,
) -> Result<String, CliError> {
    match it.next() {
        Some(v) => Ok(v),
        None => Err(CliError::new(format!("{flag} needs {want} after it."))),
    }
}

fn expect_end<I: Iterator<Item = String>>(
    it: &mut std::iter::Peekable<I>,
    cmd: &str,
    g: &mut Global,
) -> Result<(), CliError> {
    while let Some(arg) = it.next() {
        if try_global(&arg, it, g)? {
            continue;
        }
        return Err(CliError::new(format!(
            "`{cmd}` does not take {arg:?}. Run `nx-wisp help {cmd}` for what it does take."
        )));
    }
    Ok(())
}

fn unknown_flag(flag: &str, cmd: &str) -> CliError {
    CliError::new(format!(
        "{cmd} does not take {flag}. Run `nx-wisp help {cmd}` for the flags it does take."
    ))
}

/// `30s`, `5m`, `2h`, or a bare number of seconds.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    // `ms` first: its last character is also the one that means seconds.
    let (num, mult) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1u64)
    } else {
        match s.chars().last()? {
            's' => (&s[..s.len() - 1], 1_000),
            'm' => (&s[..s.len() - 1], 60_000),
            'h' => (&s[..s.len() - 1], 3_600_000),
            _ => (s, 1_000),
        }
    };
    let n: u64 = num.trim().parse().ok()?;
    Some(Duration::from_millis(n.checked_mul(mult)?))
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Run one invocation. Returns the process exit code.
///
/// Printing happens here rather than being returned as a string, because `log
/// --all` on a full recorder is megabytes and there is no reason to build it in
/// memory first.
pub fn dispatch(inv: Invocation) -> Result<i32, CliError> {
    // One place resolves the config dir, and it does so by setting the same
    // environment variable every module already reads. Nothing downstream needs
    // to know the flag exists.
    if let Some(dir) = &inv.global.config_dir {
        std::env::set_var("NX_WISP_CONFIG_DIR", dir);
    }
    let dir = config::config_dir();

    match inv.command {
        Command::Help(topic) => {
            print!("{}", help(topic.as_deref()));
            Ok(0)
        }
        Command::Version => {
            println!("nx-wisp {}", crate::VERSION);
            Ok(0)
        }
        Command::Run(args) => run(&dir, &inv.global, args),
        Command::Status => {
            print!("{}", status_text(&dir));
            Ok(0)
        }
        Command::Log(args) => {
            print!("{}", log_text(&dir, &args));
            Ok(0)
        }
        Command::Explain => {
            let (text, code) = explain_text(&dir);
            print!("{text}");
            Ok(code)
        }
        Command::Senses(cmd) => senses(&dir, cmd),
        Command::Tier(cmd) => tier(&dir, cmd),
        Command::Config(cmd) => config_cmd(&dir, cmd),
        Command::Doctor => {
            let checks = doctor::run(&doctor::Env::current());
            print!("{}", doctor::render(&checks));
            Ok(if doctor::worst(&checks) == doctor::Level::Fail { 1 } else { 0 })
        }
        Command::Install(args) => install_cmd(args),
        Command::Uninstall(args) => uninstall_cmd(args),
    }
}

fn run(dir: &std::path::Path, global: &Global, args: RunArgs) -> Result<i32, CliError> {
    let mut opts = crate::app::Options::new(dir.to_path_buf());
    opts.mock = global.mock;
    opts.fleet = args.fleet;
    opts.run_for = args.run_for;

    // A real run puts her on the compositor. `--mock` deliberately does not:
    // that mode exists so the loop can be exercised with no Wayland and no
    // GPU, and quietly opening a surface would defeat it.
    if !global.mock {
        let size = config::load_from(dir).config.appearance.size_px;
        match crate::shell_layer::LayerShellHost::new(size) {
            Ok(host) => opts = opts.with_shell(Box::new(host)),
            Err(e) => {
                // Say what is wrong and keep going headless rather than
                // refusing to start — `doctor` explains the environment, and
                // the CLI half of her still works.
                eprintln!("She cannot draw herself: {e}");
                eprintln!("Running without a body. `nx-wisp doctor` will say why.");
            }
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("nx-wisp")
        .build()
        .map_err(|e| CliError::with_code(format!("Could not start the runtime — {e}"), 1))?;

    match rt.block_on(crate::app::run(opts)) {
        Ok(summary) => {
            tracing::info!(?summary, "stopped");
            Ok(0)
        }
        Err(e) => Err(CliError::with_code(e.to_string(), 1)),
    }
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

/// Tier, cost, and what she can see.
///
/// Works whether or not she is running, and says which it is. A running
/// instance publishes [`state`]; a stopped one is described from the flight
/// recorder and the consent ledger, with a live governor probe for the tier she
/// *would* be at. Never a guess presented as a reading.
pub fn status_text(dir: &std::path::Path) -> String {
    let mut s = String::new();
    let cfg = config::load_from(dir);
    if let Some(note) = cfg.note() {
        s.push_str(&format!("{note}\n\n"));
    }
    let cfg = cfg.config;

    let running = lock::is_held(dir);
    let live = state::load(dir).filter(|st| !st.is_stale(crate::epoch_ms()));

    const W: usize = 12;
    match (&live, running) {
        (Some(st), _) => {
            s.push_str(&fmt::heading("She is running."));
            s.push_str(&fmt::row("pid", &st.pid.to_string(), W));
            s.push('\n');
            s.push_str(&fmt::row("tier", &format!("{} — {}", fmt::tier_label(st.tier), st.because), W));
            s.push('\n');
            s.push_str(&fmt::row("cost", &st.headline, W));
            s.push('\n');
            s.push_str(&fmt::row(
                "measured",
                &format!(
                    "{} MiB of RAM, {} of a core, {} MiB of VRAM on the discrete card",
                    st.measured_rss_mib,
                    fmt::cpu(st.measured_cpu_centi_pct),
                    st.dgpu_vram_mib
                ),
                W,
            ));
            s.push('\n');
            s.push_str(&fmt::row("chattiness", &st.chattiness, W));
            s.push('\n');
            if st.waiting > 0 {
                s.push_str(&fmt::row(
                    "waiting",
                    &format!("{} thing{} she has not found a moment for", st.waiting, if st.waiting == 1 { "" } else { "s" }),
                    W,
                ));
                s.push('\n');
            }
            if st.mock {
                s.push_str(&fmt::row("mode", "mock — fake senses, fake governor", W));
                s.push('\n');
            }
            if !st.by_subsystem.is_empty() {
                s.push_str("\nWhat each part claims at this tier\n");
                for (name, c) in &st.by_subsystem {
                    s.push_str(&fmt::row(
                        name,
                        &format!(
                            "{} MiB RAM, {} MiB VRAM, {} of a core",
                            c.ram_mib,
                            c.vram_mib,
                            fmt::cpu(c.cpu_centi_pct)
                        ),
                        W,
                    ));
                    s.push('\n');
                }
            }
        }
        (None, true) => {
            s.push_str(&fmt::heading(
                "She is running, but has not published her state yet. Try again in a moment.",
            ));
        }
        (None, false) => {
            s.push_str(&fmt::heading("She is not running."));
            s.push_str(&fmt::row("chattiness", config::chattiness_name(cfg.chattiness), W));
            s.push('\n');
            match cfg.tier.pinned {
                Some(t) => {
                    s.push_str(&fmt::row("tier", &format!("pinned to {}", fmt::tier_label(t)), W));
                }
                None => {
                    s.push_str(&fmt::row("tier", "not pinned; the governor decides", W));
                }
            }
            s.push('\n');
            if let Some(last) = recorder::tail_from(dir, cfg.recorder.keep, 4_000)
                .iter()
                .rev()
                .find(|r| r.tag() == "tier")
            {
                s.push_str(&fmt::row(
                    "last seen",
                    &format!("{} at {}", fmt::event(&last.kind), fmt::wall_clock(last.wall_ms())),
                    W,
                ));
                s.push('\n');
            }
        }
    }

    // What she can see. Straight from the ledger, running or not — this is the
    // answer to "what does she have access to", and it must not depend on
    // whether a process happens to be up.
    s.push_str("\nWhat she can see\n");
    let rows = config::sense_rows(dir);
    let width = rows.iter().map(|r| r.label.len()).max().unwrap_or(10);
    for r in &rows {
        let state_word = match (r.enabled, r.live) {
            (false, _) => "off".to_string(),
            (true, true) => "on, live now".to_string(),
            (true, false) => "on".to_string(),
        };
        let uses = if r.uses_today > 0 {
            format!(", used {} time{} today", r.uses_today, if r.uses_today == 1 { "" } else { "s" })
        } else {
            String::new()
        };
        let tell = if r.consent == Consent::Invasive { " [invasive]" } else { "" };
        s.push_str(&fmt::row(r.label, &format!("{state_word}{uses}{tell}"), width));
        s.push('\n');
    }

    let invasive_live: Vec<&str> =
        rows.iter().filter(|r| r.live && r.consent == Consent::Invasive).map(|r| r.label).collect();
    if !invasive_live.is_empty() {
        s.push_str(&format!(
            "\n{} {} live right now, and she is showing it.\n",
            invasive_live.join(" and "),
            if invasive_live.len() == 1 { "is" } else { "are" }
        ));
    }
    s
}

// ---------------------------------------------------------------------------
// log / explain
// ---------------------------------------------------------------------------

pub fn log_text(dir: &std::path::Path, args: &LogArgs) -> String {
    let keep = config::load_from(dir).config.recorder.keep;
    let records = recorder::filtered_from(dir, keep, &args.kind, args.n);
    if records.is_empty() {
        return match &args.kind {
            KindFilter::All => "Nothing has been recorded in this profile yet.\n".to_string(),
            KindFilter::Tags(t) => {
                format!("Nothing of kind {} has been recorded yet.\n", t.join(", "))
            }
        };
    }
    let mut s = String::new();
    let mut session = None;
    for r in &records {
        // A restart resets the run offset, so mark it rather than printing a
        // column that silently goes backwards.
        if session != Some(r.session) {
            if session.is_some() {
                s.push('\n');
            }
            s.push_str(&format!(
                "-- run started {} --\n",
                fmt::wall_clock(r.session)
            ));
            session = Some(r.session);
        }
        let line = if args.wall { r.line_wall() } else { r.line() };
        s.push_str(&line);
        s.push('\n');
    }
    s
}

pub fn explain_text(dir: &std::path::Path) -> (String, i32) {
    let prefs = config::load_from(dir).config.recorder;
    let records = recorder::tail_from(dir, prefs.keep, 8_000);
    match recorder::explain(&records, prefs.explain_window_ms) {
        Some(e) => (e.render(), 0),
        None => (
            "She has not said anything in this profile yet, so there is nothing to \
             explain. `nx-wisp log` shows everything that has happened.\n"
                .to_string(),
            0,
        ),
    }
}

// ---------------------------------------------------------------------------
// senses
// ---------------------------------------------------------------------------

fn senses(dir: &std::path::Path, cmd: SensesCmd) -> Result<i32, CliError> {
    match cmd {
        SensesCmd::List => {
            print!("{}", senses_list(dir));
            Ok(0)
        }
        SensesCmd::Enable(id) => set_sense(dir, id, true),
        SensesCmd::Disable(id) => set_sense(dir, id, false),
    }
}

pub fn senses_list(dir: &std::path::Path) -> String {
    let rows = config::sense_rows(dir);
    let width = rows.iter().map(|r| config::sense_key(r.id).len()).max().unwrap_or(10);
    let mut s = String::new();
    for r in &rows {
        s.push_str(&format!(
            "{}  {:<width$}  {:<10} {}\n",
            if r.enabled { "on " } else { "off" },
            config::sense_key(r.id),
            fmt::consent_word(r.consent),
            r.label
        ));
        s.push_str(&format!("     {:<width$}  {:<10} {}\n", "", "", r.description));
    }
    s.push_str(
        "\nAmbient senses ship on. Explicit and invasive ship off; an invasive one shows a \
         tell on her the whole time it is live.\n",
    );
    s
}

fn set_sense(dir: &std::path::Path, id: SenseId, on: bool) -> Result<i32, CliError> {
    let before = config::sense_rows(dir)
        .into_iter()
        .find(|r| r.id == id)
        .map(|r| r.enabled)
        .unwrap_or(false);

    config::set_sense_enabled(dir, id, on).map_err(|e| {
        CliError::with_code(
            format!("Could not save that choice to {} — {e}", dir.display()),
            1,
        )
    })?;

    let label = fmt::sense_label(id);
    if before == on {
        println!("{label} was already {}.", if on { "on" } else { "off" });
        return Ok(0);
    }
    if on && id.consent() == Consent::Invasive {
        println!(
            "{label} is on. It is invasive, so she will show a visible tell on herself for \
             the whole time it is live."
        );
    } else {
        println!("{label} is {}.", if on { "on" } else { "off" });
    }
    if lock::is_held(dir) {
        // Being honest about the limitation rather than letting the operator
        // believe it took effect: `wisp-senses` has no reload path.
        println!("She is running, and picks this up when she next starts.");
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// tier
// ---------------------------------------------------------------------------

fn tier(dir: &std::path::Path, cmd: TierCmd) -> Result<i32, CliError> {
    match cmd {
        TierCmd::Show => {
            print!("{}", tier_text(dir));
            Ok(0)
        }
        TierCmd::Pin(t) => {
            update_config(dir, |c| c.tier.pinned = Some(t))?;
            println!("Pinned to {} — {}.", fmt::tier_label(t), fmt::tier_meaning(t));
            if lock::is_held(dir) {
                println!("The running copy applies it within a couple of seconds.");
            }
            Ok(0)
        }
        TierCmd::Unpin => {
            update_config(dir, |c| c.tier.pinned = None)?;
            println!("Unpinned. The governor decides again.");
            if lock::is_held(dir) {
                println!("The running copy applies it within a couple of seconds.");
            }
            Ok(0)
        }
    }
}

pub fn tier_text(dir: &std::path::Path) -> String {
    let cfg = config::load_from(dir).config;
    let mut s = String::new();
    match state::load(dir).filter(|st| !st.is_stale(crate::epoch_ms())) {
        Some(st) => {
            s.push_str(&format!("{} — {}\n", fmt::tier_label(st.tier), st.because));
            s.push_str(&format!("  {}\n", fmt::tier_meaning(st.tier)));
        }
        None => s.push_str("She is not running, so there is no current tier.\n"),
    }
    match cfg.tier.pinned {
        Some(t) => s.push_str(&format!("\nPinned to {} by hand.\n", fmt::tier_label(t))),
        None => s.push_str("\nNot pinned; the governor decides.\n"),
    }
    s.push_str("\nThe ladder\n");
    for t in [Tier::Feral, Tier::Full, Tier::Reduced, Tier::Lobotomised, Tier::Dormant] {
        s.push_str(&fmt::row(&fmt::tier_label(t), fmt::tier_meaning(t), 16));
        s.push('\n');
    }
    s
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

fn config_cmd(dir: &std::path::Path, cmd: ConfigCmd) -> Result<i32, CliError> {
    match cmd {
        ConfigCmd::Path => {
            println!("{}", dir.join(config::CONFIG_FILE).display());
            Ok(0)
        }
        ConfigCmd::Show => {
            print!("{}", config_text(dir));
            Ok(0)
        }
        ConfigCmd::Get(key) => {
            let cfg = config::load_from(dir).config;
            match cfg.get(&key) {
                Some(v) => {
                    println!("{v}");
                    Ok(0)
                }
                None => Err(CliError::new(format!(
                    "There is no setting called {key}. Run `nx-wisp config show` for the list."
                ))),
            }
        }
        ConfigCmd::Set(key, value) => {
            let mut applied = None;
            update_config(dir, |c| {
                applied = Some(c.set(&key, &value));
            })?;
            match applied {
                Some(Ok(())) => {
                    let cfg = config::load_from(dir).config;
                    println!("{key} is now {}.", cfg.get(&key).unwrap_or(value));
                    if lock::is_held(dir) && matches!(key.as_str(), "chattiness" | "size" | "pinned_tier")
                    {
                        println!("The running copy applies it within a couple of seconds.");
                    } else if lock::is_held(dir) {
                        println!("She is running, and picks this up when she next starts.");
                    }
                    Ok(0)
                }
                Some(Err(e)) => Err(CliError::new(e.to_string())),
                None => unreachable!("the update closure always runs"),
            }
        }
        ConfigCmd::Reset => {
            config::save_to(dir, &Config::default()).map_err(write_error(dir))?;
            println!(
                "Reset to the defaults. Your consent choices and your flight recorder are \
                 untouched."
            );
            Ok(0)
        }
    }
}

pub fn config_text(dir: &std::path::Path) -> String {
    let loaded = config::load_from(dir);
    let mut s = String::new();
    if let Some(note) = loaded.note() {
        s.push_str(&format!("{note}\n\n"));
    }
    let width = config::KEYS.iter().map(|k| k.len()).max().unwrap_or(10);
    for key in config::KEYS {
        if let Some(v) = loaded.config.get(key) {
            s.push_str(&fmt::row(key, &v, width));
            s.push('\n');
        }
    }
    s.push_str(&format!("\n{}\n", dir.join(config::CONFIG_FILE).display()));
    s.push_str(
        "Which senses she may use is not here: `nx-wisp senses` shows and changes that.\n",
    );
    s
}

/// Read, change, write — atomically, and never on top of a config we failed to
/// parse. Editing a recovered default would silently discard the operator's
/// file, so a recovery is reported and the write goes ahead against the
/// defaults only because the old file has already been preserved beside it.
fn update_config(
    dir: &std::path::Path,
    f: impl FnOnce(&mut Config),
) -> Result<(), CliError> {
    let loaded = config::load_from(dir);
    if let Some(note) = loaded.note() {
        eprintln!("{note}");
    }
    let mut cfg = loaded.config;
    f(&mut cfg);
    config::save_to(dir, &cfg).map_err(write_error(dir))?;
    Ok(())
}

fn write_error(dir: &std::path::Path) -> impl Fn(std::io::Error) -> CliError + '_ {
    move |e| {
        CliError::with_code(format!("Could not write to {} — {e}", dir.display()), 1)
    }
}

// ---------------------------------------------------------------------------
// install / uninstall
// ---------------------------------------------------------------------------

fn install_cmd(args: InstallArgs) -> Result<i32, CliError> {
    let root = install::install_root();
    let exec = install::current_exe();
    let plan = install::plan(args.mode, &root, &exec);
    let opts = install::ApplyOptions {
        dry_run: args.dry_run,
        run_systemctl: args.mode == install::Mode::Systemd && !args.dry_run,
        force: args.force,
    };
    match install::apply(&plan, opts) {
        Ok(actions) => {
            for a in actions {
                println!("{a}");
            }
            Ok(0)
        }
        Err(e) => Err(CliError::with_code(e.to_string(), 1)),
    }
}

fn uninstall_cmd(args: InstallArgs) -> Result<i32, CliError> {
    let root = install::install_root();
    let opts = install::ApplyOptions {
        dry_run: args.dry_run,
        run_systemctl: !args.dry_run,
        force: args.force,
    };
    for a in install::uninstall(&root, opts) {
        println!("{a}");
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// help
// ---------------------------------------------------------------------------

pub fn help(topic: Option<&str>) -> String {
    match topic {
        Some("status") => "nx-wisp status\n\n\
             Which tier she is in and why, what she is measured to cost right now, and every\n\
             sense with whether it is on, whether it is live, and how many times it has been\n\
             used today. Works whether or not she is running, and says which.\n"
            .to_string(),
        Some("version") => "nx-wisp version\n\n\
             Print the version and exit. `--version` and `-V` do the same.\n"
            .to_string(),
        Some("log") => format!(
            "nx-wisp log [-n N] [--kind K[,K...]] [--all] [--wall]\n\n\
             The flight recorder, oldest first. Every event she has published is here,\n\
             recorded before it was dispatched.\n\n  \
             -n N        how many lines. Default 40.\n  \
             --kind K    only these kinds: {}\n  \
             --all       everything the recorder still holds.\n  \
             --wall      local clock times instead of offsets into the run.\n",
            recorder::TAGS.join(", ")
        ),
        Some("explain") => "nx-wisp explain\n\n\
             Why she said the last thing she said: the proposal it came from, the tier she\n\
             was in, what the senses had reported just before, and what she chose not to say\n\
             instead. All of it read back from the flight recorder, none of it reconstructed.\n"
            .to_string(),
        Some("senses") => "nx-wisp senses [list]\n\
             nx-wisp senses enable <sense>\n\
             nx-wisp senses disable <sense>\n\n\
             What she is allowed to notice. Ambient senses ship on; explicit and invasive\n\
             ship off. An invasive sense shows a tell on her for the whole time it is live.\n\
             A running copy picks a change up when it next starts.\n"
            .to_string(),
        Some("tier") => "nx-wisp tier [show]\n\
             nx-wisp tier pin <T0..T4>\n\
             nx-wisp tier unpin\n\n\
             How much of the machine she is allowed to be. Pinning is written to the config,\n\
             so it survives a restart and a running copy applies it within a couple of\n\
             seconds.\n"
            .to_string(),
        Some("config") => format!(
            "nx-wisp config [show]\n\
             nx-wisp config get <key>\n\
             nx-wisp config set <key> <value>\n\
             nx-wisp config path\n\
             nx-wisp config reset\n\n\
             Settings\n{}\n\n\
             Which senses she may use is not here; `nx-wisp senses` owns that.\n",
            config::KEYS.iter().map(|k| format!("  {k}")).collect::<Vec<_>>().join("\n")
        ),
        Some("install") | Some("uninstall") => "nx-wisp install [--systemd|--autostart] [--dry-run] [--force]\n\
             nx-wisp uninstall [--dry-run]\n\n\
             Start her at login. The systemd unit is the default because it carries the CPU\n\
             and memory ceilings; the autostart entry is for a session without systemd and\n\
             has none. Installing both would start two copies, so it is refused.\n\n\
             Uninstall removes either. Your config, consent choices and flight recorder are\n\
             untouched.\n"
            .to_string(),
        Some("run") => "nx-wisp run [--for DURATION] [--no-fleet]\n\n\
             Start her. This is what the systemd unit and the autostart entry invoke, and\n\
             it is what happens with no subcommand at all.\n\n  \
             --for D     stop after 30s, 5m, 2h. Used by CI.\n  \
             --no-fleet  do not join the NX Connector bus.\n  \
             --mock      fake senses and a fake governor; no compositor, no GPU.\n"
            .to_string(),
        Some("doctor") => "nx-wisp doctor\n\n\
             Is this machine one she can run on: Wayland, KDE Plasma 6, layer-shell, Vulkan,\n\
             a writable config dir, and whether anything is already running. Exits non-zero\n\
             if something will stop her working.\n"
            .to_string(),
        Some(other) => format!(
            "There is no command called {other}. Run `nx-wisp help` for the list.\n"
        ),
        None => format!(
            "nx-wisp {} — a desktop companion who costs nothing when it matters.\n\n\
             Usage: nx-wisp [--config-dir DIR] [--mock] <command>\n\n\
             Commands\n  \
             run          start her. The default with no command.\n  \
             status       tier, what she costs, and what she can see.\n  \
             log          the flight recorder.\n  \
             explain      why she said the last thing she said.\n  \
             senses       what she is allowed to notice.\n  \
             tier         how much of the machine she may be.\n  \
             config       her settings.\n  \
             doctor       check this machine has what she needs.\n  \
             install      start her at login.\n  \
             uninstall    stop starting her at login.\n  \
             version      print the version.\n\n\
             Run `nx-wisp help <command>` for any of them.\n\n\
             NX_WISP_CONFIG_DIR overrides where everything she keeps lives — her config, her\n\
             consent choices, her flight recorder and her single-instance lock. Set it to run\n\
             a second profile without touching the first.\n",
            crate::VERSION
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempConfig;
    use wisp_attn::Chattiness;

    fn p(args: &[&str]) -> Invocation {
        parse(args.iter().map(|s| s.to_string())).unwrap()
    }

    fn err(args: &[&str]) -> String {
        parse(args.iter().map(|s| s.to_string())).unwrap_err().message
    }

    #[test]
    fn every_command_parses() {
        assert_eq!(p(&["status"]).command, Command::Status);
        assert_eq!(p(&["doctor"]).command, Command::Doctor);
        assert_eq!(p(&["explain"]).command, Command::Explain);
        assert_eq!(p(&["why"]).command, Command::Explain);
        assert_eq!(p(&["version"]).command, Command::Version);
        assert_eq!(p(&["senses"]).command, Command::Senses(SensesCmd::List));
        assert_eq!(
            p(&["senses", "enable", "clipboard"]).command,
            Command::Senses(SensesCmd::Enable(SenseId::Clipboard))
        );
        assert_eq!(p(&["tier"]).command, Command::Tier(TierCmd::Show));
        assert_eq!(p(&["tier", "pin", "T3"]).command, Command::Tier(TierCmd::Pin(Tier::Lobotomised)));
        assert_eq!(p(&["tier", "unpin"]).command, Command::Tier(TierCmd::Unpin));
        assert_eq!(p(&["config"]).command, Command::Config(ConfigCmd::Show));
        assert_eq!(
            p(&["config", "set", "chattiness", "silent"]).command,
            Command::Config(ConfigCmd::Set("chattiness".into(), "silent".into()))
        );
        assert_eq!(p(&["install"]).command, Command::Install(InstallArgs::default()));
        assert_eq!(
            p(&["install", "--autostart", "--dry-run"]).command,
            Command::Install(InstallArgs {
                mode: install::Mode::Autostart,
                dry_run: true,
                force: false
            })
        );
    }

    #[test]
    fn no_arguments_at_all_runs_her() {
        // This is what the systemd unit and the desktop entry rely on.
        assert_eq!(p(&[]).command, Command::Run(RunArgs::default()));
    }

    #[test]
    fn global_flags_are_accepted_on_either_side_of_the_subcommand() {
        let before = p(&["--config-dir", "/tmp/x", "--mock", "-q", "run", "--for", "5s"]);
        let after = p(&["run", "--for", "5s", "--config-dir", "/tmp/x", "--mock", "-q"]);
        let mixed = p(&["--mock", "run", "--config-dir", "/tmp/x", "--for", "5s", "-q"]);
        for inv in [&before, &after, &mixed] {
            assert_eq!(inv.global.config_dir, Some(PathBuf::from("/tmp/x")));
            assert!(inv.global.mock && inv.global.quiet);
            assert_eq!(
                inv.command,
                Command::Run(RunArgs { run_for: Some(Duration::from_secs(5)), fleet: None })
            );
        }
        // …and on subcommands that take no flags of their own.
        assert!(p(&["status", "--mock"]).global.mock);
        assert!(p(&["senses", "-q", "enable", "clipboard"]).global.quiet);
        assert_eq!(
            p(&["tier", "--mock", "pin", "T3"]).command,
            Command::Tier(TierCmd::Pin(Tier::Lobotomised))
        );
    }

    #[test]
    fn a_subcommands_own_flags_stay_its_own() {
        // `--for` belongs to `run`, so it is an error anywhere else rather than
        // being quietly swallowed as if it were global.
        assert!(err(&["log", "--for", "5s"]).contains("does not take"));
        assert!(err(&["status", "--all"]).contains("does not take"));
        assert!(err(&["run", "--kind", "said"]).contains("does not take"));
    }

    #[test]
    fn a_positional_value_that_looks_like_a_flag_is_still_a_value() {
        assert_eq!(
            p(&["config", "set", "skin", "--mock"]).command,
            Command::Config(ConfigCmd::Set("skin".into(), "--mock".into()))
        );
        assert!(!p(&["config", "set", "skin", "--mock"]).global.mock);
    }

    #[test]
    fn log_flags_parse_including_the_kind_filter() {
        let Command::Log(a) = p(&["log", "-n", "5", "--kind", "said,dropped", "--wall"]).command
        else {
            panic!()
        };
        assert_eq!(a.n, 5);
        assert!(a.wall);
        assert_eq!(a.kind, KindFilter::Tags(vec!["said".into(), "dropped".into()]));
        let Command::Log(a) = p(&["log", "--all"]).command else { panic!() };
        assert_eq!(a.n, usize::MAX);
    }

    #[test]
    fn durations_parse_the_way_a_person_writes_them() {
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("soon"), None);
    }

    /// DESIGN.md §9: an error says what happened *and what to do next*.
    #[test]
    fn every_parse_error_names_the_remedy_and_does_not_shout() {
        let cases = [
            vec!["frobnicate"],
            vec!["--nonsense"],
            vec!["log", "--nonsense"],
            vec!["log", "-n"],
            vec!["log", "--kind", "shouting"],
            vec!["tier", "pin", "T9"],
            vec!["tier", "sideways"],
            vec!["senses", "enable", "telepathy"],
            vec!["senses", "sniff"],
            vec!["config", "frobnicate"],
            vec!["config", "get"],
            vec!["status", "extra"],
            vec!["--config-dir"],
        ];
        for c in cases {
            let m = err(&c);
            assert!(!m.contains('!'), "{c:?}: {m}");
            assert!(
                m.contains("nx-wisp")
                    || m.contains("Try ")
                    || m.contains("They are ")
                    || m.contains("The senses are")
                    || m.contains("The kinds are")
                    || m.contains("takes ")
                    || m.contains("needs "),
                "{c:?} gives no way forward: {m}"
            );
        }
    }

    #[test]
    fn help_covers_every_command_and_stays_in_voice() {
        let general = help(None);
        for cmd in
            ["run", "status", "log", "explain", "senses", "tier", "config", "doctor", "install"]
        {
            assert!(general.contains(cmd), "help does not mention {cmd}");
            let topic = help(Some(cmd));
            assert!(topic.len() > 80, "`help {cmd}` is a stub");
            assert!(!topic.contains('!'), "`help {cmd}`: {topic}");
        }
        assert!(general.contains("NX_WISP_CONFIG_DIR"), "the override must be documented");
        assert!(help(Some("nope")).contains("no command called"));
    }

    // ---- dispatch ---------------------------------------------------------

    #[test]
    fn status_works_when_she_is_not_running_and_says_so() {
        let tmp = TempConfig::new();
        let text = status_text(tmp.path());
        assert!(text.contains("She is not running"), "{text}");
        assert!(text.contains("What she can see"), "{text}");
        // Every sense has a row whether or not anything is up.
        for id in wisp_senses::ALL_SENSES {
            assert!(text.contains(fmt::sense_label(id)), "{id:?} missing from status");
        }
        assert!(!text.contains('!'), "{text}");
    }

    #[test]
    fn status_reads_a_running_instances_state() {
        let tmp = TempConfig::new();
        let _held = lock::acquire(tmp.path()).unwrap();
        let s = state::State {
            written_ms: crate::epoch_ms(),
            pid: 4242,
            tier: Tier::Lobotomised,
            because: "T3 because a game is running".into(),
            headline: "she is currently costing you nothing".into(),
            ..Default::default()
        };
        state::save(tmp.path(), &s).unwrap();
        let text = status_text(tmp.path());
        assert!(text.contains("She is running."), "{text}");
        assert!(text.contains("4242"), "{text}");
        assert!(text.contains("T3 Lobotomised"), "{text}");
        assert!(text.contains("costing you nothing"), "{text}");
    }

    #[test]
    fn a_stale_state_file_is_not_presented_as_current() {
        let tmp = TempConfig::new();
        let s = state::State {
            written_ms: crate::epoch_ms() - state::STALE_AFTER_MS - 60_000,
            ..Default::default()
        };
        state::save(tmp.path(), &s).unwrap();
        assert!(status_text(tmp.path()).contains("She is not running"));
    }

    #[test]
    fn log_prints_the_trace_and_marks_a_restart() {
        let tmp = TempConfig::new();
        let prefs = config::RecorderPrefs::default();
        {
            let r = crate::Recorder::open(tmp.path(), prefs, 1_700_000_000_000).unwrap();
            r.record_kind(
                0,
                wisp_proto::EventKind::Sensed(wisp_proto::Observation::Focus {
                    app_id: "org.kde.kate".into(),
                    title: "lib.rs".into(),
                }),
            );
        }
        {
            let r = crate::Recorder::open(tmp.path(), prefs, 1_700_000_100_000).unwrap();
            r.record_kind(0, wisp_proto::EventKind::Said { text: "hello there".into() });
        }
        let text = log_text(tmp.path(), &LogArgs::default());
        assert!(text.contains("org.kde.kate"), "{text}");
        assert!(text.contains("hello there"), "{text}");
        assert_eq!(text.matches("-- run started").count(), 2, "a restart must be marked: {text}");

        let only_said =
            log_text(tmp.path(), &LogArgs { kind: KindFilter::parse("said").unwrap(), ..Default::default() });
        assert!(only_said.contains("hello there"));
        assert!(!only_said.contains("org.kde.kate"));
    }

    #[test]
    fn log_and_explain_say_something_useful_on_an_empty_profile() {
        let tmp = TempConfig::new();
        assert!(log_text(tmp.path(), &LogArgs::default()).contains("Nothing has been recorded"));
        let (text, code) = explain_text(tmp.path());
        assert_eq!(code, 0, "an empty trace is not an error");
        assert!(text.contains("nothing to explain"), "{text}");
        assert!(text.contains("nx-wisp log"), "it should point somewhere: {text}");
    }

    #[test]
    fn explain_reads_the_same_file_the_loop_wrote() {
        let tmp = TempConfig::new();
        let r = crate::Recorder::open(tmp.path(), config::RecorderPrefs::default(), 1).unwrap();
        r.record_kind(
            100,
            wisp_proto::EventKind::Sensed(wisp_proto::Observation::Idle {
                idle: true,
                for_ms: 300_000,
            }),
        );
        r.record_kind(
            200,
            wisp_proto::EventKind::Proposed(wisp_proto::Utterance::new(
                "you have been away a while",
                wisp_proto::Urgency::Whim,
            )),
        );
        r.record_kind(900, wisp_proto::EventKind::Said { text: "you have been away a while".into() });
        r.flush();

        let (text, code) = explain_text(tmp.path());
        assert_eq!(code, 0);
        assert!(text.contains("you have been away a while"), "{text}");
        assert!(text.contains("you went idle"), "{text}");
    }

    #[test]
    fn senses_list_shows_every_row_with_its_plain_english_description() {
        let tmp = TempConfig::new();
        let text = senses_list(tmp.path());
        for id in wisp_senses::ALL_SENSES {
            assert!(text.contains(config::sense_key(id)), "{id:?} missing");
        }
        assert!(text.contains("invasive"), "{text}");
        assert!(text.contains("never stored or sent anywhere"), "descriptions missing: {text}");
    }

    #[test]
    fn enabling_a_sense_reaches_the_ledger_and_nothing_else() {
        let tmp = TempConfig::new();
        assert_eq!(
            dispatch(Invocation {
                global: Global::default(),
                command: Command::Senses(SensesCmd::Enable(SenseId::Clipboard)),
            })
            .unwrap(),
            0
        );
        assert!(
            config::sense_rows(tmp.path())
                .iter()
                .find(|r| r.id == SenseId::Clipboard)
                .unwrap()
                .enabled
        );
        assert!(tmp.path().join("senses.json").exists());
        // Nothing about enablement went into config.json.
        let cfg = std::fs::read_to_string(tmp.path().join(config::CONFIG_FILE)).unwrap_or_default();
        assert!(!cfg.contains("clipboard"), "{cfg}");
    }

    #[test]
    fn pinning_a_tier_writes_the_config_the_running_loop_watches() {
        let tmp = TempConfig::new();
        dispatch(Invocation {
            global: Global::default(),
            command: Command::Tier(TierCmd::Pin(Tier::Dormant)),
        })
        .unwrap();
        assert_eq!(config::load_from(tmp.path()).config.tier.pinned, Some(Tier::Dormant));

        dispatch(Invocation {
            global: Global::default(),
            command: Command::Tier(TierCmd::Unpin),
        })
        .unwrap();
        assert_eq!(config::load_from(tmp.path()).config.tier.pinned, None);
    }

    #[test]
    fn config_set_persists_and_config_show_lists_every_key() {
        let tmp = TempConfig::new();
        dispatch(Invocation {
            global: Global::default(),
            command: Command::Config(ConfigCmd::Set("chattiness".into(), "silent".into())),
        })
        .unwrap();
        assert_eq!(config::load_from(tmp.path()).config.chattiness, Chattiness::Silent);

        let text = config_text(tmp.path());
        for k in config::KEYS {
            assert!(text.contains(k), "{k} missing from config show");
        }
        assert!(text.contains("nx-wisp senses"), "it must point at where consent lives: {text}");
    }

    #[test]
    fn config_set_rejects_nonsense_without_touching_the_file() {
        let tmp = TempConfig::new();
        let before = config::load_from(tmp.path()).config;
        let e = dispatch(Invocation {
            global: Global::default(),
            command: Command::Config(ConfigCmd::Set("chattiness".into(), "loud".into())),
        })
        .unwrap_err();
        assert!(e.message.contains("insufferable"), "{}", e.message);
        assert_eq!(config::load_from(tmp.path()).config, before);
    }

    #[test]
    fn config_reset_leaves_consent_and_the_recorder_alone() {
        let tmp = TempConfig::new();
        config::set_sense_enabled(tmp.path(), SenseId::Clipboard, true).unwrap();
        let r = crate::Recorder::open(tmp.path(), config::RecorderPrefs::default(), 1).unwrap();
        r.record_kind(0, wisp_proto::EventKind::Said { text: "kept".into() });
        r.flush();
        drop(r);

        dispatch(Invocation {
            global: Global::default(),
            command: Command::Config(ConfigCmd::Reset),
        })
        .unwrap();

        assert_eq!(config::load_from(tmp.path()).config, Config::default());
        assert!(config::sense_rows(tmp.path())
            .iter()
            .find(|r| r.id == SenseId::Clipboard)
            .unwrap()
            .enabled);
        assert!(log_text(tmp.path(), &LogArgs::default()).contains("kept"));
    }

    #[test]
    fn the_config_dir_flag_redirects_everything_downstream() {
        let tmp = TempConfig::new();
        let other = tmp.path().join("second-profile");
        dispatch(Invocation {
            global: Global { config_dir: Some(other.clone()), ..Default::default() },
            command: Command::Config(ConfigCmd::Set("size".into(), "64".into())),
        })
        .unwrap();
        assert_eq!(config::load_from(&other).config.appearance.size_px, 64.0);
        assert_eq!(
            config::load_from(tmp.path()).config.appearance.size_px,
            128.0,
            "the first profile must be untouched"
        );
    }

    #[test]
    fn doctor_exits_nonzero_only_when_something_would_stop_her() {
        let tmp = TempConfig::new();
        let checks = doctor::run(&doctor::Env::offline(tmp.path(), &tmp.install_root()));
        let code = if doctor::worst(&checks) == doctor::Level::Fail { 1 } else { 0 };
        // On a headless CI box the Wayland check fails, which is correct.
        assert!(code == 0 || code == 1);
        assert!(!doctor::render(&checks).is_empty());
    }

    #[test]
    fn install_and_uninstall_round_trip_through_dispatch() {
        let tmp = TempConfig::new();
        let root = tmp.install_root();
        // `--dry-run` keeps systemctl out of it, which is what a test wants.
        dispatch(Invocation {
            global: Global::default(),
            command: Command::Install(InstallArgs { dry_run: true, ..Default::default() }),
        })
        .unwrap();
        assert!(!install::unit_path(&root).exists(), "a dry run wrote a file");

        dispatch(Invocation {
            global: Global::default(),
            command: Command::Uninstall(InstallArgs { dry_run: true, ..Default::default() }),
        })
        .unwrap();
    }
}
