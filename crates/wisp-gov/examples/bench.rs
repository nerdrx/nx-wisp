//! Measure what the governor's own perception costs.
//!
//! ```text
//! cargo run --release -p wisp-gov --example bench
//! ```
//!
//! SPEC §0.1 says she costs nothing when it matters, and that has to include the
//! thing doing the measuring. The numbers this prints are what
//! [`wisp_gov::config::Cadence`]'s defaults are derived from; if a change here
//! makes the full poll materially more expensive, the cadence has to change with
//! it or the T3 budget of ~0.5% of one core is gone before she has done
//! anything at all.

use std::time::Instant;

use wisp_gov::{
    config::GovConfig,
    probe::{gpu::SysfsGpuProbe, procs::ProcfsProcProbe, GpuProbe, ProcProbe, Probes},
};
use wisp_proto::Tier;

const N: u32 = 20;

fn bench(name: &str, mut f: impl FnMut()) -> f64 {
    f(); // warm up: the first proc scan has no deltas and no page cache
    let t = Instant::now();
    for _ in 0..N {
        f();
    }
    let per = t.elapsed().as_secs_f64() / N as f64;
    println!("{name:<12} {:>8.3} ms/poll", per * 1000.0);
    per
}

fn main() {
    let cfg = GovConfig::default();

    let mut gpu = SysfsGpuProbe::default();
    bench("sysfs drm", || {
        gpu.read();
    });

    let mut procs = ProcfsProcProbe::new(cfg.procs.clone());
    bench("procfs", || {
        procs.read();
    });

    let mut all = Probes::real(&cfg);
    let full = bench("full poll", || {
        all.poll();
    });

    println!("\ncost of perception at each tier's cadence:");
    for tier in [
        Tier::Feral,
        Tier::Full,
        Tier::Reduced,
        Tier::Lobotomised,
        Tier::Dormant,
    ] {
        let interval = cfg.cadence.for_tier(tier) as f64 / 1000.0;
        let pct = full / interval * 100.0;
        println!(
            "  T{}  every {:>4.1}s  {:>5.2}% of one core",
            tier as u8, interval, pct
        );
    }
}
