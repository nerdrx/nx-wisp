//! **F57 — the systemd user unit and the autostart entry.**
//!
//! The unit itself is not written here. `wisp-gov` generates it
//! ([`wisp_gov::ceiling::unit_file`]) because the ceilings *are* the governor's
//! business: `CPUQuota=` and `MemoryMax=` become cgroup v2 `cpu.max` and
//! `memory.max`, which is a wall the kernel enforces on her whether or not the
//! tier ladder is working. F65 calls it "enforced from outside the process",
//! and that is exactly the point — everything else in the app is her policing
//! herself.
//!
//! What is here is where the file goes, how it is removed again, and the choice
//! between the two ways a user session can start something.
//!
//! # One or the other, never both
//!
//! systemd and XDG autostart will each happily start her, and installing both
//! means two copies racing for the single-instance lock. One would win, the
//! other would exit, and which is which would depend on session start-up order.
//! [`apply`] refuses rather than leaving that to chance, and [`Status`] reports
//! it if it somehow happens anyway.
//!
//! The default is the systemd unit, because it is the one that carries the
//! ceilings. The `.desktop` entry exists for a session that is not running
//! systemd --user, and it gets a note saying it has no ceilings.
//!
//! # The install root
//!
//! systemd reads user units from `$XDG_CONFIG_HOME/systemd/user`, not from
//! `NX_WISP_CONFIG_DIR` — an isolated config dir is a different *profile*, not
//! a different systemd. So the install root is resolved separately, and
//! `NX_WISP_INSTALL_ROOT` overrides it, which is how the tests write unit files
//! into a temp dir instead of the operator's session.
//!
//! An install from an isolated config dir carries `NX_WISP_CONFIG_DIR` into the
//! unit's `Environment=`, so the installed copy keeps the profile it was
//! installed from rather than silently reverting to the default one.

use std::path::{Path, PathBuf};

use wisp_gov::ceiling::{self, UnitSpec};

pub const UNIT_NAME: &str = "nx-wisp";
pub const DESKTOP_FILE: &str = "nx-wisp.desktop";

/// Which mechanism starts her at login.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// A systemd user unit. Carries the F65 ceilings.
    #[default]
    Systemd,
    /// An XDG autostart entry. No ceilings; for a session without
    /// `systemd --user`.
    Autostart,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Systemd => "systemd",
            Mode::Autostart => "autostart",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "systemd" | "unit" | "service" => Some(Mode::Systemd),
            "autostart" | "desktop" | "xdg" => Some(Mode::Autostart),
            _ => None,
        }
    }
}

/// `$NX_WISP_INSTALL_ROOT`, else `$XDG_CONFIG_HOME`, else `~/.config`.
pub fn install_root() -> PathBuf {
    if let Some(d) = std::env::var_os("NX_WISP_INSTALL_ROOT") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    if let Some(d) = std::env::var_os("XDG_CONFIG_HOME") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    crate::config::home().join(".config")
}

pub fn unit_path(root: &Path) -> PathBuf {
    root.join("systemd").join("user").join(format!("{UNIT_NAME}.service"))
}

pub fn desktop_path(root: &Path) -> PathBuf {
    root.join("autostart").join(DESKTOP_FILE)
}

// ---------------------------------------------------------------------------
// The spec
// ---------------------------------------------------------------------------

/// Ceilings scaled to *this* machine, not to the machine she was written on.
///
/// `UnitSpec::for_machine` takes a quarter of the cores (clamped to 1–4) and a
/// sixteenth of RAM (clamped to 1–8 GiB), so a laptop gets a laptop's numbers
/// from the same call the desktop uses.
pub fn spec_for_this_machine(exec: &Path) -> UnitSpec {
    let cores = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4);
    let ram_mib = mem_total_mib().unwrap_or(8 * 1024);
    let mut spec = UnitSpec::for_machine(cores, ram_mib);
    spec.name = UNIT_NAME.to_string();
    spec.exec_start = exec.display().to_string();
    spec.args = vec!["run".to_string()];
    // An isolated profile stays isolated once installed.
    if let Some(dir) = std::env::var_os("NX_WISP_CONFIG_DIR") {
        if !dir.is_empty() {
            spec.environment.push((
                "NX_WISP_CONFIG_DIR".to_string(),
                dir.to_string_lossy().to_string(),
            ));
        }
    }
    spec
}

fn mem_total_mib() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = s.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib / 1024)
}

/// The XDG autostart entry.
///
/// `OnlyShowIn=KDE` is not caution, it is SPEC §1: KDE Plasma 6 is the hard
/// target and there is no other session she runs in. `X-KDE-autostart-after`
/// waits for the panel, so the layer surface has outputs to attach to.
pub fn desktop_entry(exec: &Path) -> String {
    let mut exec_line = exec.display().to_string();
    if exec_line.contains(' ') {
        exec_line = format!("\"{exec_line}\"");
    }
    let mut s = String::new();
    s.push_str("[Desktop Entry]\n");
    s.push_str("Type=Application\n");
    s.push_str("Name=NX Wisp\n");
    s.push_str("Comment=Desktop companion\n");
    s.push_str(&format!("Exec={exec_line} run\n"));
    s.push_str("Icon=nx-wisp\n");
    s.push_str("Terminal=false\n");
    s.push_str("Categories=Utility;\n");
    s.push_str("OnlyShowIn=KDE;\n");
    s.push_str("X-KDE-autostart-after=panel\n");
    if let Some(dir) = std::env::var_os("NX_WISP_CONFIG_DIR") {
        if !dir.is_empty() {
            // `Exec` has no environment of its own, so the profile travels as an
            // argument instead.
            s = s.replace(
                &format!("Exec={exec_line} run\n"),
                &format!(
                    "Exec={exec_line} --config-dir \"{}\" run\n",
                    dir.to_string_lossy()
                ),
            );
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Plan / apply
// ---------------------------------------------------------------------------

/// What an install would write, before it writes it. `--dry-run` prints this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub mode: Mode,
    pub path: PathBuf,
    pub body: String,
    /// The other mechanism's file, which must not exist alongside ours.
    pub conflicts_with: PathBuf,
}

pub fn plan(mode: Mode, root: &Path, exec: &Path) -> Plan {
    match mode {
        Mode::Systemd => Plan {
            mode,
            path: unit_path(root),
            body: ceiling::unit_file(&spec_for_this_machine(exec)),
            conflicts_with: desktop_path(root),
        },
        Mode::Autostart => Plan {
            mode,
            path: desktop_path(root),
            body: desktop_entry(exec),
            conflicts_with: unit_path(root),
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplyOptions {
    /// Say what would happen and change nothing.
    pub dry_run: bool,
    /// Run `systemctl --user daemon-reload` and `enable`. Off in tests, and off
    /// when there is no session bus to talk to.
    pub run_systemctl: bool,
    /// Overwrite the other mechanism's entry instead of refusing.
    pub force: bool,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        ApplyOptions { dry_run: false, run_systemctl: true, force: false }
    }
}

/// What actually happened, as lines for the CLI to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Wrote(PathBuf),
    Removed(PathBuf),
    Ran(String),
    Skipped(String),
    Note(String),
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Action::Wrote(p) => write!(f, "wrote {}", p.display()),
            Action::Removed(p) => write!(f, "removed {}", p.display()),
            Action::Ran(c) => write!(f, "ran {c}"),
            Action::Skipped(s) => write!(f, "skipped {s}"),
            Action::Note(s) => write!(f, "{s}"),
        }
    }
}

#[derive(Debug)]
pub enum InstallError {
    /// The other mechanism is already installed and would race with this one.
    Conflict { existing: PathBuf, wanted: Mode },
    Io { path: PathBuf, source: std::io::Error },
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallError::Conflict { existing, wanted } => write!(
                f,
                "{} already starts her at login, and installing the {} entry as well would \
                 start two copies. Run `nx-wisp uninstall` first, or pass --force to replace it.",
                existing.display(),
                wanted.as_str()
            ),
            InstallError::Io { path, source } => {
                write!(f, "Could not write {} — {source}", path.display())
            }
        }
    }
}

impl std::error::Error for InstallError {}

pub fn apply(plan: &Plan, opts: ApplyOptions) -> Result<Vec<Action>, InstallError> {
    let mut actions = Vec::new();

    if plan.conflicts_with.exists() {
        if !opts.force {
            return Err(InstallError::Conflict {
                existing: plan.conflicts_with.clone(),
                wanted: plan.mode,
            });
        }
        if opts.dry_run {
            actions.push(Action::Note(format!(
                "would remove {}",
                plan.conflicts_with.display()
            )));
        } else {
            let _ = std::fs::remove_file(&plan.conflicts_with);
            actions.push(Action::Removed(plan.conflicts_with.clone()));
        }
    }

    if opts.dry_run {
        actions.push(Action::Note(format!("would write {}", plan.path.display())));
        if plan.mode == Mode::Systemd {
            actions.push(Action::Note(
                "would run systemctl --user daemon-reload, then enable nx-wisp.service"
                    .to_string(),
            ));
        }
        return Ok(actions);
    }

    let io = |p: &Path, e| InstallError::Io { path: p.to_path_buf(), source: e };
    if let Some(parent) = plan.path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
    }
    write_atomic(&plan.path, plan.body.as_bytes()).map_err(|e| io(&plan.path, e))?;
    actions.push(Action::Wrote(plan.path.clone()));

    if plan.mode == Mode::Systemd {
        if opts.run_systemctl {
            for args in [
                vec!["--user", "daemon-reload"],
                vec!["--user", "enable", &format!("{UNIT_NAME}.service")],
            ] {
                match std::process::Command::new("systemctl").args(&args).status() {
                    Ok(s) if s.success() => {
                        actions.push(Action::Ran(format!("systemctl {}", args.join(" "))))
                    }
                    Ok(s) => actions.push(Action::Skipped(format!(
                        "systemctl {} exited {}",
                        args.join(" "),
                        s.code().unwrap_or(-1)
                    ))),
                    Err(e) => actions.push(Action::Skipped(format!(
                        "systemctl {} — {e}",
                        args.join(" ")
                    ))),
                }
            }
        } else {
            actions.push(Action::Skipped("systemctl".to_string()));
        }
        actions.push(Action::Note(
            "She starts at your next login. `systemctl --user start nx-wisp` starts her now."
                .to_string(),
        ));
    } else {
        actions.push(Action::Note(
            "She starts at your next login. This entry carries no CPU or memory ceilings; \
             the systemd unit does."
                .to_string(),
        ));
    }

    Ok(actions)
}

/// Remove both mechanisms, whichever is present.
pub fn uninstall(root: &Path, opts: ApplyOptions) -> Vec<Action> {
    let mut actions = Vec::new();
    let unit = unit_path(root);
    let desktop = desktop_path(root);

    if unit.exists() {
        if opts.dry_run {
            actions.push(Action::Note(format!("would remove {}", unit.display())));
        } else {
            if opts.run_systemctl {
                for args in [
                    vec!["--user", "disable", "--now", &format!("{UNIT_NAME}.service")],
                ] {
                    match std::process::Command::new("systemctl").args(&args).status() {
                        Ok(_) => actions.push(Action::Ran(format!("systemctl {}", args.join(" ")))),
                        Err(e) => actions
                            .push(Action::Skipped(format!("systemctl {} — {e}", args.join(" ")))),
                    }
                }
            }
            match std::fs::remove_file(&unit) {
                Ok(()) => actions.push(Action::Removed(unit)),
                Err(e) => actions.push(Action::Skipped(format!("{}: {e}", unit.display()))),
            }
            if opts.run_systemctl {
                let _ = std::process::Command::new("systemctl")
                    .args(["--user", "daemon-reload"])
                    .status();
            }
        }
    }

    if desktop.exists() {
        if opts.dry_run {
            actions.push(Action::Note(format!("would remove {}", desktop.display())));
        } else {
            match std::fs::remove_file(&desktop) {
                Ok(()) => actions.push(Action::Removed(desktop)),
                Err(e) => actions.push(Action::Skipped(format!("{}: {e}", desktop.display()))),
            }
        }
    }

    if actions.is_empty() {
        actions.push(Action::Note("Nothing was installed.".to_string()));
    } else if !opts.dry_run {
        actions.push(Action::Note(
            "Your config, consent choices and flight recorder are untouched.".to_string(),
        ));
    }
    actions
}

fn write_atomic(path: &Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("out"),
        std::process::id()
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub root: PathBuf,
    pub unit: Option<PathBuf>,
    pub desktop: Option<PathBuf>,
    /// The `CPUQuota=` in the installed unit, if any.
    pub cpu_quota_pct: Option<u32>,
    pub memory_max: Option<String>,
    /// The binary the installed entry points at.
    pub exec: Option<String>,
}

impl Status {
    pub fn installed(&self) -> bool {
        self.unit.is_some() || self.desktop.is_some()
    }

    /// Both mechanisms present: two copies would race at login.
    pub fn conflicted(&self) -> bool {
        self.unit.is_some() && self.desktop.is_some()
    }

    pub fn describe(&self) -> String {
        match (&self.unit, &self.desktop) {
            (Some(_), Some(_)) => {
                "both a systemd unit and an autostart entry are installed".to_string()
            }
            (Some(_), None) => match self.cpu_quota_pct {
                Some(q) => format!("systemd user unit, capped at {q}% of one core"),
                None => "systemd user unit, with no CPU cap".to_string(),
            },
            (None, Some(_)) => "autostart entry, with no ceilings".to_string(),
            (None, None) => "not installed".to_string(),
        }
    }
}

pub fn status(root: &Path) -> Status {
    let unit = unit_path(root);
    let desktop = desktop_path(root);
    let mut s = Status {
        root: root.to_path_buf(),
        unit: unit.exists().then(|| unit.clone()),
        desktop: desktop.exists().then(|| desktop.clone()),
        cpu_quota_pct: None,
        memory_max: None,
        exec: None,
    };
    if let Ok(text) = std::fs::read_to_string(&unit) {
        let parsed = ceiling::parse_unit(&text);
        s.cpu_quota_pct = ceiling::unit_value(&parsed, "Service", "CPUQuota")
            .and_then(|v| v.trim_end_matches('%').parse().ok());
        s.memory_max =
            ceiling::unit_value(&parsed, "Service", "MemoryMax").map(str::to_string);
        s.exec = ceiling::unit_value(&parsed, "Service", "ExecStart").map(str::to_string);
    } else if let Ok(text) = std::fs::read_to_string(&desktop) {
        s.exec = text
            .lines()
            .find_map(|l| l.strip_prefix("Exec="))
            .map(str::to_string);
    }
    s
}

/// Our own binary, for the `ExecStart=`. Falls back to the install name, which
/// systemd resolves on `PATH`.
pub fn current_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from(crate::APP_ID))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempConfig;

    fn no_systemctl() -> ApplyOptions {
        ApplyOptions { dry_run: false, run_systemctl: false, force: false }
    }

    #[test]
    fn the_install_root_honours_its_override() {
        let tmp = TempConfig::new();
        assert_eq!(install_root(), tmp.install_root());
        let home = crate::config::home();
        assert!(!install_root().starts_with(home.join(".config")));
    }

    #[test]
    fn the_unit_carries_the_ceilings_that_are_the_point_of_it() {
        let tmp = TempConfig::new();
        let exec = PathBuf::from("/usr/bin/nx-wisp");
        let p = plan(Mode::Systemd, &tmp.install_root(), &exec);
        apply(&p, no_systemctl()).unwrap();

        let text = std::fs::read_to_string(unit_path(&tmp.install_root())).unwrap();
        let parsed = ceiling::parse_unit(&text);
        assert!(ceiling::unit_value(&parsed, "Service", "CPUQuota").is_some(), "{text}");
        assert!(ceiling::unit_value(&parsed, "Service", "MemoryMax").is_some(), "{text}");
        assert_eq!(
            ceiling::unit_value(&parsed, "Service", "ExecStart"),
            Some("/usr/bin/nx-wisp run")
        );
        assert_eq!(
            ceiling::unit_value(&parsed, "Install", "WantedBy"),
            Some("graphical-session.target")
        );
        // The whole file is generated by wisp-gov; this asserts we did not
        // hand-roll a second one.
        assert_eq!(text, ceiling::unit_file(&spec_for_this_machine(&exec)));
    }

    #[test]
    fn an_isolated_profile_survives_being_installed() {
        let tmp = TempConfig::new();
        let p = plan(Mode::Systemd, &tmp.install_root(), Path::new("/usr/bin/nx-wisp"));
        assert!(
            p.body.contains("Environment=NX_WISP_CONFIG_DIR="),
            "an install from an isolated profile must keep it: {}",
            p.body
        );
        let d = plan(Mode::Autostart, &tmp.install_root(), Path::new("/usr/bin/nx-wisp"));
        assert!(d.body.contains("--config-dir"), "{}", d.body);
    }

    #[test]
    fn the_autostart_entry_is_kde_only_per_spec_1() {
        let tmp = TempConfig::new();
        let p = plan(Mode::Autostart, &tmp.install_root(), Path::new("/usr/bin/nx-wisp"));
        apply(&p, no_systemctl()).unwrap();
        let text = std::fs::read_to_string(desktop_path(&tmp.install_root())).unwrap();
        assert!(text.starts_with("[Desktop Entry]\n"), "{text}");
        assert!(text.contains("OnlyShowIn=KDE;"), "{text}");
        assert!(text.contains("X-KDE-autostart-after=panel"), "{text}");
        assert!(!text.contains("GNOME"), "SPEC §1 forbids a GNOME branch: {text}");
        assert!(!text.contains("Type=Application\nType="), "{text}");
    }

    #[test]
    fn installing_both_would_start_two_copies_so_it_is_refused() {
        let tmp = TempConfig::new();
        let root = tmp.install_root();
        let exec = Path::new("/usr/bin/nx-wisp");
        apply(&plan(Mode::Systemd, &root, exec), no_systemctl()).unwrap();

        let err = apply(&plan(Mode::Autostart, &root, exec), no_systemctl()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("two copies"), "{msg}");
        assert!(msg.contains("uninstall"), "the message must say what to do: {msg}");
        assert!(!desktop_path(&root).exists(), "nothing was written");

        // …and --force replaces rather than duplicates.
        let forced = ApplyOptions { force: true, ..no_systemctl() };
        apply(&plan(Mode::Autostart, &root, exec), forced).unwrap();
        assert!(desktop_path(&root).exists());
        assert!(!unit_path(&root).exists());
        assert!(!status(&root).conflicted());
    }

    #[test]
    fn install_is_idempotent() {
        let tmp = TempConfig::new();
        let root = tmp.install_root();
        let p = plan(Mode::Systemd, &root, Path::new("/usr/bin/nx-wisp"));
        apply(&p, no_systemctl()).unwrap();
        let first = std::fs::read_to_string(unit_path(&root)).unwrap();
        apply(&p, no_systemctl()).unwrap();
        assert_eq!(std::fs::read_to_string(unit_path(&root)).unwrap(), first);
    }

    #[test]
    fn uninstall_removes_both_and_leaves_the_operators_state_alone() {
        let tmp = TempConfig::new();
        let root = tmp.install_root();
        let exec = Path::new("/usr/bin/nx-wisp");
        apply(&plan(Mode::Systemd, &root, exec), no_systemctl()).unwrap();
        let forced = ApplyOptions { force: true, ..no_systemctl() };
        apply(&plan(Mode::Autostart, &root, exec), forced).unwrap();

        // Something that must survive.
        crate::config::save_to(tmp.path(), &crate::Config::default()).unwrap();

        let actions = uninstall(&root, no_systemctl());
        assert!(!unit_path(&root).exists());
        assert!(!desktop_path(&root).exists());
        assert!(actions.iter().any(|a| matches!(a, Action::Removed(_))));
        assert!(tmp.path().join(crate::config::CONFIG_FILE).exists(), "config was deleted");

        // And a second uninstall says so rather than erroring.
        let again = uninstall(&root, no_systemctl());
        assert_eq!(again.len(), 1);
        assert!(matches!(&again[0], Action::Note(n) if n.contains("Nothing was installed")));
    }

    #[test]
    fn a_dry_run_changes_nothing() {
        let tmp = TempConfig::new();
        let root = tmp.install_root();
        let p = plan(Mode::Systemd, &root, Path::new("/usr/bin/nx-wisp"));
        let actions = apply(&p, ApplyOptions { dry_run: true, ..no_systemctl() }).unwrap();
        assert!(!unit_path(&root).exists());
        assert!(actions.iter().any(|a| a.to_string().contains("would write")));
        assert!(actions.iter().any(|a| a.to_string().contains("daemon-reload")));
    }

    #[test]
    fn status_reads_back_what_was_installed() {
        let tmp = TempConfig::new();
        let root = tmp.install_root();
        assert!(!status(&root).installed());
        assert_eq!(status(&root).describe(), "not installed");

        apply(&plan(Mode::Systemd, &root, Path::new("/usr/bin/nx-wisp")), no_systemctl())
            .unwrap();
        let s = status(&root);
        assert!(s.installed() && !s.conflicted());
        assert!(s.cpu_quota_pct.is_some());
        assert!(s.memory_max.is_some());
        assert_eq!(s.exec.as_deref(), Some("/usr/bin/nx-wisp run"));
        assert!(s.describe().contains("of one core"), "{}", s.describe());
    }

    #[test]
    fn the_ceilings_are_scaled_to_the_machine_not_baked_in() {
        let _tmp = TempConfig::new();
        let spec = spec_for_this_machine(Path::new("/usr/bin/nx-wisp"));
        let q = spec.cpu_quota_pct.expect("a unit with no CPU cap is not a ceiling");
        assert!((100..=400).contains(&q), "{q}");
        let m = spec.memory_max_mib.unwrap();
        assert!((1024..=8192).contains(&m), "{m}");
        assert!(spec.memory_swap_max_zero, "swapping a companion out is worse than shedding");
    }

    #[test]
    fn modes_parse_from_what_a_person_would_type() {
        assert_eq!(Mode::parse("systemd"), Some(Mode::Systemd));
        assert_eq!(Mode::parse("Autostart"), Some(Mode::Autostart));
        assert_eq!(Mode::parse("launchd"), None);
        assert_eq!(Mode::default(), Mode::Systemd, "the ceilings are the default");
    }
}
