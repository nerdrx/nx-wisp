//! F65 — hard ceilings. The unit generator is a pure function, so it is tested
//! as one; where `systemd-analyze` is on the machine, the generated file is also
//! handed to systemd itself for verification.

mod common;

use wisp_gov::ceiling::{
    effective_limits_at, parse_unit, set_thread_ioprio_idle, set_thread_nice, set_thread_sched,
    unit_file, unit_value, SchedClass, UnitSpec,
};

fn spec() -> UnitSpec {
    UnitSpec {
        exec_start: "/usr/bin/true".to_string(),
        ..UnitSpec::default()
    }
}

#[test]
fn the_unit_has_the_sections_and_the_ceilings() {
    let _g = common::isolate("ceiling");
    let text = unit_file(&spec());
    let p = parse_unit(&text);

    let sections: Vec<&str> = p.iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(sections, vec!["Unit", "Service", "Install"]);

    // The two that matter: these become cgroup v2 `cpu.max` and `memory.max`.
    assert_eq!(unit_value(&p, "Service", "CPUQuota"), Some("200%"));
    assert_eq!(unit_value(&p, "Service", "MemoryMax"), Some("4096M"));
    assert_eq!(unit_value(&p, "Service", "MemoryHigh"), Some("3072M"));
    assert_eq!(unit_value(&p, "Service", "MemorySwapMax"), Some("0"));
    assert_eq!(unit_value(&p, "Service", "Nice"), Some("10"));
    assert_eq!(unit_value(&p, "Service", "IOSchedulingClass"), Some("idle"));
    assert_eq!(unit_value(&p, "Service", "TasksMax"), Some("256"));
    assert_eq!(unit_value(&p, "Service", "Type"), Some("simple"));
    assert_eq!(
        unit_value(&p, "Install", "WantedBy"),
        Some("graphical-session.target")
    );
}

#[test]
fn every_line_is_valid_unit_syntax() {
    let _g = common::isolate("ceiling");
    let text = unit_file(&spec());
    for (n, line) in text.lines().enumerate() {
        let n = n + 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            assert!(line.ends_with(']'), "line {n}: unterminated section: {line:?}");
            continue;
        }
        let (k, _) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("line {n} is neither a section nor key=value: {line:?}"));
        assert!(!k.is_empty(), "line {n}: empty key");
        assert!(
            k.chars().all(|c| c.is_ascii_alphanumeric()),
            "line {n}: bad key {k:?}"
        );
        assert!(!line.starts_with(' '), "line {n}: leading space");
    }
    assert!(text.ends_with('\n'));
}

#[test]
fn ceilings_scale_to_the_machine_and_are_never_hardcoded() {
    let _g = common::isolate("ceiling");
    // The operator's desktop: 32 threads, 60 GiB.
    let desk = UnitSpec::for_machine(32, 61_820);
    assert_eq!(desk.cpu_quota_pct, Some(400), "a quarter, capped at four cores");
    assert_eq!(desk.memory_max_mib, Some(3_863));

    // Their laptop: 8 threads, 16 GiB. Same call, different numbers.
    let lap = UnitSpec::for_machine(8, 15_800);
    assert_eq!(lap.cpu_quota_pct, Some(200));
    assert_eq!(lap.memory_max_mib, Some(1_024), "floored at 1 GiB");

    // Something tiny.
    let tiny = UnitSpec::for_machine(1, 2_048);
    assert_eq!(tiny.cpu_quota_pct, Some(100), "never below one core");
    assert_eq!(tiny.memory_max_mib, Some(1_024));

    // And a machine much bigger than either.
    let big = UnitSpec::for_machine(256, 1_048_576);
    assert_eq!(big.cpu_quota_pct, Some(400));
    assert_eq!(big.memory_max_mib, Some(8_192), "capped: she is not a workload");
}

#[test]
fn arguments_and_environment_are_quoted() {
    let _g = common::isolate("ceiling");
    let s = UnitSpec {
        exec_start: "/opt/NX Wisp/nx-wisp".to_string(),
        args: vec!["--config".into(), "/home/n/my configs/wisp.toml".into()],
        environment: vec![("MESA_VK_DEVICE_SELECT".into(), "1002:13c0".into())],
        ..spec()
    };
    let text = unit_file(&s);
    let p = parse_unit(&text);
    assert_eq!(
        unit_value(&p, "Service", "ExecStart"),
        Some(r#""/opt/NX Wisp/nx-wisp" --config "/home/n/my configs/wisp.toml""#)
    );
    assert_eq!(
        unit_value(&p, "Service", "Environment"),
        Some("MESA_VK_DEVICE_SELECT=1002:13c0")
    );
}

#[test]
fn a_multiline_description_cannot_corrupt_the_file() {
    let _g = common::isolate("ceiling");
    let s = UnitSpec {
        description: "line one\nExecStart=/bin/evil".to_string(),
        ..spec()
    };
    let p = parse_unit(&unit_file(&s));
    assert_eq!(
        unit_value(&p, "Unit", "Description"),
        Some("line one ExecStart=/bin/evil")
    );
    // And no `ExecStart` snuck into the wrong section.
    assert!(unit_value(&p, "Unit", "ExecStart").is_none());
}

#[test]
fn omitted_limits_omit_their_directives_rather_than_writing_nonsense() {
    let _g = common::isolate("ceiling");
    let s = UnitSpec {
        cpu_quota_pct: None,
        memory_max_mib: None,
        memory_high_mib: None,
        memory_swap_max_zero: false,
        tasks_max: None,
        ..spec()
    };
    let text = unit_file(&s);
    for key in [
        "CPUQuota",
        "MemoryMax",
        "MemoryHigh",
        "MemorySwapMax",
        "TasksMax",
    ] {
        assert!(!text.contains(key), "{key} should not appear");
    }
}

#[test]
fn the_user_unit_path_is_where_systemd_looks() {
    let _g = common::isolate("ceiling");
    assert_eq!(
        spec().user_unit_relative_path(),
        "systemd/user/nx-wisp.service"
    );
}

/// If systemd is installed, let it be the judge. Skipped where it is not.
#[test]
fn systemd_itself_accepts_the_generated_unit() {
    let root = common::isolate("ceiling");
    let Ok(out) = std::process::Command::new("systemd-analyze")
        .arg("--version")
        .output()
    else {
        eprintln!("systemd-analyze not present; skipping");
        return;
    };
    if !out.status.success() {
        eprintln!("systemd-analyze not usable; skipping");
        return;
    }

    let path = root.write("nx-wisp.service", &unit_file(&spec()));
    let verify = std::process::Command::new("systemd-analyze")
        .arg("verify")
        .arg("--user")
        .arg(&path)
        .output()
        .expect("run systemd-analyze verify");
    let stderr = String::from_utf8_lossy(&verify.stderr);
    let stdout = String::from_utf8_lossy(&verify.stdout);

    // `verify` warns about unrelated environment things (missing units to order
    // against, for instance). What must not appear is a complaint about one of
    // *our* directives.
    for bad in [
        "Unknown lvalue",
        "Unknown section",
        "Failed to parse",
        "Invalid",
        "not a valid",
    ] {
        assert!(
            !stderr.contains(bad) && !stdout.contains(bad),
            "systemd rejected the unit ({bad}):\n{stderr}{stdout}"
        );
    }
}

#[test]
fn effective_limits_are_read_back_from_cgroup_v2() {
    let root = common::isolate("ceiling");
    root.write("proc/cgroup", "0::/user.slice/user-1000.slice/app.scope\n");
    root.write("cgroup/user.slice/user-1000.slice/app.scope/cpu.max", "200000 100000\n");
    root.write(
        "cgroup/user.slice/user-1000.slice/app.scope/memory.max",
        "4294967296\n",
    );
    let l = effective_limits_at(&root.join("proc/cgroup"), &root.join("cgroup"));
    assert_eq!(l.cgroup_path, "/user.slice/user-1000.slice/app.scope");
    assert_eq!(l.cpu_quota_pct, Some(200));
    assert_eq!(l.memory_max_mib, Some(4096));
}

#[test]
fn an_unlimited_cgroup_reports_no_limit_rather_than_zero() {
    let root = common::isolate("ceiling");
    root.write("proc/cgroup", "0::/\n");
    root.write("cgroup/cpu.max", "max 100000\n");
    let l = effective_limits_at(&root.join("proc/cgroup"), &root.join("cgroup"));
    assert_eq!(l.cpu_quota_pct, None);
    assert_eq!(l.memory_max_mib, None);
}

/// The syscall wrappers, on this thread, in a thread of their own so the rest
/// of the suite is not left running at `SCHED_IDLE`.
#[test]
fn a_background_thread_can_lower_its_own_priority() {
    let _g = common::isolate("ceiling");
    std::thread::spawn(|| {
        // These are allowed to fail (a container may forbid them); what must
        // never happen is a panic or an unsound call.
        let sched = set_thread_sched(SchedClass::Idle);
        let nice = set_thread_nice(19);
        let io = set_thread_ioprio_idle();
        if let Err(e) = sched {
            eprintln!("SCHED_IDLE refused: errno {e}");
        }
        if let Err(e) = nice {
            eprintln!("nice refused: errno {e}");
        }
        if let Err(e) = io {
            eprintln!("ionice refused: errno {e}");
        }
        // Going back to normal is the one that has to work, or a worker thread
        // could never be reused.
        set_thread_sched(SchedClass::Normal).expect("SCHED_OTHER must be restorable");
    })
    .join()
    .expect("thread must not panic");
}
