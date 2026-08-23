//! The `nx-wisp` PATH shim — NX Hub's own pattern, borrowed.
//!
//! The hub extract-installs her AppImage to `~/Applications/nx/nx-wisp/…`,
//! which is not on anyone's PATH — so every `nx-wisp …` command in the README
//! and in her own give-up messages would be a lie for a hub install. Like the
//! hub's `nx` shim: refreshed best-effort on every startup, marked so we never
//! clobber a file we did not write, and pointing at the binary that is
//! actually running.

use std::path::PathBuf;

const MARKER: &str = "# nx-wisp-shim";

pub fn refresh() {
    if let Err(e) = try_refresh() {
        tracing::debug!("shim not refreshed: {e}");
    }
}

fn try_refresh() -> std::io::Result<()> {
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(());
    };
    let me = std::fs::canonicalize(std::env::current_exe()?)?;
    // Inside an extracted AppImage the real entry point is AppRun, which sets
    // the library paths up; the inner binary alone may not start. Prefer it.
    let target = me
        .parent()
        .and_then(|d| {
            let apprun = d.join("../../AppRun");
            apprun.canonicalize().ok().filter(|p| p.is_file())
        })
        .unwrap_or(me);

    let bin = PathBuf::from(home).join(".local/bin");
    let shim = bin.join("nx-wisp");
    let body = format!(
        "#!/bin/sh\n{MARKER}\n# Rewritten by nx-wisp on startup; edits will not survive.\nexec \"{}\" \"$@\"\n",
        target.display()
    );
    if let Ok(existing) = std::fs::read_to_string(&shim) {
        if existing == body {
            return Ok(());
        }
        // Never clobber something we did not write.
        if !existing.contains(MARKER) {
            return Ok(());
        }
    }
    std::fs::create_dir_all(&bin)?;
    std::fs::write(&shim, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))?;
    }
    tracing::debug!(shim = %shim.display(), "nx-wisp shim refreshed");
    Ok(())
}
