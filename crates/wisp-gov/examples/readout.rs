//! Print what the governor sees on this machine, once, and exit.
//!
//! ```text
//! cargo run -p wisp-gov --example readout
//! ```
//!
//! This is the only thing in the crate that touches the real `/sys` and `/proc`,
//! and it exists so the probes can be sanity-checked against a real machine
//! without a compositor, a window or a GPU context.

use wisp_gov::{ceiling, config::GovConfig, probe::Probes, Governor};

fn main() {
    let cfg = GovConfig::default();

    // Raw probe output first, so a wrong reading is visible before the ladder
    // has a chance to interpret it.
    let mut probes = Probes::real(&cfg);
    let snap = probes.poll();

    println!("cards:");
    for g in &snap.gpus {
        println!(
            "  [{}] card{} {:?} {} {:04x}:{:04x} {} busy {}% vram {}/{} MiB temp {:?}",
            g.id.enumeration_index,
            g.id.card_index,
            g.id.kind,
            g.id.pci_slot,
            g.id.vendor_id,
            g.id.device_id,
            g.id
                .render_node
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            g.busy_pct,
            g.vram_used_mib,
            g.vram_total_mib,
            g.temp_c,
        );
    }
    println!(
        "cpu:   {} cores, load {:.2} ({:.2}/core), psi {:.2}",
        snap.cpu.cores,
        snap.cpu.load1,
        snap.cpu.load_per_core(),
        snap.cpu.psi_some_avg10
    );
    println!(
        "mem:   {}/{} MiB available, psi {:.2}",
        snap.mem.available_mib, snap.mem.total_mib, snap.mem.psi_some_avg10
    );
    println!("power: {:?}", snap.power);

    // Then the whole loop. Twice with a second between, because CPU percentages
    // are deltas and two steps back to back would measure nothing but the cost
    // of taking the measurement.
    let mut gov = Governor::real(cfg);
    gov.step();
    std::thread::sleep(std::time::Duration::from_secs(1));
    let step = gov.step();

    println!("\n{}", step.explanation);
    println!("{}", step.cost.headline);
    println!("devices: {}", step.devices.note);
    println!("vram:    {}", step.vram.note);
    println!("procs:    {:?}", step.snapshot.procs);
    println!("measured: {:?}", step.cost.measured);
    println!("cgroup:   {:?}", ceiling::effective_limits());
}
