//! `nx-wisp` — the binary.
//!
//! Deliberately thin. Everything it does lives in the library half of this
//! crate, so `cargo test -p wisp` exercises the same code paths the operator
//! runs rather than a testable rewrite of them.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let invocation = match wisp::cli::parse(args) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(e.code.clamp(0, 255) as u8);
        }
    };

    init_tracing(&invocation);
    // Keep `nx-wisp` on PATH for hub installs — best-effort, marker-guarded,
    // and free when nothing changed.
    wisp::shim::refresh();

    match wisp::cli::dispatch(invocation) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(e.code.clamp(0, 255) as u8)
        }
    }
}

/// Logs go to stderr so a piped `nx-wisp log` stays machine-readable.
///
/// The default is quiet: `run` is a background process and the flight recorder,
/// not stderr, is where its trace belongs. `RUST_LOG` turns it up.
fn init_tracing(inv: &wisp::cli::Invocation) {
    use tracing_subscriber::{fmt, EnvFilter};

    if inv.global.quiet {
        return;
    }
    let default = match inv.command {
        wisp::cli::Command::Run(_) => "nx_wisp=info,wisp=info,warn",
        _ => "warn",
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default));
    let _ = fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
}
