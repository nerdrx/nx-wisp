//! The GPU probe, tested against synthetic `/sys/class/drm` trees.
//!
//! The fixtures below are transcriptions of the operator's real machine, probed
//! 2026-08-22 — including the two details that break naive implementations:
//! the *integrated* Radeon is `card0`, and the discrete card owns `renderD128`.

mod common;

use std::path::Path;

use wisp_gov::{
    probe::gpu::{classify_kind, enumerate, SysfsGpuProbe},
    probe::GpuProbe,
    reading::GpuKind,
};

/// `/sys/class/drm/card1` on the operator's desktop: RX 7900 XTX (Navi 31).
fn discrete_card(root: &common::TempDir, card: &str, pci: &str) {
    let d = format!("{card}/device");
    root.write(&format!("{d}/vendor"), "0x1002\n");
    root.write(&format!("{d}/device"), "0x744c\n");
    root.write(&format!("{d}/subsystem_vendor"), "0x1eae\n");
    root.write(
        &format!("{d}/uevent"),
        &format!(
            "DRIVER=amdgpu\nPCI_CLASS=30000\nPCI_ID=1002:744C\nPCI_SLOT_NAME={pci}\n"
        ),
    );
    root.write(&format!("{d}/gpu_busy_percent"), "57\n");
    root.write(&format!("{d}/mem_info_vram_total"), "25753026560\n");
    root.write(&format!("{d}/mem_info_vram_used"), "8341966848\n");
    root.write(&format!("{d}/mem_info_vram_vendor"), "samsung\n");
    root.write(&format!("{d}/mem_info_gtt_used"), "134217728\n");
    root.write(&format!("{d}/mem_busy_percent"), "12\n");
    root.write(&format!("{d}/board_info"), "vbios: x\n");
    root.write(&format!("{d}/rom"), "");
    root.mkdir(&format!("{d}/drm/renderD128"));
    root.mkdir(&format!("{d}/drm/{card}"));
    // hwmon2: a fan, a power cap, edge/junction/mem temperatures.
    root.write(&format!("{d}/hwmon/hwmon2/name"), "amdgpu\n");
    root.write(&format!("{d}/hwmon/hwmon2/fan1_input"), "1200\n");
    root.write(&format!("{d}/hwmon/hwmon2/power1_cap"), "327000000\n");
    root.write(&format!("{d}/hwmon/hwmon2/temp1_input"), "68000\n");
    root.write(&format!("{d}/hwmon/hwmon2/temp2_input"), "78000\n");
    root.write(&format!("{d}/hwmon/hwmon2/temp3_input"), "84000\n");
    // Connector directories, which must be ignored.
    root.mkdir(&format!("{card}-DP-1/device"));
    root.mkdir(&format!("{card}-Writeback-1/device"));
}

/// `/sys/class/drm/card0` on the operator's desktop: Raphael integrated Radeon.
fn integrated_card(root: &common::TempDir, card: &str, pci: &str) {
    let d = format!("{card}/device");
    root.write(&format!("{d}/vendor"), "0x1002\n");
    root.write(&format!("{d}/device"), "0x13c0\n");
    root.write(
        &format!("{d}/uevent"),
        &format!("DRIVER=amdgpu\nPCI_ID=1002:13C0\nPCI_SLOT_NAME={pci}\n"),
    );
    root.write(&format!("{d}/gpu_busy_percent"), "0\n");
    root.write(&format!("{d}/mem_info_vram_total"), "2147483648\n");
    root.write(&format!("{d}/mem_info_vram_used"), "20979712\n");
    // No mem_info_vram_vendor, no rom, no board_info, no mem_busy_percent.
    root.mkdir(&format!("{d}/drm/renderD129"));
    root.mkdir(&format!("{d}/drm/{card}"));
    // hwmon3: power1_input but no cap, no fan.
    root.write(&format!("{d}/hwmon/hwmon3/name"), "amdgpu\n");
    root.write(&format!("{d}/hwmon/hwmon3/power1_input"), "12000000\n");
    root.write(&format!("{d}/hwmon/hwmon3/temp1_input"), "43000\n");
    root.mkdir(&format!("{card}-DP-4/device"));
}

fn operators_desktop() -> common::TempDir {
    let root = common::isolate("sysfs");
    // Deliberately the real, awkward layout: iGPU is card0, dGPU is card1.
    integrated_card(&root, "card0", "0000:7b:00.0");
    discrete_card(&root, "card1", "0000:03:00.0");
    root.write("version", "drm 1.1.0 20060810\n");
    root.mkdir("renderD128");
    root
}

#[test]
fn it_finds_both_cards_and_gets_the_kinds_the_right_way_round() {
    let root = operators_desktop();
    let cards = enumerate(root.path());
    assert_eq!(cards.len(), 2, "connectors and render nodes must be ignored");

    // PCI order, not card order.
    assert_eq!(cards[0].id.pci_slot, "0000:03:00.0");
    assert_eq!(cards[0].id.card_index, 1);
    assert_eq!(cards[0].id.kind, GpuKind::Discrete);
    assert_eq!(cards[0].id.enumeration_index, 0);

    assert_eq!(cards[1].id.pci_slot, "0000:7b:00.0");
    assert_eq!(cards[1].id.card_index, 0);
    assert_eq!(cards[1].id.kind, GpuKind::Integrated);
    assert_eq!(cards[1].id.enumeration_index, 1);
}

#[test]
fn the_render_nodes_are_crossed_over_exactly_as_they_really_are() {
    let root = operators_desktop();
    let cards = enumerate(root.path());
    // card1 (discrete) owns renderD128; card0 (integrated) owns renderD129.
    assert_eq!(
        cards[0].id.render_node.as_deref(),
        Some(Path::new("/dev/dri/renderD128"))
    );
    assert_eq!(
        cards[1].id.render_node.as_deref(),
        Some(Path::new("/dev/dri/renderD129"))
    );
}

#[test]
fn the_numbers_match_the_real_machine() {
    let root = operators_desktop();
    let cards = enumerate(root.path());
    let d = &cards[0];
    assert_eq!(d.vram_total_mib, 24_560);
    assert_eq!(d.vram_used_mib, 7_955);
    assert_eq!(d.busy_pct, 57);
    assert_eq!(d.temp_c, Some(84), "hottest sensor on the card");
    assert_eq!(d.id.vendor_id, 0x1002);
    assert_eq!(d.id.device_id, 0x744c);
    assert_eq!(d.id.driver, "amdgpu", "read from uevent when there is no symlink");

    let i = &cards[1];
    assert_eq!(i.vram_total_mib, 2_048);
    assert_eq!(i.busy_pct, 0);
}

#[test]
fn a_card_with_no_render_node_is_not_our_business() {
    let root = common::isolate("sysfs");
    root.write("card0/device/vendor", "0x1002\n");
    root.write("card0/device/uevent", "PCI_SLOT_NAME=0000:01:00.0\n");
    assert!(enumerate(root.path()).is_empty());
}

#[test]
fn a_missing_sysfs_is_an_empty_list_not_a_panic() {
    let _g = common::isolate("sysfs");
    assert!(enumerate(Path::new("/nonexistent/class/drm")).is_empty());
    let mut p = SysfsGpuProbe::with_root("/nonexistent/class/drm");
    assert!(p.read().is_empty());
}

#[test]
fn an_nvidia_laptop_dgpu_is_discrete_even_with_little_vram() {
    let root = common::isolate("sysfs");
    root.write("card1/device/uevent", "DRIVER=nvidia\nPCI_SLOT_NAME=0000:01:00.0\n");
    let dev = root.join("card1/device");
    assert_eq!(classify_kind(&dev, "nvidia", 4096), GpuKind::Discrete);
}

#[test]
fn an_intel_igpu_is_integrated_and_an_intel_dgpu_is_not() {
    let root = common::isolate("sysfs");

    let igpu = root.mkdir("igpu/device");
    assert_eq!(classify_kind(&igpu, "i915", 0), GpuKind::Integrated);

    // An Arc card runs the same driver, but has local memory and a fan.
    let dgpu = root.mkdir("dgpu/device");
    root.write("dgpu/device/lmem_total_bytes", "17179869184\n");
    root.write("dgpu/device/hwmon/hwmon0/fan1_input", "900\n");
    assert_eq!(classify_kind(&dgpu, "xe", 16_384), GpuKind::Discrete);
}

#[test]
fn a_card_we_cannot_place_is_unknown_and_therefore_never_borrowed() {
    let root = common::isolate("sysfs");
    let dev = root.mkdir("mystery/device");
    // No driver we recognise, no fan, no ROM, no VRAM figures at all.
    assert_eq!(classify_kind(&dev, "someotherdrv", 0), GpuKind::Unknown);
}

#[test]
fn a_virtual_gpu_is_treated_as_integrated_so_we_never_pretend_it_is_a_card() {
    let root = common::isolate("sysfs");
    let dev = root.mkdir("virt/device");
    assert_eq!(classify_kind(&dev, "virtio-gpu", 256), GpuKind::Integrated);
}
