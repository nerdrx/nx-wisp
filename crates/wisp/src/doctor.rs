//! `nx-wisp doctor` — is this machine one she can run on?
//!
//! SPEC §1 is unusually strict: Linux, Wayland, KDE Plasma 6 / KWin ≥ 6.0,
//! Vulkan, `zwlr_layer_shell_v1` and KWin's D-Bus scripting interface, with **no
//! X11, no GNOME and no fallbacks anywhere in the tree**. That is a deliberate,
//! permanent choice, and its cost is that a machine which does not meet it fails
//! in a way that looks like a bug. This command is the answer to that: it says
//! which requirement is missing and what to do about it, before she has drawn a
//! frame.
//!
//! Every check is read-only. In particular the compositor check enumerates the
//! Wayland registry and disconnects — it never creates a surface, and creating
//! one here would be `wisp-shell`'s job in any case.

use std::path::{Path, PathBuf};

use crate::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Fine.
    Ok,
    /// Context, not a verdict.
    Info,
    /// She will run, but something is degraded.
    Warn,
    /// She will not run, or not properly.
    Fail,
}

impl Level {
    pub fn mark(self) -> &'static str {
        match self {
            Level::Ok => "ok  ",
            Level::Info => "    ",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub level: Level,
    /// What is true, right now.
    pub detail: String,
    /// What to do about it. DESIGN.md §9: errors say what happened *and* what
    /// to do next.
    pub fix: Option<String>,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Check {
        Check { name, level: Level::Ok, detail: detail.into(), fix: None }
    }
    fn info(name: &'static str, detail: impl Into<String>) -> Check {
        Check { name, level: Level::Info, detail: detail.into(), fix: None }
    }
    fn warn(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
        Check { name, level: Level::Warn, detail: detail.into(), fix: Some(fix.into()) }
    }
    fn fail(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
        Check { name, level: Level::Fail, detail: detail.into(), fix: Some(fix.into()) }
    }
}

/// Where to look. Injected so the whole report is testable against a fixture
/// rather than against whatever session happens to be running.
#[derive(Debug, Clone)]
pub struct Env {
    pub config_dir: PathBuf,
    pub install_root: PathBuf,
    /// Where Vulkan ICD manifests live.
    pub vulkan_icd_dirs: Vec<PathBuf>,
    /// Where the KWin terrain script is written at runtime.
    pub script_dir: PathBuf,
    /// Ask the compositor for its globals. Off in tests: a CI machine has no
    /// compositor, and the operator's session must not be poked by a unit test.
    pub probe_compositor: bool,
    /// Read this machine's GPUs through `wisp-gov`'s probes.
    pub probe_gpus: bool,
}

impl Env {
    pub fn current() -> Env {
        Env {
            config_dir: crate::config::config_dir(),
            install_root: crate::install::install_root(),
            vulkan_icd_dirs: vec![
                PathBuf::from("/usr/share/vulkan/icd.d"),
                PathBuf::from("/etc/vulkan/icd.d"),
                crate::config::home().join(".local/share/vulkan/icd.d"),
            ],
            script_dir: wisp_senses::kwin::script::script_dir(),
            probe_compositor: true,
            probe_gpus: true,
        }
    }

    /// Everything on, nothing probed. What the tests use.
    pub fn offline(config_dir: &Path, install_root: &Path) -> Env {
        Env {
            config_dir: config_dir.to_path_buf(),
            install_root: install_root.to_path_buf(),
            vulkan_icd_dirs: vec![config_dir.join("vulkan/icd.d")],
            script_dir: config_dir.join("run"),
            probe_compositor: false,
            probe_gpus: false,
        }
    }
}

/// Run every check, in the order the report prints them.
pub fn run(env: &Env) -> Vec<Check> {
    vec![
        session_type(),
        desktop(),
        compositor(env),
        vulkan(env),
        gpus(env),
        ceilings(),
        config_dir_check(env),
        recorder_check(env),
        consent(env),
        kwin_script(env),
        install_check(env),
        running(env),
    ]
}

pub fn worst(checks: &[Check]) -> Level {
    checks.iter().map(|c| c.level).max().unwrap_or(Level::Ok)
}

/// Plain text, in DESIGN.md §9's voice.
pub fn render(checks: &[Check]) -> String {
    let width = checks.iter().map(|c| c.name.len()).max().unwrap_or(10);
    let mut s = String::new();
    for c in checks {
        s.push_str(&format!("{}  {:<width$}  {}\n", c.level.mark(), c.name, c.detail));
        if let Some(fix) = &c.fix {
            s.push_str(&format!("      {:<width$}  {fix}\n", ""));
        }
    }
    s.push('\n');
    s.push_str(&match worst(checks) {
        Level::Ok | Level::Info => "Everything she needs is here.".to_string(),
        Level::Warn => {
            let n = checks.iter().filter(|c| c.level == Level::Warn).count();
            format!("She will run. {n} thing{} above could be better.", plural(n))
        }
        Level::Fail => {
            let n = checks.iter().filter(|c| c.level == Level::Fail).count();
            format!("{n} thing{} above will stop her from working properly.", plural(n))
        }
    });
    s.push('\n');
    s
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

// ---------------------------------------------------------------------------
// The checks
// ---------------------------------------------------------------------------

fn session_type() -> Check {
    let kind = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
    if !display.is_empty() {
        return Check::ok("wayland", format!("WAYLAND_DISPLAY={display}"));
    }
    if kind == "x11" || std::env::var_os("DISPLAY").is_some() {
        return Check::fail(
            "wayland",
            "this is an X11 session",
            "Log out and pick Plasma (Wayland) at the login screen. She has no X11 path \
             and will not grow one.",
        );
    }
    Check::fail(
        "wayland",
        "no WAYLAND_DISPLAY in the environment",
        "Run her from inside a Plasma Wayland session.",
    )
}

fn desktop() -> Check {
    let current = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let version = std::env::var("KDE_SESSION_VERSION").unwrap_or_default();
    if !current.to_ascii_uppercase().contains("KDE") {
        let seen = if current.is_empty() { "nothing".to_string() } else { current };
        return Check::fail(
            "plasma",
            format!("XDG_CURRENT_DESKTOP is {seen}"),
            "She needs KDE Plasma 6. The window terrain comes from a KWin script and \
             there is no other source for it.",
        );
    }
    match version.parse::<u32>() {
        Ok(v) if v >= 6 => Check::ok("plasma", format!("KDE Plasma {v}")),
        Ok(v) => Check::fail(
            "plasma",
            format!("KDE Plasma {v}"),
            "She needs Plasma 6 or newer; KWin 5's scripting interface is different.",
        ),
        Err(_) => Check::warn(
            "plasma",
            "KDE, but KDE_SESSION_VERSION is not set",
            "Probably fine. If the window terrain never arrives, check that this is \
             Plasma 6.",
        ),
    }
}

/// The one hard Wayland dependency: `zwlr_layer_shell_v1`.
fn compositor(env: &Env) -> Check {
    if !env.probe_compositor {
        return Check::info("layer-shell", "not checked");
    }
    match wayland_globals() {
        Err(e) => Check::warn(
            "layer-shell",
            format!("could not reach the compositor: {e}"),
            "She needs a Wayland session to check this. Run `nx-wisp doctor` from inside \
             one.",
        ),
        Ok(globals) => {
            let has = |name: &str| globals.iter().any(|(n, _)| n == name);
            if has("zwlr_layer_shell_v1") {
                let extras = ["ext_idle_notifier_v1", "ext_data_control_manager_v1"];
                let missing: Vec<&str> = extras.iter().copied().filter(|n| !has(n)).collect();
                if missing.is_empty() {
                    Check::ok(
                        "layer-shell",
                        format!("zwlr_layer_shell_v1, and {} globals in all", globals.len()),
                    )
                } else {
                    Check::warn(
                        "layer-shell",
                        format!("zwlr_layer_shell_v1 is there; {} is not", missing.join(", ")),
                        "The senses that need those will stay quiet. Everything else works.",
                    )
                }
            } else {
                Check::fail(
                    "layer-shell",
                    "this compositor does not advertise zwlr_layer_shell_v1",
                    "It is a hard dependency and a permanent choice (SPEC §1). On KWin it \
                     is built in, so this usually means the session is not KWin.",
                )
            }
        }
    }
}

fn vulkan(env: &Env) -> Check {
    let mut icds = Vec::new();
    for dir in &env.vulkan_icd_dirs {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if e.path().extension().is_some_and(|x| x == "json") {
                    icds.push(e.file_name().to_string_lossy().to_string());
                }
            }
        }
    }
    let loader = ["/usr/lib/libvulkan.so.1", "/usr/lib64/libvulkan.so.1", "/lib/libvulkan.so.1"]
        .iter()
        .any(|p| Path::new(p).exists());

    match (loader, icds.is_empty()) {
        (true, false) => Check::ok("vulkan", format!("loader present, drivers: {}", icds.join(", "))),
        (true, true) => Check::warn(
            "vulkan",
            "the loader is installed but no driver manifest was found",
            "Install the Vulkan driver for your card (vulkan-radeon, vulkan-intel or \
             nvidia-utils).",
        ),
        (false, false) => Check::warn(
            "vulkan",
            format!("driver manifests exist ({}) but no loader was found", icds.len()),
            "Install vulkan-icd-loader.",
        ),
        (false, true) => Check::fail(
            "vulkan",
            "no Vulkan loader and no driver manifests",
            "Install vulkan-icd-loader and the driver for your card. She renders through \
             Vulkan and has no software path.",
        ),
    }
}

fn gpus(env: &Env) -> Check {
    if !env.probe_gpus {
        return Check::info("gpu", "not checked");
    }
    use wisp_gov::probe::SnapshotSource;
    let mut probes = wisp_gov::probe::Probes::real(&wisp_gov::GovConfig::default());
    let snap = probes.snapshot();
    if snap.gpus.is_empty() {
        return Check::fail(
            "gpu",
            "no GPU found under /sys/class/drm",
            "She needs a DRM device. In a container, pass /dev/dri through.",
        );
    }
    let names: Vec<String> = snap
        .gpus
        .iter()
        .map(|g| {
            format!(
                "{} {} ({} MiB)",
                g.id.driver,
                match g.id.kind {
                    wisp_gov::GpuKind::Discrete => "discrete",
                    wisp_gov::GpuKind::Integrated => "integrated",
                    _ => "other",
                },
                g.vram_total_mib
            )
        })
        .collect();
    if snap.gpus.len() > 1 {
        Check::ok("gpu", format!("{} — she can hide on the second one at T3", names.join(", ")))
    } else {
        Check::info(
            "gpu",
            format!("{} — one card, so T3 means the sprite atlas rather than an offload", names.join(", ")),
        )
    }
}

fn ceilings() -> Check {
    let limits = wisp_gov::ceiling::effective_limits();
    match (limits.cpu_quota_pct, limits.memory_max_mib) {
        (None, None) => Check::info(
            "ceilings",
            "no cgroup limits are in force on this process",
            ),
        (cpu, mem) => Check::ok(
            "ceilings",
            format!(
                "cgroup: cpu {}, memory {}",
                cpu.map(|c| format!("{c}%")).unwrap_or_else(|| "unlimited".into()),
                mem.map(|m| format!("{m} MiB")).unwrap_or_else(|| "unlimited".into())
            ),
        ),
    }
}

fn config_dir_check(env: &Env) -> Check {
    let dir = &env.config_dir;
    let overridden = std::env::var_os("NX_WISP_CONFIG_DIR").is_some();
    if let Err(e) = std::fs::create_dir_all(dir) {
        return Check::fail(
            "config",
            format!("cannot create {} — {e}", dir.display()),
            "Check the permissions on the parent directory.",
        );
    }
    let probe = dir.join(".doctor-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            let note = if overridden { " (NX_WISP_CONFIG_DIR)" } else { "" };
            Check::ok("config", format!("{}{note}", dir.display()))
        }
        Err(e) => Check::fail(
            "config",
            format!("{} is not writable — {e}", dir.display()),
            "She keeps her consent choices and her flight recorder here and cannot start \
             without it.",
        ),
    }
}

fn recorder_check(env: &Env) -> Check {
    let prefs = crate::config::load_from(&env.config_dir).config.recorder;
    let bytes = crate::recorder::disk_bytes(&env.config_dir, prefs.keep);
    let files = crate::recorder::generations(&env.config_dir, prefs.keep).len();
    let cap = prefs.max_bytes * (prefs.keep as u64 + 1);
    if files == 0 {
        return Check::info("recorder", "no trace yet; she has not run in this profile");
    }
    Check::ok(
        "recorder",
        format!(
            "{} across {files} file{}, capped at {}",
            fmt::bytes(bytes),
            plural(files),
            fmt::bytes(cap)
        ),
    )
}

fn consent(env: &Env) -> Check {
    let rows = crate::config::sense_rows(&env.config_dir);
    let on: Vec<&str> = rows.iter().filter(|r| r.enabled).map(|r| r.label).collect();
    let invasive: Vec<&str> = rows
        .iter()
        .filter(|r| r.enabled && r.consent == wisp_proto::Consent::Invasive)
        .map(|r| r.label)
        .collect();
    let detail = format!("{} of {} senses on: {}", on.len(), rows.len(), on.join(", "));
    if invasive.is_empty() {
        Check::ok("consent", detail)
    } else {
        // Not a problem — the operator asked for it — but it is the one thing
        // worth saying out loud (SPEC §0.3).
        Check::info(
            "consent",
            format!("{detail}. Invasive and live-tell: {}", invasive.join(", ")),
        )
    }
}

fn kwin_script(env: &Env) -> Check {
    let path = env.script_dir.join("terrain.js");
    if path.exists() {
        Check::ok("kwin script", format!("installed at {}", path.display()))
    } else {
        // It is written to XDG_RUNTIME_DIR at start-up, so its absence before
        // the first run is expected rather than wrong.
        Check::info(
            "kwin script",
            "not written yet; she installs it into XDG_RUNTIME_DIR when she starts",
        )
    }
}

fn install_check(env: &Env) -> Check {
    let s = crate::install::status(&env.install_root);
    if s.conflicted() {
        return Check::warn(
            "autostart",
            s.describe(),
            "Two copies would race at login. Run `nx-wisp uninstall`, then install one of \
             them.",
        );
    }
    if !s.installed() {
        return Check::info("autostart", "not installed; run `nx-wisp install` to start at login");
    }
    Check::ok("autostart", s.describe())
}

fn running(env: &Env) -> Check {
    let held = crate::lock::is_held(&env.config_dir);
    let state = crate::state::load(&env.config_dir);
    match (held, state) {
        (true, Some(st)) => Check::info(
            "running",
            format!(
                "yes, as process {} at {} — {}",
                st.pid,
                fmt::tier_label(st.tier),
                st.headline
            ),
        ),
        (true, None) => Check::info("running", "yes, and she has not published her state yet"),
        (false, _) => Check::info("running", "no"),
    }
}

// ---------------------------------------------------------------------------
// The Wayland registry
// ---------------------------------------------------------------------------

/// Every global the compositor advertises, as `(interface, version)`.
///
/// Registry enumeration and a disconnect. No surface is created here and none
/// may be: the layer surface belongs to `wisp-shell`.
pub fn wayland_globals() -> Result<Vec<(String, u32)>, String> {
    use wayland_client::protocol::wl_registry;
    use wayland_client::{Connection, Dispatch, QueueHandle};

    #[derive(Default)]
    struct Globals(Vec<(String, u32)>);

    impl Dispatch<wl_registry::WlRegistry, ()> for Globals {
        fn event(
            state: &mut Self,
            _: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global { interface, version, .. } = event {
                state.0.push((interface, version));
            }
        }
    }

    let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let mut queue = conn.new_event_queue::<Globals>();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut globals = Globals::default();
    queue.roundtrip(&mut globals).map_err(|e| e.to_string())?;
    globals.0.sort();
    Ok(globals.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempConfig;

    fn env(tmp: &TempConfig) -> Env {
        Env::offline(tmp.path(), &tmp.install_root())
    }

    #[test]
    fn a_bare_environment_produces_a_complete_report_and_never_panics() {
        let tmp = TempConfig::new();
        let checks = run(&env(&tmp));
        assert!(checks.len() >= 10, "{checks:#?}");
        let names: Vec<_> = checks.iter().map(|c| c.name).collect();
        for want in ["wayland", "plasma", "layer-shell", "vulkan", "config", "recorder", "consent"]
        {
            assert!(names.contains(&want), "no {want} check: {names:?}");
        }
        let text = render(&checks);
        assert!(!text.is_empty());
        assert!(!text.contains('!'), "DESIGN.md §9: {text}");
    }

    #[test]
    fn every_failure_says_what_to_do_next() {
        let tmp = TempConfig::new();
        let checks = run(&env(&tmp));
        for c in &checks {
            match c.level {
                Level::Fail | Level::Warn => {
                    let fix = c.fix.as_ref().unwrap_or_else(|| {
                        panic!("{} is a {:?} with no remedy", c.name, c.level)
                    });
                    assert!(fix.len() > 20, "{}: {fix:?} is not advice", c.name);
                }
                Level::Ok | Level::Info => assert!(c.fix.is_none(), "{} explains itself", c.name),
            }
        }
    }

    #[test]
    fn the_config_check_passes_on_a_writable_dir_and_names_the_override() {
        let tmp = TempConfig::new();
        let c = config_dir_check(&env(&tmp));
        assert_eq!(c.level, Level::Ok, "{c:?}");
        assert!(c.detail.contains("NX_WISP_CONFIG_DIR"), "{}", c.detail);
        // …and the probe file is cleaned up.
        assert!(!tmp.path().join(".doctor-write-probe").exists());
    }

    #[test]
    fn the_recorder_check_reports_the_cap_once_there_is_a_trace() {
        let tmp = TempConfig::new();
        assert_eq!(recorder_check(&env(&tmp)).level, Level::Info);

        let r = crate::Recorder::open(
            tmp.path(),
            crate::config::RecorderPrefs::default(),
            1,
        )
        .unwrap();
        r.record_kind(
            0,
            wisp_proto::EventKind::Sensed(wisp_proto::Observation::Idle {
                idle: true,
                for_ms: 1,
            }),
        );
        r.flush();
        let c = recorder_check(&env(&tmp));
        assert_eq!(c.level, Level::Ok, "{c:?}");
        assert!(c.detail.contains("capped at"), "{}", c.detail);
    }

    #[test]
    fn the_consent_check_names_an_invasive_sense_that_is_switched_on() {
        let tmp = TempConfig::new();
        let c = consent(&env(&tmp));
        assert_eq!(c.level, Level::Ok, "invasive is off by default: {c:?}");

        crate::config::set_sense_enabled(tmp.path(), wisp_proto::SenseId::Clipboard, true)
            .unwrap();
        let c = consent(&env(&tmp));
        assert_eq!(c.level, Level::Info);
        assert!(c.detail.contains("Clipboard"), "{}", c.detail);
        assert!(c.detail.contains("live-tell"), "SPEC §0.3: {}", c.detail);
    }

    #[test]
    fn the_install_check_flags_two_mechanisms_racing() {
        let tmp = TempConfig::new();
        let root = tmp.install_root();
        assert_eq!(install_check(&env(&tmp)).level, Level::Info);

        let exec = Path::new("/usr/bin/nx-wisp");
        let opts = crate::install::ApplyOptions {
            dry_run: false,
            run_systemctl: false,
            force: true,
        };
        crate::install::apply(&crate::install::plan(crate::install::Mode::Systemd, &root, exec), opts)
            .unwrap();
        assert_eq!(install_check(&env(&tmp)).level, Level::Ok);

        crate::install::apply(
            &crate::install::plan(crate::install::Mode::Autostart, &root, exec),
            crate::install::ApplyOptions { force: false, ..opts },
        )
        .unwrap_err();
        // Force both into place to reach the conflicted state doctor warns on.
        std::fs::create_dir_all(root.join("systemd/user")).unwrap();
        std::fs::write(crate::install::unit_path(&root), "[Unit]\n").unwrap();
        std::fs::create_dir_all(root.join("autostart")).unwrap();
        std::fs::write(crate::install::desktop_path(&root), "[Desktop Entry]\n").unwrap();
        let c = install_check(&env(&tmp));
        assert_eq!(c.level, Level::Warn, "{c:?}");
        assert!(c.fix.unwrap().contains("uninstall"));
    }

    #[test]
    fn the_running_check_follows_the_lock() {
        let tmp = TempConfig::new();
        assert!(running(&env(&tmp)).detail.contains("no"));
        let _l = crate::lock::acquire(tmp.path()).unwrap();
        assert!(running(&env(&tmp)).detail.contains("yes"));
    }

    #[test]
    fn skipped_probes_are_reported_as_skipped_not_as_passing() {
        let tmp = TempConfig::new();
        let e = env(&tmp);
        assert_eq!(compositor(&e).level, Level::Info);
        assert_eq!(compositor(&e).detail, "not checked");
        assert_eq!(gpus(&e).level, Level::Info);
    }

    #[test]
    fn the_summary_line_matches_the_worst_check() {
        let ok = vec![Check::ok("a", "fine")];
        assert!(render(&ok).contains("Everything she needs is here"));
        let warn = vec![Check::ok("a", "fine"), Check::warn("b", "x", "do y instead")];
        assert!(render(&warn).contains("She will run. 1 thing"));
        let fail = vec![Check::fail("b", "x", "do y instead"), Check::warn("c", "x", "do y")];
        assert!(render(&fail).contains("1 thing above will stop her"));
        assert_eq!(worst(&fail), Level::Fail);
    }

    #[test]
    fn vulkan_is_judged_on_the_loader_and_the_drivers_together() {
        let tmp = TempConfig::new();
        let mut e = env(&tmp);
        e.vulkan_icd_dirs = vec![tmp.path().join("icd.d")];
        // Nothing at all: on a machine with a loader this is a warn, without
        // one it is a fail. Either way it is not Ok and it explains itself.
        let c = vulkan(&e);
        assert_ne!(c.level, Level::Ok);
        assert!(c.fix.is_some());

        std::fs::create_dir_all(tmp.path().join("icd.d")).unwrap();
        std::fs::write(tmp.path().join("icd.d/radeon_icd.json"), b"{}").unwrap();
        let c = vulkan(&e);
        assert!(c.detail.contains("radeon_icd.json"), "{}", c.detail);
    }
}
