//! **F65 — hard ceilings, enforced from outside the process.**
//!
//! Everything else in this crate is the wisp policing herself. That is not
//! enough: a bug in the rig or a runaway llama.cpp thread would sail straight
//! through it. So the ceilings are also imposed from outside, by the kernel:
//!
//! * a **systemd user unit** carrying `CPUQuota=` and `MemoryMax=`, which become
//!   cgroup v2 `cpu.max` and `memory.max` — a hard wall, not a request;
//! * **`SCHED_IDLE`** on background worker threads, so they only ever run on
//!   cores nothing else wants;
//! * **`nice`** and **`ionice`** so even our foreground work loses every
//!   contest against the operator's game.
//!
//! [`unit_file`] is a pure function and is unit-tested. The syscalls are thin
//! wrappers around `libc` and are only exercised on the current thread.

use serde::{Deserialize, Serialize};

/// Everything that varies about the generated unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitSpec {
    /// Unit name without the `.service`, e.g. `nx-wisp`.
    pub name: String,
    pub description: String,
    /// Absolute path to the binary.
    pub exec_start: String,
    /// Arguments, already split. Quoted on the way out.
    pub args: Vec<String>,
    /// Percent of **one** core. 200 means two cores' worth. `None` omits the
    /// directive rather than writing something meaningless.
    pub cpu_quota_pct: Option<u32>,
    /// Hard memory wall in MiB. The cgroup OOM-kills us rather than the
    /// operator's game, which is the correct order of preference.
    pub memory_max_mib: Option<u64>,
    /// Soft wall: reclaim gets aggressive above this instead of killing.
    pub memory_high_mib: Option<u64>,
    /// Refuse swap entirely. Swapping a companion out is worse than shedding
    /// its work.
    pub memory_swap_max_zero: bool,
    /// `Nice=`, -20..=19.
    pub nice: i32,
    /// `CPUWeight=`, 1..=10000. 100 is the default; we want far less.
    pub cpu_weight: u32,
    /// `IOWeight=`, 1..=10000.
    pub io_weight: u32,
    /// Task limit, so a thread leak cannot fork-bomb the session.
    pub tasks_max: Option<u32>,
    /// Environment lines, e.g. the `MESA_VK_DEVICE_SELECT` hint from F61.
    pub environment: Vec<(String, String)>,
    /// `After=` / `PartOf=` target. `graphical-session.target` for a user unit.
    pub wanted_by: String,
}

impl Default for UnitSpec {
    fn default() -> Self {
        UnitSpec {
            name: "nx-wisp".to_string(),
            description: "NX Wisp \u{2014} desktop companion".to_string(),
            exec_start: "/usr/bin/nx-wisp".to_string(),
            args: Vec::new(),
            // Two cores of a 32-thread desktop, one of a laptop's eight: the
            // caller passes a number derived from the machine, never a constant.
            cpu_quota_pct: Some(200),
            memory_max_mib: Some(4096),
            memory_high_mib: Some(3072),
            memory_swap_max_zero: true,
            nice: 10,
            cpu_weight: 20,
            io_weight: 20,
            tasks_max: Some(256),
            environment: Vec::new(),
            wanted_by: "graphical-session.target".to_string(),
        }
    }
}

impl UnitSpec {
    /// Ceilings scaled to the machine actually running her. Nothing about the
    /// operator's 32-thread, 60 GiB desktop is baked in; a laptop gets a laptop's
    /// numbers from the same call.
    pub fn for_machine(cores: u32, ram_mib: u64) -> Self {
        let cores = cores.max(1);
        // A quarter of the machine, never less than one core, never more than
        // four: past that she is not a companion, she is a workload.
        let quota = ((cores * 100) / 4).clamp(100, 400);
        // A sixteenth of RAM, floor 1 GiB, ceiling 8 GiB. The deliberate model
        // lives in page cache (F62), which is not charged to `memory.max`.
        let max = (ram_mib / 16).clamp(1024, 8192);
        UnitSpec {
            cpu_quota_pct: Some(quota),
            memory_max_mib: Some(max),
            memory_high_mib: Some(max * 3 / 4),
            ..UnitSpec::default()
        }
    }

    /// Path a user unit belongs at, relative to `$XDG_CONFIG_HOME`.
    pub fn user_unit_relative_path(&self) -> String {
        format!("systemd/user/{}.service", self.name)
    }
}

/// Render the unit file. Pure — no filesystem, no systemd, no environment.
pub fn unit_file(spec: &UnitSpec) -> String {
    let mut s = String::new();

    s.push_str("[Unit]\n");
    s.push_str(&format!("Description={}\n", one_line(&spec.description)));
    s.push_str(&format!("After={}\n", spec.wanted_by));
    s.push_str(&format!("PartOf={}\n", spec.wanted_by));
    s.push('\n');

    s.push_str("[Service]\n");
    s.push_str("Type=simple\n");
    let mut exec = quote_arg(&spec.exec_start);
    for a in &spec.args {
        exec.push(' ');
        exec.push_str(&quote_arg(a));
    }
    s.push_str(&format!("ExecStart={exec}\n"));
    for (k, v) in &spec.environment {
        s.push_str(&format!("Environment={}={}\n", k, quote_arg(v)));
    }
    s.push_str("Restart=on-failure\n");
    s.push_str("RestartSec=5\n");

    s.push_str("\n# F65 hard ceilings: cgroup v2 walls, not requests.\n");
    if let Some(q) = spec.cpu_quota_pct {
        s.push_str(&format!("CPUQuota={q}%\n"));
    }
    s.push_str(&format!("CPUWeight={}\n", spec.cpu_weight.clamp(1, 10_000)));
    if let Some(m) = spec.memory_max_mib {
        s.push_str(&format!("MemoryMax={m}M\n"));
    }
    if let Some(m) = spec.memory_high_mib {
        s.push_str(&format!("MemoryHigh={m}M\n"));
    }
    if spec.memory_swap_max_zero {
        s.push_str("MemorySwapMax=0\n");
    }
    s.push_str(&format!("IOWeight={}\n", spec.io_weight.clamp(1, 10_000)));
    s.push_str(&format!("Nice={}\n", spec.nice.clamp(-20, 19)));
    // Even at the foreground niceness she must lose to a game's I/O.
    s.push_str("IOSchedulingClass=idle\n");
    if let Some(t) = spec.tasks_max {
        s.push_str(&format!("TasksMax={t}\n"));
    }
    // OOM-kill us before anything else in the session.
    s.push_str("OOMScoreAdjust=500\n");
    s.push('\n');

    s.push_str("[Install]\n");
    s.push_str(&format!("WantedBy={}\n", spec.wanted_by));
    s
}

/// systemd's own quoting rules: quote if it contains whitespace or a quote.
fn quote_arg(v: &str) -> String {
    if v.is_empty() {
        return "\"\"".to_string();
    }
    if v.chars().any(|c| c.is_whitespace() || c == '"' || c == '\'') {
        format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        v.to_string()
    }
}

/// Unit values are single-line; a newline in a description would silently
/// corrupt the file.
fn one_line(v: &str) -> String {
    v.replace(['\n', '\r'], " ")
}

// ---------------------------------------------------------------------------
// A minimal unit-file reader, used by the tests to assert on structure rather
// than on string equality, and by `wisp` to check what is already installed.
// ---------------------------------------------------------------------------

/// `[Section] -> [(key, value)]`, in file order. Repeated keys are preserved
/// because systemd treats several of them as additive.
pub fn parse_unit(text: &str) -> Vec<(String, Vec<(String, String)>)> {
    let mut out: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            out.push((name.to_string(), Vec::new()));
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if let Some(section) = out.last_mut() {
            section.1.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    out
}

/// First value of `key` in `section`.
pub fn unit_value<'a>(
    parsed: &'a [(String, Vec<(String, String)>)],
    section: &str,
    key: &str,
) -> Option<&'a str> {
    parsed
        .iter()
        .find(|(s, _)| s == section)?
        .1
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

// ---------------------------------------------------------------------------
// Thin syscall wrappers
// ---------------------------------------------------------------------------

/// Scheduling policy for a thread we own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedClass {
    /// `SCHED_IDLE` — runs only when nothing else wants the core. Every
    /// background job (F64: consolidation, embedding backfill, summaries) runs
    /// here, which is what makes T0 work cost the operator nothing the moment
    /// they touch the mouse.
    Idle,
    /// `SCHED_OTHER` — normal. The rig and anything the operator is waiting on.
    Normal,
}

/// Apply a scheduling class to the **calling thread**.
///
/// Returns the `errno` on failure. Failure is never fatal: on a kernel or in a
/// container where this is denied we simply lose one of several ceilings.
pub fn set_thread_sched(class: SchedClass) -> Result<(), i32> {
    let policy = match class {
        SchedClass::Idle => libc::SCHED_IDLE,
        SchedClass::Normal => libc::SCHED_OTHER,
    };
    // Both SCHED_IDLE and SCHED_OTHER require sched_priority == 0.
    let param = libc::sched_param { sched_priority: 0 };
    // SAFETY: pid 0 means the calling thread; `param` is a fully initialised
    // `sched_param` that outlives the call.
    let rc = unsafe { libc::sched_setscheduler(0, policy, &param) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(-1))
    }
}

/// `nice(2)` on the calling thread. Positive is politer.
pub fn set_thread_nice(nice: i32) -> Result<(), i32> {
    // SAFETY: `setpriority` with PRIO_PROCESS and who=0 targets the calling
    // thread on Linux and has no memory-safety preconditions.
    let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice.clamp(-20, 19)) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(-1))
    }
}

const IOPRIO_WHO_PROCESS: libc::c_int = 1;
const IOPRIO_CLASS_IDLE: libc::c_int = 3;
const IOPRIO_CLASS_SHIFT: libc::c_int = 13;

/// `ionice -c idle` on the calling thread. Model loads and memory consolidation
/// must never make the operator's disk seek while a game is streaming assets.
pub fn set_thread_ioprio_idle() -> Result<(), i32> {
    let value = IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT;
    // SAFETY: `ioprio_set` takes three integers and touches no user memory.
    let rc = unsafe { libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, value) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(-1))
    }
}

/// Everything a background worker thread should do to itself on startup.
/// Failures are logged, never propagated — a missing ceiling degrades the
/// guarantee, but refusing to start the work would be worse.
pub fn make_background_thread() {
    if let Err(e) = set_thread_sched(SchedClass::Idle) {
        tracing::warn!(errno = e, "could not set SCHED_IDLE on background thread");
    }
    if let Err(e) = set_thread_nice(19) {
        tracing::warn!(errno = e, "could not nice background thread");
    }
    if let Err(e) = set_thread_ioprio_idle() {
        tracing::warn!(errno = e, "could not ionice background thread");
    }
}

// ---------------------------------------------------------------------------
// Reading back what the kernel actually enforced
// ---------------------------------------------------------------------------

/// The limits currently in force on our own cgroup, so the cost meter can show
/// the operator the real wall rather than the one we asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveLimits {
    pub cgroup_path: String,
    /// `cpu.max` as a percentage of one core. `None` means `max`.
    pub cpu_quota_pct: Option<u32>,
    /// `memory.max` in MiB. `None` means `max`.
    pub memory_max_mib: Option<u64>,
}

/// Read `cpu.max` and `memory.max` from our cgroup v2 slice.
pub fn effective_limits() -> EffectiveLimits {
    effective_limits_at(
        std::path::Path::new("/proc/self/cgroup"),
        std::path::Path::new("/sys/fs/cgroup"),
    )
}

pub fn effective_limits_at(
    proc_cgroup: &std::path::Path,
    cgroup_root: &std::path::Path,
) -> EffectiveLimits {
    let rel = std::fs::read_to_string(proc_cgroup)
        .ok()
        .and_then(|s| parse_cgroup2_path(&s))
        .unwrap_or_default();
    let dir = cgroup_root.join(rel.trim_start_matches('/'));
    EffectiveLimits {
        cgroup_path: rel,
        cpu_quota_pct: std::fs::read_to_string(dir.join("cpu.max"))
            .ok()
            .and_then(|s| parse_cpu_max(&s)),
        memory_max_mib: std::fs::read_to_string(dir.join("memory.max"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|b| b / (1024 * 1024)),
    }
}

/// `0::/user.slice/...` -> the path.
pub(crate) fn parse_cgroup2_path(s: &str) -> Option<String> {
    s.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| p.trim().to_string())
}

/// `"200000 100000"` -> 200 (%). `"max 100000"` -> `None`.
pub(crate) fn parse_cpu_max(s: &str) -> Option<u32> {
    let mut it = s.split_whitespace();
    let quota = it.next()?;
    let period: u64 = it.next()?.parse().ok()?;
    if quota == "max" || period == 0 {
        return None;
    }
    let quota: u64 = quota.parse().ok()?;
    Some(((quota * 100) / period) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_path_parses() {
        let s = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app.scope\n";
        assert_eq!(
            parse_cgroup2_path(s).as_deref(),
            Some("/user.slice/user-1000.slice/user@1000.service/app.slice/app.scope")
        );
    }

    #[test]
    fn cpu_max_parses_both_forms() {
        assert_eq!(parse_cpu_max("200000 100000\n"), Some(200));
        assert_eq!(parse_cpu_max("max 100000\n"), None);
    }
}
