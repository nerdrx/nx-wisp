//! Materialising the companion KWin script.
//!
//! KWin's `loadScript` takes a path, so the script has to exist on disk. It is
//! embedded in the binary and written out fresh on every start — the operator
//! can read it (it is plain JS in their runtime dir) but nothing else can
//! substitute a different one behind our back.

use std::path::{Path, PathBuf};

/// The script, verbatim, with its placeholders intact.
pub const TEMPLATE: &str = include_str!("../../assets/terrain.js");

/// The plugin name KWin knows the script by. `unloadScript` takes this.
pub const PLUGIN_NAME: &str = "nx-wisp-terrain";

/// Our D-Bus identity, which the script calls back into.
pub const SERVICE: &str = "org.nx.Wisp.Senses";
pub const OBJECT: &str = "/org/nx/Wisp/Senses/Terrain";
pub const IFACE: &str = "org.nx.Wisp.Terrain";

/// How long the script coalesces geometry changes before one D-Bus call.
///
/// Measured against a nested KWin 6.7.4 driving a window as fast as it can
/// (`tests/nested/bench.sh`), the feed sustains:
///
/// | `flush_ms` | batches/s | comment |
/// |---|---|---|
/// | 0  | ~966 | one call per KWin signal; the transport ceiling |
/// | 4  | ~198 | |
/// | 8  | ~111 | **default** |
/// | 16 | ~59  | |
/// | 33 | ~30  | |
///
/// 8 ms is the default because `Tier::target_fps` is 60 at T0/T1: terrain
/// arrives roughly twice per rig frame, which is as fresh as she can act on,
/// and it costs KWin's main thread one D-Bus call per 8 ms instead of ten.
/// Uncoalesced the pipeline keeps up with everything KWin can emit, so the cap
/// is a deliberate budget rather than a limitation — raising it is the first
/// thing the governor should do at a reduced tier.
pub const DEFAULT_FLUSH_MS: u32 = 8;

#[derive(Debug, Clone)]
pub struct ScriptConfig {
    pub service: String,
    pub object: String,
    pub iface: String,
    /// Identifies this instance of the script. The Rust side generates it so it
    /// can tell "the script I just installed" from "a script left over from a
    /// previous run".
    pub epoch: u64,
    pub flush_ms: u32,
}

impl Default for ScriptConfig {
    fn default() -> Self {
        ScriptConfig {
            service: SERVICE.to_string(),
            object: OBJECT.to_string(),
            iface: IFACE.to_string(),
            epoch: fresh_epoch(),
            flush_ms: DEFAULT_FLUSH_MS,
        }
    }
}

/// A non-repeating instance id. Monotonic wall time is fine here: this is an
/// identity, never an ordering.
pub fn fresh_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(1)
}

/// Substitute the placeholders. Fails loudly if any survive, because a script
/// with a literal `__NX_WISP_SERVICE__` in it would load, run, and silently
/// never reach us.
pub fn render(cfg: &ScriptConfig) -> Result<String, RenderError> {
    // JSON string escaping is exactly what a JS string literal needs, and it is
    // the difference between a config value and arbitrary injected script.
    let esc = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string());
    let quoted = |s: &str| {
        let j = esc(s);
        j[1..j.len() - 1].to_string()
    };

    let out = TEMPLATE
        .replace("__NX_WISP_SERVICE__", &quoted(&cfg.service))
        .replace("__NX_WISP_OBJECT__", &quoted(&cfg.object))
        .replace("__NX_WISP_IFACE__", &quoted(&cfg.iface))
        .replace("__NX_WISP_EPOCH__", &cfg.epoch.to_string())
        .replace("__NX_WISP_FLUSH_MS__", &cfg.flush_ms.to_string());

    if let Some(pos) = out.find("__NX_WISP_") {
        let end = out[pos..].find("__\n").map(|e| pos + e + 2).unwrap_or(out.len().min(pos + 40));
        return Err(RenderError::Unsubstituted(out[pos..end].to_string()));
    }
    Ok(out)
}

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("KWin script still contains an unsubstituted placeholder: {0}")]
    Unsubstituted(String),
    #[error("could not write the KWin script: {0}")]
    Io(#[from] std::io::Error),
}

/// Where the script is written. `XDG_RUNTIME_DIR` so it is tmpfs, private to the
/// operator, and gone at logout — it is generated state, not configuration.
pub fn script_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("NX_WISP_SCRIPT_DIR") {
        return PathBuf::from(d);
    }
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(d).join("nx-wisp");
    }
    crate::consent::config_dir().join("run")
}

/// Render and write, returning the path to hand to `loadScript`.
pub fn install(cfg: &ScriptConfig, dir: &Path) -> Result<PathBuf, RenderError> {
    let body = render(cfg)?;
    std::fs::create_dir_all(dir)?;
    let path = dir.join("terrain.js");
    std::fs::write(&path, body.as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_has_every_placeholder_the_renderer_knows_about() {
        for p in [
            "__NX_WISP_SERVICE__",
            "__NX_WISP_OBJECT__",
            "__NX_WISP_IFACE__",
            "__NX_WISP_EPOCH__",
            "__NX_WISP_FLUSH_MS__",
        ] {
            assert!(TEMPLATE.contains(p), "template lost {p}");
        }
    }

    #[test]
    fn render_substitutes_everything() {
        let cfg = ScriptConfig { epoch: 42, flush_ms: 8, ..Default::default() };
        let out = render(&cfg).unwrap();
        assert!(!out.contains("__NX_WISP_"), "placeholder survived");
        assert!(out.contains(r#"var SERVICE = "org.nx.Wisp.Senses";"#));
        assert!(out.contains(r#"var OBJECT = "/org/nx/Wisp/Senses/Terrain";"#));
        assert!(out.contains("var EPOCH = 42;"));
        assert!(out.contains("var FLUSH_MS = 8;"));
    }

    #[test]
    fn config_values_cannot_inject_script() {
        let cfg = ScriptConfig {
            service: r#"a"; workspace.activeWindow.closeWindow(); var x = ""#.into(),
            ..Default::default()
        };
        let out = render(&cfg).unwrap();
        // The payload survives as inert text, but the quote that would have
        // ended the string literal is escaped, so it stays inside it.
        assert!(
            out.contains(r#"var SERVICE = "a\"; workspace.activeWindow.closeWindow(); var x = \"";"#),
            "an unescaped quote let a config value out of its string literal:\n{}",
            out.lines().find(|l| l.contains("var SERVICE")).unwrap_or("<missing>")
        );
    }

    #[test]
    fn a_missing_placeholder_substitution_is_an_error() {
        // Simulate the template gaining a placeholder the renderer forgot.
        let doctored = format!("{TEMPLATE}\nvar oops = __NX_WISP_FUTURE__;\n");
        let out = doctored.replace("__NX_WISP_EPOCH__", "1").replace("__NX_WISP_FLUSH_MS__", "8");
        assert!(out.contains("__NX_WISP_"));
    }

    #[test]
    fn script_never_mutates_kwin() {
        // The charter says this feed is read-only. These are the KWin scripting
        // calls that would change the operator's session; none may appear.
        for forbidden in [
            "closeWindow(", "setMaximize(", "frameGeometry = ", "activeWindow = ",
            "workspace.slotSwitch", "registerShortcut", "registerScreenEdge",
            "workspace.createDesktop", "workspace.removeDesktop", "sendClientToScreen",
        ] {
            assert!(!TEMPLATE.contains(forbidden), "terrain script does {forbidden}");
        }
    }

    #[test]
    fn install_writes_a_runnable_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = ScriptConfig { epoch: 7, ..Default::default() };
        let p = install(&cfg, tmp.path()).unwrap();
        assert!(p.exists());
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(body.contains("var EPOCH = 7;"));
        assert!(body.starts_with("// NX Wisp"));
    }

    #[test]
    fn script_dir_honours_the_test_override() {
        let _cfg = crate::testing::TempConfig::new();
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("NX_WISP_SCRIPT_DIR");
        std::env::set_var("NX_WISP_SCRIPT_DIR", tmp.path());
        assert_eq!(script_dir(), tmp.path());
        match prev {
            Some(v) => std::env::set_var("NX_WISP_SCRIPT_DIR", v),
            None => std::env::remove_var("NX_WISP_SCRIPT_DIR"),
        }
    }
}
