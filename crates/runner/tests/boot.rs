//! Boot-level integration tests: each boots a galexy.os kernel image in
//! headless QEMU and asserts on the exit code and COM1 output.

mod common;

use common::{
    boot, boot_and_type, boot_and_type_ps2, boot_and_type_uefi, boot_galfs_once,
    boot_galfs_once_virtio, boot_liveness, boot_uefi, boot_with_galfs,
    boot_with_galfs_both_corrupt, boot_with_galfs_cache, boot_with_galfs_crash,
    boot_with_galfs_part, boot_with_galfs_recover, boot_with_galfs_torn, boot_with_galfs_virtio,
    boot_with_galfs_virtio_legacy, image, uart_login_serial, use_kvm, GalfsDiskCache,
    GALFS_PART_BYTE_OFF, QEMU_EXIT_SUCCESS,
};
use std::time::Duration;

#[test]
fn test_kernel_runs_and_passes() {
    let (code, serial) = boot(&image("test-basic"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-basic should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-basic] passed"),
        "test-basic success marker missing; serial:\n{serial}"
    );
}

#[test]
fn should_panic_kernel_exits_successfully() {
    let (code, serial) = boot(&image("test-should-panic"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "expected panic should map to Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[PANIC]"),
        "panic report missing from serial:\n{serial}"
    );
}

#[test]
fn memory_test_passes() {
    let (code, serial) = boot(&image("test-memory"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-memory should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-memory] passed"),
        "test-memory success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[mm] frame allocator ready"),
        "frame allocator init marker missing; serial:\n{serial}"
    );
}

#[test]
fn paging_test_passes() {
    let (code, serial) = boot(&image("test-paging"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-paging should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-paging] page fault fired as expected"),
        "unmapped-access fault marker missing; serial:\n{serial}"
    );
}

#[test]
fn heap_test_passes() {
    let (code, serial) = boot(&image("test-heap"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-heap should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-heap] passed"),
        "test-heap success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[heap] ready"),
        "heap init marker missing; serial:\n{serial}"
    );
}

#[test]
fn shutdown_test_powers_off() {
    let (code, serial) = boot(&image("test-shutdown"));
    assert!(
        serial.contains("[test-shutdown] powering off"),
        "shutdown never started; serial:\n{serial}"
    );
    assert!(
        !serial.contains("[PANIC]"),
        "shutdown returned; serial:\n{serial}"
    );
    assert!(
        code.is_some(),
        "QEMU did not exit after shutdown; serial:\n{serial}"
    );
}

#[test]
fn reboot_test_resets() {
    let (code, serial) = boot(&image("test-reboot"));
    assert!(
        serial.contains("[test-reboot] resetting"),
        "reboot never started; serial:\n{serial}"
    );
    assert!(
        !serial.contains("[PANIC]"),
        "reboot returned; serial:\n{serial}"
    );
    assert!(
        code.is_some(),
        "QEMU did not exit after reboot; serial:\n{serial}"
    );
}

#[test]
fn acpi_test_passes() {
    // MADT discovery is architecture truth for both boot paths: run it on
    // the BIOS image AND the UEFI image (OVMF's tables must parse too).
    let (code, serial) = boot(&image("test-acpi"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-acpi should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-acpi] passed"),
        "test-acpi success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[acpi] madt ready"),
        "MADT discovery marker missing; serial:\n{serial}"
    );

    // The UEFI image boots OVMF; its tables must validate the same way.
    let mut last = String::new();
    for _ in 0..3 {
        last = boot_uefi(&image("test-acpi"), Duration::from_secs(30));
        if last.contains("[test-acpi] passed") {
            break;
        }
    }
    assert!(
        last.contains("[test-acpi] passed"),
        "test-acpi under UEFI never passed after 3 attempts; serial:\n{last}"
    );
}

#[test]
fn apic_test_passes() {
    let (code, serial) = boot(&image("test-apic"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-apic should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-apic] passed"),
        "test-apic success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[apic] lapic up"),
        "LAPIC enable marker missing; serial:\n{serial}"
    );
    // `-cpu max,+x2apic` requests the MSR path. TCG keeps the bit only when
    // the accelerator implements x2APIC (QEMU 9+). Older TCG strips it and
    // the guest must stay on xAPIC. `bin/test-apic` asserts that match.
    if serial.contains("[apic] x2apic enabled") {
        assert!(
            serial.contains("detected mode: X2Apic"),
            "x2APIC was enabled but the guest stayed off the MSR path; serial:\n{serial}"
        );
    } else {
        assert!(
            serial.contains("[apic] x2apic not enumerated"),
            "missing x2APIC decision; serial:\n{serial}"
        );
        assert!(
            serial.contains("detected mode: XApic"),
            "no x2APIC bit must stay on MMIO; serial:\n{serial}"
        );
    }
    // TSC-deadline is selected when CPUID.1 ECX bit 24 is set. Current QEMU
    // TCG does not enumerate that bit, so the suite observes one-shot mode.
    // `bin/test-apic` checks the LVT against the same bit.
    if serial.contains("timer tsc-deadline") {
        assert!(
            serial.contains("timer ticking"),
            "TSC-deadline timer did not fire; serial:\n{serial}"
        );
    } else {
        assert!(
            serial.contains("timer oneshot"),
            "one-shot timer was not selected; serial:\n{serial}"
        );
    }
    assert!(
        serial.contains("[pci] ecam"),
        "q35 boot must use PCIe ECAM; serial:\n{serial}"
    );
    assert!(
        serial.contains("[kbd] virtio-input"),
        "virtio-keyboard was not claimed; serial:\n{serial}"
    );
    if serial.contains("8259=no") {
        assert!(
            serial.contains("remap skipped"),
            "FADT reported no 8259 but the PIC was remapped; serial:\n{serial}"
        );
    } else {
        assert!(
            serial.contains("remapped + fully masked"),
            "8259 present or assumed must be remapped and masked; serial:\n{serial}"
        );
    }
}

#[test]
fn heap_grow_test_passes() {
    let (code, serial) = boot(&image("test-heapgrow"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-heapgrow should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-heapgrow] passed"),
        "test-heapgrow success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[heap] grown:"),
        "heap growth marker missing; serial:\n{serial}"
    );
}

#[test]
fn screen_test_passes() {
    let (code, serial) = boot(&image("test-screen"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-screen should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-screen] passed"),
        "test-screen success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("\u{1b}[31mZ"),
        "console did not mirror the CSI bytes to serial:\n{serial}"
    );
}

#[test]
fn audit_console_test_passes() {
    let (code, serial) = boot(&image("test-audit"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-audit should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-audit] passed"),
        "test-audit success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[kbd] queue full"),
        "keyboard overflow must warn on serial; serial:\n{serial}"
    );
}

#[test]
fn sched_test_passes() {
    let (code, serial) = boot(&image("test-sched"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-sched should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-sched] passed"),
        "test-sched success marker missing; serial:\n{serial}"
    );
}

#[test]
fn preempt_test_passes() {
    let (code, serial) = boot(&image("test-preempt"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-preempt should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-preempt] passed"),
        "test-preempt success marker missing; serial:\n{serial}"
    );
}

#[test]
fn cloneroot_test_passes() {
    let (code, serial) = boot(&image("test-cloneroot"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-cloneroot should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-cloneroot] passed"),
        "test-cloneroot success marker missing; serial:\n{serial}"
    );
}

#[test]
fn freshl4_test_passes() {
    let (code, serial) = boot(&image("test-freshl4"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-freshl4 should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-freshl4] passed"),
        "test-freshl4 success marker missing; serial:\n{serial}"
    );
}

#[test]
fn reuse_test_passes() {
    let (code, serial) = boot(&image("test-reuse"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-reuse should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-reuse] keeper live, exited 80"),
        "test-reuse live-name marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-reuse] passed"),
        "test-reuse success marker missing; serial:\n{serial}"
    );
}

#[test]
fn threadexit_test_passes() {
    let (code, serial) = boot(&image("test-threadexit"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-threadexit should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-threadexit] passed"),
        "test-threadexit success marker missing; serial:\n{serial}"
    );
    // Per-CPU reaping (SMP M18): the three exits can be reaped in chunks
    // per owner CPU (any split of 3, e.g. "reaped 3" or "reaped 1" twice).
    let total: usize = serial
        .lines()
        .filter_map(|l| {
            let idx = l.find("reaped ")?;
            let rest = &l[idx + "reaped ".len()..];
            let word = rest.split(' ').next()?;
            word.parse::<usize>().ok()
        })
        .sum();
    assert!(
        total == 3,
        "reaper freed {} stack(s) across CPUs; serial:\n{serial}",
        total
    );
}

#[test]
fn rings_test_passes() {
    let (code, serial) = boot(&image("test-rings"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-rings should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-rings] passed"),
        "test-rings success marker missing; serial:\n{serial}"
    );
}

#[test]
fn userpreempt_test_passes() {
    let (code, serial) = boot(&image("test-userpreempt"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-userpreempt should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-userpreempt] passed"),
        "test-userpreempt success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("ready (own tree cr3="),
        "user task spawn marker missing; serial:\n{serial}"
    );
}

#[test]
fn syscall_test_passes() {
    let (code, serial) = boot(&image("test-syscall"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-syscall should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-syscall] passed"),
        "test-syscall success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("SYSCALL/SYSRET live"),
        "syscall MSR init marker missing; serial:\n{serial}"
    );
}

#[test]
fn user_lifecycle_test_passes() {
    let (code, serial) = boot(&image("test-user"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-user should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-user] passed"),
        "test-user success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("task 'uhello' exited (syscall)"),
        "user task exit marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("ready (own tree cr3="),
        "user task spawn marker missing; serial:\n{serial}"
    );
}

#[test]
fn treechurn_test_passes() {
    let (code, serial) = boot(&image("test-treechurn"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-treechurn should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-treechurn] passed"),
        "test-treechurn success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("freed task 'churn' tree"),
        "tree-walk marker missing; serial:\n{serial}"
    );
}

#[test]
fn userfault_test_passes() {
    let (code, serial) = boot(&image("test-userfault"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-userfault should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-userfault] passed"),
        "test-userfault success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("task 'boomer' exited (page fault)"),
        "fault-kill marker missing; serial:\n{serial}"
    );
}

#[test]
fn wx_test_passes() {
    let (code, serial) = boot(&image("test-wx"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-wx should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-wx] passed"),
        "test-wx success marker missing; serial:\n{serial}"
    );
}

#[test]
fn smep_test_passes() {
    let (code, serial) = boot(&image("test-smep"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-smep should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("smep=1"),
        "SMEP was not enabled; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-smep] fault"),
        "SMEP fault marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-smep] passed"),
        "test-smep success marker missing; serial:\n{serial}"
    );
}

#[test]
fn smap_test_passes() {
    let (code, serial) = boot(&image("test-smap"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-smap should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("smap=1"),
        "SMAP was not enabled; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-smap] fault"),
        "SMAP fault marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-smap] passed"),
        "test-smap success marker missing; serial:\n{serial}"
    );
}

#[test]
fn umip_test_passes() {
    let (code, serial) = boot(&image("test-umip"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-umip should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("umip=1"),
        "UMIP was not enabled; serial:\n{serial}"
    );
    assert!(
        serial.contains("ring-3 task fault"),
        "ring-3 fault marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-umip] passed"),
        "test-umip success marker missing; serial:\n{serial}"
    );
}

#[test]
fn kaslr_kernel_base_differs_across_boots() {
    let (code_a, serial_a) = boot(&image("test-kaslr"));
    let (code_b, serial_b) = boot(&image("test-kaslr"));
    assert_eq!(
        code_a,
        Some(QEMU_EXIT_SUCCESS),
        "test-kaslr boot A should succeed; serial:\n{serial_a}"
    );
    assert_eq!(
        code_b,
        Some(QEMU_EXIT_SUCCESS),
        "test-kaslr boot B should succeed; serial:\n{serial_b}"
    );
    let base = |serial: &str| -> u64 {
        let line = serial
            .lines()
            .find(|line| line.contains("[kaslr] kernel_image_offset="))
            .unwrap_or_else(|| panic!("kaslr line missing; serial:\n{serial}"));
        let hex = line.split('=').nth(1).unwrap().trim();
        u64::from_str_radix(hex.trim_start_matches("0x"), 16).expect("kaslr offset")
    };
    let a = base(&serial_a);
    let b = base(&serial_b);
    assert_ne!(
        a, b,
        "KASLR kernel base matched across boots ({a:#x}); A:\n{serial_a}\nB:\n{serial_b}"
    );
}

/// Hostile ELF suite: forged headers and segments are refused with a
/// `SysError` by the spawn gate, never a kernel panic; real programs pass.
#[test]
fn badelf_test_passes() {
    let (code, serial) = boot(&image("test-badelf"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-badelf should exit with Success; serial:\n{serial}"
    );
    for case in [
        "filesz > memsz",
        "memsz past the user window",
        "overlapping PT_LOAD",
        "truncated header/table",
        "W|X segment",
        "ET_DYN (PIE)",
    ] {
        assert!(
            serial.contains(&format!("[test-badelf] {case}: refused with")),
            "test-badelf case '{case}' missing; serial:\n{serial}"
        );
    }
    assert!(
        serial.contains("[test-badelf] passed"),
        "test-badelf success marker missing; serial:\n{serial}"
    );
}

/// Negative suite: a logged-out seat cannot spawn or create (kernel rule,
/// not shell UX); a bare program without inherited cards reads the ramdisk
/// but cannot create, open, or remove anything in galfs.
#[test]
fn negative_test_passes() {
    let (code, serial) = boot(&image("test-negative"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-negative should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-negative] pre-login spawn denied"),
        "pre-login marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-negative] bare spawn cannot touch galfs"),
        "bare-spawn marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-negative] passed"),
        "test-negative success marker missing; serial:\n{serial}"
    );
}

/// Soak: rounds of pipe + galfs + spawn(hello, wait) + sleep with idle
/// gaps; frames, pipe slots, thread slots, and galfs blocks return to
/// baseline exactly after every round.
#[test]
fn soak_test_passes() {
    let (code, serial) = boot(&image("test-soak"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-soak should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("no leak in frames / pipes / threads / blocks"),
        "soak summary missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-soak] passed"),
        "test-soak success marker missing; serial:\n{serial}"
    );
}

/// Steal fairness under load: both CPUs do work, and once the AP's
/// rotation drains it steals a BSP-pinned worker (`steal_count` moves).
#[test]
fn fairness_test_passes() {
    let (code, serial) = boot(&image("test-fairness"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-fairness should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-fairness] cpu0="),
        "fairness counters missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-fairness] passed"),
        "test-fairness success marker missing; serial:\n{serial}"
    );
}

/// Pathological input: a tight ring-3 console write loop never exceeds
/// the 512 B/tick budget, never errors, and the timer keeps ticking.
#[test]
fn pathological_test_passes() {
    let (code, serial) = boot(&image("test-pathological"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-pathological should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-pathological] passed"),
        "test-pathological success marker missing; serial:\n{serial}"
    );
}

#[test]
fn rm_test_passes() {
    let (code, serial) = boot(&image("test-rm"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-rm should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-rm] passed"),
        "test-rm success marker missing; serial:\n{serial}"
    );
}

#[test]
fn galfs_test_passes() {
    let (code, serial) = boot(&image("test-galfs"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-galfs should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-galfs] passed"),
        "test-galfs success marker missing; serial:\n{serial}"
    );
}

#[test]
fn blocks_test_passes() {
    let (code, serial) = boot(&image("test-blocks"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-blocks should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-blocks] passed"),
        "test-blocks success marker missing; serial:\n{serial}"
    );
}

#[test]
fn ops_test_passes() {
    let (code, serial) = boot(&image("test-ops"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-ops should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-ops] passed"),
        "test-ops success marker missing; serial:\n{serial}"
    );
}

#[test]
fn quota_test_passes() {
    let (code, serial) = boot(&image("test-quota"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-quota should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-quota] passed"),
        "test-quota success marker missing; serial:\n{serial}"
    );
}

#[test]
fn shares_test_passes() {
    let (code, serial) = boot(&image("test-shares"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-shares should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-shares] passed"),
        "test-shares success marker missing; serial:\n{serial}"
    );
}

#[test]
fn indirect_test_passes() {
    let (code, serial) = boot(&image("test-indirect"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-indirect should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-indirect] passed"),
        "test-indirect success marker missing; serial:\n{serial}"
    );
}

#[test]
fn cards_test_passes() {
    let (code, serial) = boot(&image("test-cards"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-cards should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-cards] passed"),
        "test-cards success marker missing; serial:\n{serial}"
    );
}

#[test]
fn paths_test_passes() {
    let (code, serial) = boot(&image("test-paths"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-paths should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-paths] passed"),
        "test-paths success marker missing; serial:\n{serial}"
    );
}

#[test]
fn galfs_disk_persists_across_reboot() {
    let result = boot_with_galfs(&image("test-galfs-disk"));
    assert!(
        result.1.contains("[pci] config via 0xCF8"),
        "IDE slave matrix must stay on -M pc (no MCFG); serial:\n{}",
        result.1
    );
    assert_galfs_disk_persists(result, "writethrough");
}

/// Guest FLUSH CACHE must make the inactive-slot commit durable when the
/// host page cache may buffer (`cache=writeback`).
#[test]
fn galfs_disk_persists_writeback_cache() {
    assert_galfs_disk_persists(
        boot_with_galfs_cache(&image("test-galfs-disk"), GalfsDiskCache::Writeback),
        "writeback",
    );
}

/// Same persistence bar with `cache=none` (bypass host page cache).
#[test]
fn galfs_disk_persists_none_cache() {
    assert_galfs_disk_persists(
        boot_with_galfs_cache(&image("test-galfs-disk"), GalfsDiskCache::None),
        "none",
    );
}

/// Persistence via virtio-blk-pci (virtio 1.x, MSI-X) on `-M q35`.
#[test]
fn galfs_disk_persists_virtio_blk() {
    let (code1, serial1, img, code2, serial2) = boot_with_galfs_virtio(&image("test-galfs-disk"));
    assert!(
        serial1.contains("[pci] ecam"),
        "virtio disk boot must use ECAM; serial:\n{serial1}"
    );
    assert!(
        serial1.contains("[virtio-blk] virtio 1.x ready"),
        "guest must bind virtio 1.x; serial:\n{serial1}"
    );
    assert!(
        serial1.contains("msi-x"),
        "virtio-blk must take MSI-X; serial:\n{serial1}"
    );
    assert_galfs_disk_persists((code1, serial1, img, code2, serial2), "virtio-pci");
}

/// Legacy I/O BAR remains a named fallback when modern virtio is disabled.
#[test]
fn galfs_disk_persists_virtio_legacy() {
    let (code1, serial1, img, code2, serial2) =
        boot_with_galfs_virtio_legacy(&image("test-galfs-disk"));
    assert!(
        serial1.contains("legacy IO BAR"),
        "legacy virtio path must name itself; serial:\n{serial1}"
    );
    assert!(
        !serial1.contains("virtio 1.x ready"),
        "disable-modern must not take the 1.x path; serial:\n{serial1}"
    );
    assert_galfs_disk_persists((code1, serial1, img, code2, serial2), "virtio-legacy");
}

/// virtio-blk DMA follows the buffer's physical pages, not `translate(buf)`
/// plus a length: a sector straddling two non-adjacent frames lands in
/// both and never in the frame that happens to be physically next. Both
/// transports share the descriptor builder; both are booted.
#[test]
fn dmasplit_test_passes() {
    for legacy in [false, true] {
        let (code, serial) = boot_galfs_once_virtio(&image("test-dmasplit"), legacy);
        let label = if legacy { "legacy" } else { "virtio 1.x" };
        assert!(
            !serial.contains("[PANIC]") && !serial.contains("PAGE FAULT in ring 0"),
            "test-dmasplit ({label}) panicked or faulted; serial:\n{serial}"
        );
        assert!(
            serial.contains("[test-dmasplit] straddling read stayed in the buffer")
                && serial.contains("[test-dmasplit] multi-page read stayed in the buffer")
                && serial.contains("[test-dmasplit] straddling write came from the buffer")
                && serial.contains("[test-dmasplit] 48-sector round trip"),
            "test-dmasplit ({label}) did not reach every stage; serial:\n{serial}"
        );
        assert!(
            serial.contains("[test-dmasplit] passed"),
            "test-dmasplit ({label}) must pass; serial:\n{serial}"
        );
        assert_eq!(
            code,
            Some(QEMU_EXIT_SUCCESS),
            "test-dmasplit ({label}) exit code; serial:\n{serial}"
        );
    }
}

/// GALF dual slots start at LBA 2048 — absolute LBA 0 stays empty.
#[test]
fn galfs_disk_persists_partition_offset() {
    let (code1, serial1, img, code2, serial2) = boot_with_galfs_part(&image("test-galfs-part"));
    assert_eq!(
        code1,
        Some(QEMU_EXIT_SUCCESS),
        "test-galfs-part write boot should exit with Success; serial:\n{serial1}"
    );
    assert!(
        serial1.contains("[test-galfs-part] wrote"),
        "write marker missing; serial:\n{serial1}"
    );
    assert!(
        serial1.contains("LBA base 2048"),
        "format must log partition LBA base; serial:\n{serial1}"
    );
    assert!(img.len() > GALFS_PART_BYTE_OFF, "parted image too short");
    assert!(
        img[..GALFS_PART_BYTE_OFF].iter().all(|&b| b == 0),
        "bytes before partition offset must stay zero"
    );
    assert!(
        !img[GALFS_PART_BYTE_OFF..]
            .windows(b"persist-ok-part-offset".len())
            .any(|w| w == b"persist-ok-part-offset"),
        "sealed partition image must not contain plaintext"
    );
    assert_eq!(
        code2,
        Some(QEMU_EXIT_SUCCESS),
        "test-galfs-part verify boot should exit with Success; serial:\n{serial2}"
    );
    assert!(
        serial2.contains("[test-galfs-part] passed"),
        "verify marker missing; serial:\n{serial2}"
    );
    let galf = &img[GALFS_PART_BYTE_OFF..];
    let mut slot_buf = vec![0u8; galexy_galf::DISK_SECTORS * galexy_galf::SECTOR];
    let mut best = Box::new(galexy_galf::Table::empty());
    let mut cand = Box::new(galexy_galf::Table::empty());
    let report = galexy_galf::check_image(
        galf,
        galexy_galf::DEFAULT_VOLUME_PASSPHRASE,
        &mut slot_buf,
        &mut best,
        &mut cand,
    );
    assert!(
        report.ok,
        "host fsck must pass image at partition offset; issues: {:?}",
        &report.issues[..report.issue_count]
    );
}

fn assert_galfs_disk_persists(
    result: (Option<i32>, String, Vec<u8>, Option<i32>, String),
    cache_label: &str,
) {
    let (code1, serial1, img, code2, serial2) = result;
    assert_eq!(
        code1,
        Some(QEMU_EXIT_SUCCESS),
        "test-galfs-disk write boot (cache={cache_label}) should exit with Success; serial:\n{serial1}"
    );
    assert!(
        serial1.contains("[test-galfs-disk] wrote"),
        "write marker missing (cache={cache_label}); serial:\n{serial1}"
    );
    assert!(
        !img.windows(b"persist-ok-block-store".len())
            .any(|w| w == b"persist-ok-block-store"),
        "sealed galfs.img must not contain plaintext file bytes (cache={cache_label})"
    );
    assert_eq!(
        code2,
        Some(QEMU_EXIT_SUCCESS),
        "test-galfs-disk verify boot (cache={cache_label}) should exit with Success; serial:\n{serial2}"
    );
    assert!(
        serial2.contains("[test-galfs-disk] passed"),
        "verify marker missing (cache={cache_label}); serial:\n{serial2}"
    );
    let mut slot_buf = vec![0u8; galexy_galf::DISK_SECTORS * galexy_galf::SECTOR];
    let mut best = Box::new(galexy_galf::Table::empty());
    let mut cand = Box::new(galexy_galf::Table::empty());
    let report = galexy_galf::check_image(
        &img,
        galexy_galf::DEFAULT_VOLUME_PASSPHRASE,
        &mut slot_buf,
        &mut best,
        &mut cand,
    );
    assert!(
        report.ok,
        "host galfs-fsck must pass (cache={cache_label}); issues: {:?}",
        &report.issues[..report.issue_count]
    );
}

#[test]
fn share_disk_persists_across_reboot() {
    let (code1, serial1, img, code2, serial2) = boot_with_galfs(&image("test-share-disk"));
    assert_eq!(
        code1,
        Some(QEMU_EXIT_SUCCESS),
        "test-share-disk write boot should exit with Success; serial:\n{serial1}"
    );
    assert!(
        serial1.contains("[test-share-disk] wrote"),
        "write marker missing; serial:\n{serial1}"
    );
    assert!(
        !img.windows(b"share-disk-marker".len())
            .any(|w| w == b"share-disk-marker"),
        "sealed galfs.img must not contain plaintext share file bytes"
    );
    assert_eq!(
        code2,
        Some(QEMU_EXIT_SUCCESS),
        "test-share-disk verify boot should exit with Success; serial:\n{serial2}"
    );
    assert!(
        serial2.contains("[test-share-disk] passed"),
        "verify marker missing; serial:\n{serial2}"
    );
    let mut slot_buf = vec![0u8; galexy_galf::DISK_SECTORS * galexy_galf::SECTOR];
    let mut best = Box::new(galexy_galf::Table::empty());
    let mut cand = Box::new(galexy_galf::Table::empty());
    let report = galexy_galf::check_image(
        &img,
        galexy_galf::DEFAULT_VOLUME_PASSPHRASE,
        &mut slot_buf,
        &mut best,
        &mut cand,
    );
    assert!(
        report.ok,
        "host galfs-fsck must pass a healthy share image; issues: {:?}",
        &report.issues[..report.issue_count]
    );
    // Sealed payload should carry at least one durable share after boot 1.
    assert!(
        best.shares.iter().any(|s| s.used),
        "host decode must see a used share slot"
    );
}

#[test]
fn galfs_disk_recovers_from_corrupt_slot() {
    let (code1, serial1, _img, code2, serial2) = boot_with_galfs_recover(&image("test-galfs-disk"));
    assert_eq!(
        code1,
        Some(QEMU_EXIT_SUCCESS),
        "write boot should succeed; serial:\n{serial1}"
    );
    assert!(
        serial1.contains("[test-galfs-disk] wrote"),
        "write marker missing; serial:\n{serial1}"
    );
    assert_eq!(
        code2,
        Some(QEMU_EXIT_SUCCESS),
        "recover boot should succeed from older slot; serial:\n{serial2}"
    );
    assert!(
        serial2.contains("[test-galfs-disk] passed"),
        "recover marker missing; serial:\n{serial2}"
    );
    assert!(
        serial2.contains("recovered from bad sibling"),
        "recover boot should log recovery; serial:\n{serial2}"
    );
}

#[test]
fn galfs_idempotent_after_recover() {
    let (code1, serial1, _img, code2, serial2) =
        boot_with_galfs_recover(&image("test-galfs-idempotent"));
    assert_eq!(
        code1,
        Some(QEMU_EXIT_SUCCESS),
        "write boot should succeed; serial:\n{serial1}"
    );
    assert!(
        serial1.contains("[test-galfs-idempotent] wrote"),
        "write marker missing; serial:\n{serial1}"
    );
    assert_eq!(
        code2,
        Some(QEMU_EXIT_SUCCESS),
        "idempotent recover boot should succeed; serial:\n{serial2}"
    );
    assert!(
        serial2.contains("[test-galfs-idempotent] passed"),
        "pass marker missing; serial:\n{serial2}"
    );
    assert!(
        serial2.contains("recovered from bad sibling"),
        "recover boot should log recovery; serial:\n{serial2}"
    );
}

#[test]
fn galfs_disk_recovers_from_torn_write() {
    let (code1, serial1, img, code2, serial2) = boot_with_galfs_torn(&image("test-galfs-disk"));
    assert_eq!(
        code1,
        Some(QEMU_EXIT_SUCCESS),
        "write boot should succeed; serial:\n{serial1}"
    );
    assert!(
        serial1.contains("[test-galfs-disk] wrote"),
        "write marker missing; serial:\n{serial1}"
    );
    // Host: torn newest slot fails; older sibling must still check clean.
    let mut slot_buf = vec![0u8; galexy_galf::DISK_SECTORS * galexy_galf::SECTOR];
    let mut best = Box::new(galexy_galf::Table::empty());
    let mut cand = Box::new(galexy_galf::Table::empty());
    // Tear a copy of the post-write image the same way the harness does.
    let mut torn = img.clone();
    {
        let mut best_gen = 0u64;
        let mut best_off = None;
        for slot in 0..2usize {
            let off = slot * galexy_galf::DISK_SECTORS * galexy_galf::SECTOR;
            if torn.len() < off + 113 || &torn[off..off + 4] != b"GALF" {
                continue;
            }
            let gen = u64::from_le_bytes(torn[off + 16..off + 24].try_into().unwrap());
            if best_off.is_none() || gen >= best_gen {
                best_gen = gen;
                best_off = Some(off);
            }
        }
        let off = best_off.expect("GALF slot");
        let cut = off + galexy_galf::DISK_HEADER + galexy_galf::SECTOR / 2;
        let end = (off + galexy_galf::DISK_SECTORS * galexy_galf::SECTOR).min(torn.len());
        torn[cut..end].fill(0);
    }
    let report = galexy_galf::check_image(
        &torn,
        galexy_galf::DEFAULT_VOLUME_PASSPHRASE,
        &mut slot_buf,
        &mut best,
        &mut cand,
    );
    assert!(
        report.ok,
        "host fsck must accept the older sibling after a torn newest; issues: {:?}",
        &report.issues[..report.issue_count]
    );
    assert_eq!(
        code2,
        Some(QEMU_EXIT_SUCCESS),
        "torn-write recover boot should succeed; serial:\n{serial2}"
    );
    assert!(
        serial2.contains("[test-galfs-disk] passed"),
        "recover marker missing; serial:\n{serial2}"
    );
    assert!(
        serial2.contains("recovered from bad sibling"),
        "torn-write boot should log recovery; serial:\n{serial2}"
    );
}

#[test]
fn galfs_disk_refuses_format_when_both_slots_corrupt() {
    let (code1, serial1, img, code2, serial2, img_after) =
        boot_with_galfs_both_corrupt(&image("test-galfs-disk"), &image("test-galfs-corrupt"));
    assert_eq!(
        code1,
        Some(QEMU_EXIT_SUCCESS),
        "write boot should succeed; serial:\n{serial1}"
    );
    assert_eq!(
        code2,
        Some(QEMU_EXIT_SUCCESS),
        "corrupt boot should succeed; serial:\n{serial2}"
    );
    assert!(
        serial2.contains("[test-galfs-corrupt] passed"),
        "corrupt marker missing; serial:\n{serial2}"
    );
    assert!(
        serial2.contains("refusing silent format"),
        "must refuse format; serial:\n{serial2}"
    );
    assert!(
        !serial2.contains("formatted sealed disk"),
        "must not format over a corrupt volume; serial:\n{serial2}"
    );
    // Host broke only the AEAD tag; refuse-format must leave gens + magic alone.
    for slot in 0..2 {
        let off = slot * 288 * 512;
        assert!(
            img_after.len() >= off + 113 && img.len() >= off + 113,
            "galfs.img too short for slot {slot}"
        );
        assert_eq!(
            &img_after[off..off + 4],
            b"GALF",
            "slot {slot} must keep GALF magic"
        );
        assert_eq!(
            &img_after[off + 16..off + 24],
            &img[off + 16..off + 24],
            "slot {slot} generation must be unchanged (format would rewrite)"
        );
        assert_ne!(
            img_after[off + 112],
            img[off + 112],
            "slot {slot} tag should still be host-corrupted"
        );
    }
    let mut slot_buf = vec![0u8; galexy_galf::DISK_SECTORS * galexy_galf::SECTOR];
    let mut best = Box::new(galexy_galf::Table::empty());
    let mut cand = Box::new(galexy_galf::Table::empty());
    let report = galexy_galf::check_image(
        &img_after,
        galexy_galf::DEFAULT_VOLUME_PASSPHRASE,
        &mut slot_buf,
        &mut best,
        &mut cand,
    );
    assert!(!report.ok);
    assert_eq!(
        report.issues[0],
        galexy_galf::Issue::BothSlotsCorrupt,
        "host fsck should see both-corrupt; issues: {:?}",
        &report.issues[..report.issue_count]
    );
}

#[test]
fn fsck_test_passes() {
    let (code, serial) = boot(&image("test-fsck"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-fsck should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-fsck] passed"),
        "test-fsck success marker missing; serial:\n{serial}"
    );
}

#[test]
fn userheap_test_passes() {
    let (code, serial) = boot(&image("test-userheap"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-userheap should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-userheap] passed"),
        "test-userheap success marker missing; serial:\n{serial}"
    );
}

#[test]
fn channel_test_passes() {
    let (code, serial) = boot(&image("test-channel"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-channel should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-channel] passed"),
        "test-channel success marker missing; serial:\n{serial}"
    );
}

#[test]
fn pipe_test_passes() {
    let (code, serial) = boot(&image("test-pipe"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-pipe should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-pipe] passed"),
        "test-pipe success marker missing; serial:\n{serial}"
    );
}

#[test]
fn proccap_test_passes() {
    let (code, serial) = boot(&image("test-proccap"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-proccap should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-proccap] passed"),
        "test-proccap success marker missing; serial:\n{serial}"
    );
}

#[test]
fn selfcap_test_passes() {
    let (code, serial) = boot(&image("test-selfcap"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-selfcap should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-selfcap] passed"),
        "test-selfcap success marker missing; serial:\n{serial}"
    );
}

#[test]
fn orphan_test_passes() {
    let (code, serial) = boot(&image("test-orphan"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-orphan should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-orphan] passed"),
        "test-orphan success marker missing; serial:\n{serial}"
    );
}

#[test]
fn init_test_passes() {
    let (code, serial) = boot(&image("test-init"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-init should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-init] passed"),
        "test-init success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[init] backoff name=probe ms=250"),
        "init did not back off the probe storm; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-init] logged-out init rpc denied"),
        "logged-out init rpc was not denied; serial:\n{serial}"
    );
}

const SVC_STATUS_KEYS: &[(&str, &str)] = &[
    ("s", "s"),
    ("v", "v"),
    ("c", "c"),
    ("spc", " "),
    ("s", "s"),
    ("t", "t"),
    ("a", "a"),
    ("t", "t"),
    ("u", "u"),
    ("s", "s"),
    ("spc", " "),
    ("s", "s"),
    ("h", "h"),
    ("e", "e"),
    ("l", "l"),
    ("l", "l"),
    ("ret", "shell running"),
];

#[test]
fn shell_svc_status_typing_e2e() {
    let keys = with_login(SVC_STATUS_KEYS);
    let serial = boot_and_type(
        &image("galexy-os"),
        &keys,
        "[boot] main loop ready",
        "shell running",
        Duration::from_millis(30),
        Duration::from_secs(180),
    );
    assert!(
        serial.contains("shell running"),
        "svc status shell did not report the seat; serial:\n{serial}"
    );
}

const SHUTDOWN_KEYS: &[(&str, &str)] = &[
    ("s", "s"),
    ("h", "h"),
    ("u", "u"),
    ("t", "t"),
    ("d", "d"),
    ("o", "o"),
    ("w", "w"),
    ("n", "n"),
    ("ret", "asking init"),
];

#[test]
fn shell_shutdown_reaches_init() {
    let keys = with_login(SHUTDOWN_KEYS);
    let serial = boot_and_type(
        &image("galexy-os"),
        &keys,
        "[boot] main loop ready",
        "[init] power",
        Duration::from_millis(30),
        Duration::from_secs(180),
    );
    assert!(
        serial.contains("[init] shutdown"),
        "shutdown never reached init; serial:\n{serial}"
    );
    assert!(
        serial.contains("[init] power"),
        "init did not reach Power after killing seats; serial:\n{serial}"
    );
}

#[test]
fn jobcap_test_passes() {
    let (code, serial) = boot(&image("test-jobcap"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-jobcap should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-jobcap] passed"),
        "test-jobcap success marker missing; serial:\n{serial}"
    );
}

#[test]
fn procgive_test_passes() {
    let (code, serial) = boot(&image("test-procgive"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-procgive should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-procgive] passed"),
        "test-procgive success marker missing; serial:\n{serial}"
    );
}

#[test]
fn procbudget_test_passes() {
    let (code, serial) = boot(&image("test-procbudget"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-procbudget should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-procbudget] passed"),
        "test-procbudget success marker missing; serial:\n{serial}"
    );
}

#[test]
fn capforge_test_passes() {
    let (code, serial) = boot(&image("test-capforge"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-capforge should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-capforge] passed"),
        "test-capforge success marker missing; serial:\n{serial}"
    );
}

#[test]
fn seek_test_passes() {
    let (code, serial) = boot(&image("test-seek"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-seek should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-seek] passed"),
        "test-seek success marker missing; serial:\n{serial}"
    );
}

#[test]
fn users_test_passes() {
    let (code, serial) = boot(&image("test-users"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-users should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-users] passed"),
        "test-users success marker missing; serial:\n{serial}"
    );
}

#[test]
fn mustchange_test_passes() {
    let (code, serial) = boot(&image("test-mustchange"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-mustchange should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[auth] session login user=admin"),
        "session generation line missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-mustchange] passed"),
        "test-mustchange success marker missing; serial:\n{serial}"
    );
}

#[test]
fn crash_injection_picks_consistent_slot() {
    let (killed, code, serial) = boot_with_galfs_crash(&image("test-crash"));
    assert!(
        killed.contains("[test-crash] mutating"),
        "guest must announce the in-flight mutate; serial:\n{killed}"
    );
    assert!(
        !killed.contains("[test-crash] committed-drop"),
        "QEMU must die before the in-flight commit returns; serial:\n{killed}"
    );
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "next boot should exit with Success; serial:\n{serial}"
    );
    assert!(
        !serial.contains("disk corrupt"),
        "crash must not look like a corrupt volume; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-crash] passed"),
        "consistent slot marker missing; serial:\n{serial}"
    );
}

#[test]
fn ata_absent_returns_unsupported() {
    let (code, serial) = boot(&image("test-ata"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-ata should exit with Success; serial:\n{serial}"
    );
    assert!(
        !serial.contains("[PANIC]"),
        "missing ATA slave must not panic; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-ata] passed"),
        "test-ata success marker missing; serial:\n{serial}"
    );
}

#[test]
fn unlock_test_passes() {
    let (code, serial) = boot_galfs_once(&image("test-unlock"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-unlock should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[galfs] unlock failed; RAM-only"),
        "wrong passphrase must stay RAM-only; serial:\n{serial}"
    );
    assert!(
        serial.contains("[galfs] volume key wiped"),
        "key wipe missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[galfs] unlocked slot"),
        "correct passphrase must remount; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-unlock] passed"),
        "test-unlock success marker missing; serial:\n{serial}"
    );
}

#[test]
fn idle_test_passes() {
    let (code, serial) = boot(&image("test-idle"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-idle should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[auth] idle logout user=idle1 tty=1"),
        "idle logout line missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-idle] passed"),
        "test-idle success marker missing; serial:\n{serial}"
    );
}

#[test]
fn lockout_test_passes() {
    let (code, serial) = boot(&image("test-lockout"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-lockout should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[auth] lockout user=eve"),
        "actor lockout line missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[auth] lockout tty=1"),
        "tty lockout line missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[auth] login refused user=eve tty=1 locked"),
        "refused line missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("fails=5"),
        "failure count missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-lockout] passed"),
        "test-lockout success marker missing; serial:\n{serial}"
    );
}

#[test]
fn scratch_test_passes() {
    let (code, serial) = boot(&image("test-scratch"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-scratch should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-scratch] passed"),
        "test-scratch success marker missing; serial:\n{serial}"
    );
}

#[test]
fn open_test_passes() {
    let (code, serial) = boot(&image("test-open"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-open should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-open] passed"),
        "test-open success marker missing; serial:\n{serial}"
    );
}

#[test]
fn ramdisk_test_passes() {
    let (code, serial) = boot(&image("test-ramdisk"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-ramdisk should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-ramdisk] passed"),
        "test-ramdisk success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("tar entry: 'banner.txt'"),
        "ramdisk entry marker missing; serial:\n{serial}"
    );
    // Measurement: the kernel's boot-time SHA-256 of the archive equals the
    // digest build.rs printed when it packed the tar (Milestone 51 repro
    // note). A mismatch means the image does not carry the ramdisk you
    // built.
    let built = env!("GALEXY_RAMDISK_SHA256");
    assert!(
        serial.contains(&format!("[test-ramdisk] sha256 {built} (")),
        "ramdisk digest mismatch: build.rs measured {built}; serial:\n{serial}"
    );
}

#[test]
fn realprogram_test_passes() {
    let (code, serial) = boot(&image("test-realprogram"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-realprogram should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-realprogram] passed"),
        "test-realprogram success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[loader] program 'hello' ready"),
        "loader spawn marker missing; serial:\n{serial}"
    );
    // The write syscall must actually SUCCEED: hello's text reaches COM1
    // through the console mirror (guards the active-tree buffer walk —
    // a kernel-tree walk reports BadBuffer for every user buffer).
    assert!(
        serial.contains(HELLO_TEXT),
        "hello's console output missing from serial (write syscall failed?); serial:\n{serial}"
    );
}

/// Milestone 56: `sleep` parks until a monotonic deadline (`nap` util).
#[test]
fn sleep_test_passes() {
    let (code, serial) = boot(&image("test-sleep"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-sleep should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-sleep] passed"),
        "test-sleep success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("napping"),
        "nap never printed start; serial:\n{serial}"
    );
    assert!(
        serial.contains("awake"),
        "nap never woke from sleep; serial:\n{serial}"
    );
}

/// Milestone 61: ELF from host `gxc` (gxr subset) runs under the loader.
#[test]
fn hellogxc_test_passes() {
    let (code, serial) = boot(&image("test-hellogxc"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-hellogxc should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-hellogxc] passed"),
        "test-hellogxc success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[loader] program 'hello-gxc' ready"),
        "loader spawn marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains(HELLO_GXC_TEXT),
        "gxc hello console output missing from serial; serial:\n{serial}"
    );
}

#[test]
fn runshell_test_passes() {
    let (code, serial) = boot(&image("test-runshell"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-runshell should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-runshell] passed"),
        "test-runshell success marker missing; serial:\n{serial}"
    );
    // hello's output reached COM1 through the console mirror (exactly the
    // text the real program writes).
    assert!(
        serial.contains("Hello from a real Rust user program!"),
        "user program console output missing from serial:\n{serial}"
    );
    assert!(
        serial.contains("exited (syscall)"),
        "user task exit marker missing; serial:\n{serial}"
    );
}

/// The exact text the real hello program prints through the console
/// syscall (mirrored to COM1 by the kernel).
const HELLO_TEXT: &str = "Hello from a real Rust user program!";

/// Text from `crates/gxc/examples/hello.gxr` (Milestone 61).
const HELLO_GXC_TEXT: &str = "Hello from gxc!";

/// Login screen: username then masked password (`*` echo).
const LOGIN_ADMIN_KEYS: &[(&str, &str)] = &[
    ("a", "a"),
    ("d", "d"),
    ("m", "m"),
    ("i", "i"),
    ("n", "n"),
    ("ret", "Password: "),
    ("a", "*"),
    ("d", "*"),
    ("m", "*"),
    ("i", "*"),
    ("n", "*"),
    ("ret", "admin@galexy> "),
];

/// After boot login with the format default, change it so other commands work.
/// `passwd` always prompts: masked Password: then Confirm:.
const CLEAR_DEFAULT_PASSWD: &[(&str, &str)] = &[
    ("p", "p"),
    ("a", "a"),
    ("s", "s"),
    ("s", "s"),
    ("w", "w"),
    ("d", "d"),
    ("ret", "Password: "),
    ("t", "*"),
    ("e", "*"),
    ("s", "*"),
    ("t", "*"),
    ("p", "*"),
    ("a", "*"),
    ("s", "*"),
    ("s", "*"),
    ("ret", "Confirm: "),
    ("t", "*"),
    ("e", "*"),
    ("s", "*"),
    ("t", "*"),
    ("p", "*"),
    ("a", "*"),
    ("s", "*"),
    ("s", "*"),
    ("ret", "admin@galexy> "),
];

/// Login screen using the post-`passwd` password (`testpass`).
const LOGIN_ADMIN_TESTPASS_KEYS: &[(&str, &str)] = &[
    ("a", "a"),
    ("d", "d"),
    ("m", "m"),
    ("i", "i"),
    ("n", "n"),
    ("ret", "Password: "),
    ("t", "*"),
    ("e", "*"),
    ("s", "*"),
    ("t", "*"),
    ("p", "*"),
    ("a", "*"),
    ("s", "*"),
    ("s", "*"),
    ("ret", "admin@galexy> "),
];

fn with_login<'a>(keys: &'a [(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    LOGIN_ADMIN_KEYS
        .iter()
        .chain(CLEAR_DEFAULT_PASSWD.iter())
        .chain(keys.iter())
        .copied()
        .collect()
}

/// After each `Password:` / `Confirm:`, console echo must be `*` only.
/// Kernel log lines (`Ns: …`) may interleave on COM1 and are ignored.
fn assert_passwords_masked(serial: &str) {
    for marker in ["Password: ", "Confirm: "] {
        let mut from = 0;
        while let Some(rel) = serial[from..].find(marker) {
            let start = from + rel + marker.len();
            let rest = &serial[start..];
            let mut saw_star = false;
            for line in rest.split('\n') {
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                if line.starts_with('[') || looks_like_uptime_log(line) {
                    continue;
                }
                if line.contains("galexy>")
                    || line.starts_with("Login")
                    || line.starts_with("Password:")
                    || line.starts_with("Confirm:")
                    || line.starts_with("passwd:")
                    || line.starts_with("password too long")
                {
                    break;
                }
                for c in line.chars() {
                    if c == '*' {
                        saw_star = true;
                    } else if c == '[' || c.is_ascii_digit() {
                        // Same-line kernel log after stars: `*3s: [sched] …`.
                        break;
                    } else {
                        panic!(
                            "password cleartext leaked to serial after {marker}({line:?}); serial:\n{serial}"
                        );
                    }
                }
            }
            assert!(
                saw_star,
                "{marker}prompt had no masked echo; serial:\n{serial}"
            );
            from = start;
        }
    }
}

fn looks_like_uptime_log(line: &str) -> bool {
    let b = line.as_bytes();
    let mut i = 0;
    if i >= b.len() || !b[i].is_ascii_digit() {
        return false;
    }
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    b[i..].starts_with(b"s: ")
}

/// After boot login + passwd, CLI `login admin` with masked `testpass`.
const LOGIN_CLI_PROMPT_KEYS: &[(&str, &str)] = &[
    ("l", "l"),
    ("o", "o"),
    ("g", "g"),
    ("i", "i"),
    ("n", "n"),
    ("spc", " "),
    ("a", "a"),
    ("d", "d"),
    ("m", "m"),
    ("i", "i"),
    ("n", "n"),
    ("ret", "Password: "),
    ("t", "*"),
    ("e", "*"),
    ("s", "*"),
    ("t", "*"),
    ("p", "*"),
    ("a", "*"),
    ("s", "*"),
    ("s", "*"),
    ("ret", "admin@galexy> "),
];

/// Qcode + expected-echo pairs for typing `hello` + Enter (typing
/// E2E). Each key syncs on the shell's echo of it (console = screen +
/// serial); the Enter key syncs on hello's program output (the write
/// syscall's console mirror) — proof the whole dispatch ran.
const RUN_HELLO_KEYS: &[(&str, &str)] = &[
    ("h", "h"),
    ("e", "e"),
    ("l", "l"),
    ("l", "l"),
    ("o", "o"),
    ("ret", HELLO_TEXT),
];

/// `echo hi`, then `echo $?` — Cap-wait status should be 0.
const STATUS_AFTER_LOGIN: &[(&str, &str)] = &[
    ("e", "e"),
    ("c", "c"),
    ("h", "h"),
    ("o", "o"),
    ("spc", " "),
    ("h", "h"),
    ("i", "i"),
    ("ret", "hi\n"),
    ("e", "e"),
    ("c", "c"),
    ("h", "h"),
    ("o", "o"),
    ("spc", " "),
    ("shift+4", "$"),
    ("shift+slash", "?"),
    ("ret", "0\n"),
];

/// `hello`, then `linger`, then `crash`. The prompt returns while `linger`
/// is still running. `crash` kills the shell; login again, then `x` is
/// echoed by the new shell, and `beat` is still arriving.
const SUPERVISOR_AFTER_LOGIN: &[(&str, &str)] = &[
    ("h", "h"),
    ("e", "e"),
    ("l", "l"),
    ("l", "l"),
    ("o", "o"),
    ("ret", HELLO_TEXT),
    ("l", "l"),
    ("i", "i"),
    ("n", "n"),
    ("g", "g"),
    ("e", "e"),
    ("r", "r"),
    ("ret", "up\n"),
    ("c", "c"),
    ("r", "r"),
    ("a", "a"),
    ("s", "s"),
    ("h", "h"),
    ("ret", "killing the task"),
];

/// F2 selects the second shell. Login there, `echo hi`, then F1 and a key
/// on the first shell (still logged out until login — only echoes `z`).
const TTY_AFTER_F2: &[(&str, &str)] = &[("f2", "[tty] 2")];

const TTY_AFTER_LOGIN: &[(&str, &str)] = &[
    ("e", "e"),
    ("c", "c"),
    ("h", "h"),
    ("o", "o"),
    ("spc", " "),
    ("h", "h"),
    ("i", "i"),
    ("ret", "hi"),
    ("f1", "[tty] 1"),
    ("z", "z"),
];

/// `stats`, then `threads`, then `tasks`. Each Enter syncs on a line only
/// the query `read` produces (the banner's "frames free" is screen-only).
const QUERY_AFTER_LOGIN: &[(&str, &str)] = &[
    ("s", "s"),
    ("t", "t"),
    ("a", "a"),
    ("t", "t"),
    ("s", "s"),
    ("ret", "frames free:"),
    ("t", "t"),
    ("h", "h"),
    ("r", "r"),
    ("e", "e"),
    ("a", "a"),
    ("d", "d"),
    ("s", "s"),
    ("ret", "main loop:"),
    ("t", "t"),
    ("a", "a"),
    ("s", "s"),
    ("k", "k"),
    ("s", "s"),
    ("ret", "cooperative tasks:"),
    ("l", "l"),
    ("s", "s"),
    ("ret", "banner.txt"),
];

/// `cat`, redirection, `mkdir` / `cd` / `ls`. Enter syncs on text that
/// appears only after the command runs. Utilities Cap-wait, so a
/// redirected `echo`, `mkdir`, and a successful `rm` finish before the
/// prompt returns. Sync on the prompt (or command output) — not on
/// `[sched] …` lines, which `typing_visible` strips as uptime logs.
const UTIL_KEYS: &[(&str, &str)] = &[
    ("c", "c"),
    ("a", "a"),
    ("t", "t"),
    ("spc", " "),
    ("b", "b"),
    ("a", "a"),
    ("n", "n"),
    ("n", "n"),
    ("e", "e"),
    ("r", "r"),
    ("dot", "."),
    ("t", "t"),
    ("x", "x"),
    ("t", "t"),
    ("ret", "plumbing works"),
    ("e", "e"),
    ("c", "c"),
    ("h", "h"),
    ("o", "o"),
    ("spc", " "),
    ("s", "s"),
    ("c", "c"),
    ("r", "r"),
    ("a", "a"),
    ("t", "t"),
    ("c", "c"),
    ("h", "h"),
    ("minus", "-"),
    ("h", "h"),
    ("i", "i"),
    ("spc", " "),
    ("shift+dot", ">"),
    ("spc", " "),
    ("n", "n"),
    ("o", "o"),
    ("t", "t"),
    ("e", "e"),
    ("ret", "admin@galexy> "),
    ("c", "c"),
    ("a", "a"),
    ("t", "t"),
    ("spc", " "),
    ("n", "n"),
    ("o", "o"),
    ("t", "t"),
    ("e", "e"),
    ("ret", "scratch-hi\n"),
    ("e", "e"),
    ("c", "c"),
    ("h", "h"),
    ("o", "o"),
    ("spc", " "),
    ("m", "m"),
    ("o", "o"),
    ("r", "r"),
    ("e", "e"),
    ("spc", " "),
    ("shift+dot", ">"),
    ("shift+dot", ">"),
    ("spc", " "),
    ("n", "n"),
    ("o", "o"),
    ("t", "t"),
    ("e", "e"),
    ("ret", "admin@galexy> "),
    ("c", "c"),
    ("a", "a"),
    ("t", "t"),
    ("spc", " "),
    ("n", "n"),
    ("o", "o"),
    ("t", "t"),
    ("e", "e"),
    ("ret", "more\n"),
    ("m", "m"),
    ("k", "k"),
    ("d", "d"),
    ("i", "i"),
    ("r", "r"),
    ("spc", " "),
    ("b", "b"),
    ("o", "o"),
    ("x", "x"),
    ("ret", "admin@galexy> "),
    ("c", "c"),
    ("d", "d"),
    ("spc", " "),
    ("b", "b"),
    ("o", "o"),
    ("x", "x"),
    ("ret", "admin@galexy:/box> "),
    ("e", "e"),
    ("c", "c"),
    ("h", "h"),
    ("o", "o"),
    ("spc", " "),
    ("i", "i"),
    ("n", "n"),
    ("b", "b"),
    ("o", "o"),
    ("x", "x"),
    ("spc", " "),
    ("shift+dot", ">"),
    ("spc", " "),
    ("l", "l"),
    ("e", "e"),
    ("a", "a"),
    ("f", "f"),
    ("ret", "admin@galexy:/box> "),
    ("l", "l"),
    ("s", "s"),
    ("ret", "leaf\n"),
    ("c", "c"),
    ("a", "a"),
    ("t", "t"),
    ("spc", " "),
    ("l", "l"),
    ("e", "e"),
    ("a", "a"),
    ("f", "f"),
    ("ret", "inbox\n"),
    ("c", "c"),
    ("d", "d"),
    ("spc", " "),
    ("dot", "."),
    ("dot", "."),
    ("ret", "admin@galexy> "),
    ("l", "l"),
    ("s", "s"),
    ("ret", "box/"),
    ("c", "c"),
    ("a", "a"),
    ("t", "t"),
    ("spc", " "),
    ("h", "h"),
    ("e", "e"),
    ("l", "l"),
    ("l", "l"),
    ("o", "o"),
    ("ret", "cat: not text"),
    ("e", "e"),
    ("c", "c"),
    ("h", "h"),
    ("o", "o"),
    ("spc", " "),
    ("n", "n"),
    ("o", "o"),
    ("spc", " "),
    ("shift+dot", ">"),
    ("spc", " "),
    ("b", "b"),
    ("a", "a"),
    ("n", "n"),
    ("n", "n"),
    ("e", "e"),
    ("r", "r"),
    ("dot", "."),
    ("t", "t"),
    ("x", "x"),
    ("t", "t"),
    ("ret", "echo: cannot replace"),
    ("r", "r"),
    ("m", "m"),
    ("spc", " "),
    ("n", "n"),
    ("o", "o"),
    ("t", "t"),
    ("e", "e"),
    ("ret", "admin@galexy> "),
    ("c", "c"),
    ("a", "a"),
    ("t", "t"),
    ("spc", " "),
    ("n", "n"),
    ("o", "o"),
    ("t", "t"),
    ("e", "e"),
    ("ret", "cat: no such file"),
    ("r", "r"),
    ("m", "m"),
    ("spc", " "),
    ("b", "b"),
    ("o", "o"),
    ("x", "x"),
    ("ret", "rm: directory not empty"),
    ("c", "c"),
    ("d", "d"),
    ("spc", " "),
    ("b", "b"),
    ("o", "o"),
    ("x", "x"),
    ("ret", "admin@galexy:/box> "),
    ("r", "r"),
    ("m", "m"),
    ("spc", " "),
    ("l", "l"),
    ("e", "e"),
    ("a", "a"),
    ("f", "f"),
    ("ret", "admin@galexy:/box> "),
    ("c", "c"),
    ("d", "d"),
    ("spc", " "),
    ("dot", "."),
    ("dot", "."),
    ("ret", "admin@galexy> "),
    ("r", "r"),
    ("m", "m"),
    ("spc", " "),
    ("b", "b"),
    ("o", "o"),
    ("x", "x"),
    ("ret", "admin@galexy> "),
];

/// Typing `shell` must not start a nested console (reserved name).
const SHELL_RESERVE_KEYS: &[(&str, &str)] = &[
    ("s", "s"),
    ("h", "h"),
    ("e", "e"),
    ("l", "l"),
    ("l", "l"),
    ("ret", "shell: reserved"),
];

/// Default admin/admin must `passwd` before other commands.
#[test]
fn shell_must_change_typing_e2e() {
    let mut keys = LOGIN_ADMIN_KEYS.to_vec();
    keys.extend([
        ("t", "t"),
        ("o", "o"),
        ("u", "u"),
        ("c", "c"),
        ("h", "h"),
        ("spc", " "),
        ("x", "x"),
        ("ret", "passwd: change the default password first"),
    ]);
    keys.extend(CLEAR_DEFAULT_PASSWD.iter().copied());
    keys.extend([
        ("t", "t"),
        ("o", "o"),
        ("u", "u"),
        ("c", "c"),
        ("h", "h"),
        ("spc", " "),
        ("x", "x"),
        ("ret", "admin@galexy> "),
    ]);
    let serial = boot_and_type(
        &image("galexy-os"),
        &keys,
        "[boot] main loop ready",
        "admin@galexy> ",
        Duration::from_millis(30),
        Duration::from_secs(120),
    );
    assert!(
        serial.contains("passwd: change the default password first"),
        "must-change did not block touch; serial:\n{serial}"
    );
    assert_passwords_masked(&serial);
}

/// Pathological input at the password prompt: a paste longer than the
/// 64-byte password buffer is refused with `password too long`, nothing
/// but `*` reaches the console, the seat returns to `Login as:`, and a
/// normal login still works afterwards.
#[test]
fn shell_password_paste_typing_e2e() {
    let mut keys: Vec<(&str, &str)> = vec![
        ("a", "a"),
        ("d", "d"),
        ("m", "m"),
        ("i", "i"),
        ("n", "n"),
        ("ret", "Password: "),
    ];
    // 64 keys fill the buffer (each echoes a star); the 65th overflows.
    keys.extend(core::iter::repeat_n(("x", "*"), 64));
    keys.push(("x", "password too long"));
    keys.push(("a", "a"));
    keys.extend(LOGIN_ADMIN_KEYS.iter().skip(1).copied());
    let serial = boot_and_type(
        &image("galexy-os"),
        &keys,
        "[boot] main loop ready",
        "admin@galexy> ",
        Duration::from_millis(30),
        Duration::from_secs(150),
    );
    assert!(
        serial.contains("password too long"),
        "overlong paste was not refused; serial:\n{serial}"
    );
    assert_passwords_masked(&serial);
}

/// Headless COM1 is a console, not just a log: bytes written to the UART
/// log in (DEL is backspace, CR is Enter) and the password stays masked.
#[test]
fn uart_console_login_e2e() {
    let serial = uart_login_serial(&image("galexy-os"));
    assert!(
        serial.contains("[ioapic] serial: isa irq 4"),
        "COM1 was not routed; serial:\n{serial}"
    );
    assert!(
        serial.contains("passwd: change the default password"),
        "UART login never reached the default-password prompt; serial:\n{serial}"
    );
    assert_passwords_masked(&serial);
}

/// Boot login screen + CLI `login admin` both mask the password on COM1.
#[test]
fn shell_secret_prompt_typing_e2e() {
    let keys = with_login(LOGIN_CLI_PROMPT_KEYS);
    let serial = boot_and_type(
        &image("galexy-os"),
        &keys,
        "[boot] main loop ready",
        "admin@galexy> ",
        Duration::from_millis(30),
        Duration::from_secs(120),
    );
    assert_passwords_masked(&serial);
    let prompts = serial.matches("Password: ").count();
    assert!(
        prompts >= 2,
        "expected boot + CLI password prompts; serial:\n{serial}"
    );
}

/// The ring-3 shell's query caps: typed `stats` / `threads` / `tasks` /
/// `ls` come back as console text (screen + serial), including a live
/// thread and the ramdisk's `banner.txt`.
#[test]
fn shell_query_typing_e2e() {
    let keys = with_login(QUERY_AFTER_LOGIN);
    let serial = boot_and_type(
        &image("galexy-os"),
        &keys,
        "[boot] main loop ready",
        "",
        Duration::from_millis(30),
        Duration::from_secs(90),
    );
    assert_passwords_masked(&serial);
    assert!(
        serial.contains("uptime:"),
        "typed `stats` never produced the uptime line; serial:\n{serial}"
    );
    assert!(
        serial.contains("frames free:"),
        "typed `stats` never produced the frame line; serial:\n{serial}"
    );
    assert!(
        serial.contains("galfs:") && serial.contains("blocks"),
        "typed `stats` never produced the galfs line; serial:\n{serial}"
    );
    assert!(
        serial.contains("thread-a:"),
        "typed `threads` never listed thread-a; serial:\n{serial}"
    );
    assert!(
        serial.contains("cooperative tasks:"),
        "typed `tasks` never produced the task line; serial:\n{serial}"
    );
    assert!(
        serial.contains("id=") && serial.contains("name=shell"),
        "typed `tasks` never listed a process Cap label; serial:\n{serial}"
    );
    assert!(
        serial.contains("banner.txt"),
        "typed `ls` never listed banner.txt; serial:\n{serial}"
    );
    assert!(
        serial.contains("hello\n"),
        "typed `ls` never listed hello; serial:\n{serial}"
    );
}

/// Typing `shell` is refused: F1–F12 own those task names.
#[test]
fn shell_nested_spawn_refused_e2e() {
    let keys = with_login(SHELL_RESERVE_KEYS);
    let serial = boot_and_type(
        &image("galexy-os"),
        &keys,
        "[boot] main loop ready",
        "shell: reserved",
        Duration::from_millis(30),
        Duration::from_secs(60),
    );
    assert!(
        serial.contains("shell: reserved"),
        "typed `shell` was not refused; serial:\n{serial}"
    );
    assert!(
        !serial.contains("keyboard denied"),
        "typed `shell` started a keyboard-less shell; serial:\n{serial}"
    );
}

/// Default-build audit (Milestone 52): the main image's shell has no
/// `crash` command — that seam lives only in the `galexy-os-crashseam`
/// ramdisk. Typing it is `command not found` and the seat stays up.
#[test]
fn default_image_has_no_crash_seam_e2e() {
    let keys = with_login(&[
        ("c", "c"),
        ("r", "r"),
        ("a", "a"),
        ("s", "s"),
        ("h", "h"),
        ("ret", "crash: command not found"),
    ]);
    let serial = boot_and_type(
        &image("galexy-os"),
        &keys,
        "[boot] main loop ready",
        "admin@galexy> ",
        Duration::from_millis(30),
        Duration::from_secs(60),
    );
    assert!(
        serial.contains("crash: command not found"),
        "default shell accepted `crash`; serial:\n{serial}"
    );
    assert!(
        !serial.contains("task 'shell' exited (page fault)"),
        "default shell faulted on `crash`; serial:\n{serial}"
    );
}

/// `mkdir` / `cd`, a failed `cd` that keeps the prompt, `echo *`, and
/// `echo quiz | cat`.
const PIPE_GLOB_KEYS: &[(&str, &str)] = &[
    ("m", "m"),
    ("k", "k"),
    ("d", "d"),
    ("i", "i"),
    ("r", "r"),
    ("spc", " "),
    ("b", "b"),
    ("o", "o"),
    ("x", "x"),
    ("ret", "admin@galexy> "),
    ("c", "c"),
    ("d", "d"),
    ("spc", " "),
    ("b", "b"),
    ("o", "o"),
    ("x", "x"),
    ("ret", "admin@galexy:/box> "),
    ("c", "c"),
    ("d", "d"),
    ("spc", " "),
    ("n", "n"),
    ("o", "o"),
    ("p", "p"),
    ("e", "e"),
    ("ret", "cd: no such directory"),
    ("t", "t"),
    ("o", "o"),
    ("u", "u"),
    ("c", "c"),
    ("h", "h"),
    ("spc", " "),
    ("a", "a"),
    ("a", "a"),
    ("ret", "admin@galexy:/box> "),
    ("t", "t"),
    ("o", "o"),
    ("u", "u"),
    ("c", "c"),
    ("h", "h"),
    ("spc", " "),
    ("b", "b"),
    ("b", "b"),
    ("ret", "admin@galexy:/box> "),
    ("e", "e"),
    ("c", "c"),
    ("h", "h"),
    ("o", "o"),
    ("spc", " "),
    ("shift+8", "*"),
    ("ret", "aa bb\n"),
    ("e", "e"),
    ("c", "c"),
    ("h", "h"),
    ("o", "o"),
    ("spc", " "),
    ("q", "q"),
    ("u", "u"),
    ("i", "i"),
    ("z", "z"),
    ("spc", " "),
    ("shift+backslash", "|"),
    ("spc", " "),
    ("c", "c"),
    ("a", "a"),
    ("t", "t"),
    ("ret", "quiz\n"),
];

/// Pipelines, `uptime`, `ls -l`, `history`, and a background job.
const USERLAND_KEYS: &[(&str, &str)] = &[
    ("b", "b"),
    ("a", "a"),
    ("n", "n"),
    ("tab", "banner.txt "),
    ("ret", "banner.txt: reserved"),
    ("e", "e"),
    ("c", "c"),
    ("h", "h"),
    ("o", "o"),
    ("spc", " "),
    ("z", "z"),
    ("e", "e"),
    ("d", "d"),
    ("minus", "-"),
    ("6", "6"),
    ("6", "6"),
    ("spc", " "),
    ("shift+backslash", "|"),
    ("spc", " "),
    ("g", "g"),
    ("r", "r"),
    ("e", "e"),
    ("p", "p"),
    ("spc", " "),
    ("z", "z"),
    ("e", "e"),
    ("d", "d"),
    ("spc", " "),
    ("shift+backslash", "|"),
    ("spc", " "),
    ("h", "h"),
    ("e", "e"),
    ("a", "a"),
    ("d", "d"),
    // `zed-66` only. A kernel `Ns:` line can land between the payload
    // and its newline, so the sync must not require `\n`.
    ("ret", "zed-66"),
    ("u", "u"),
    ("p", "p"),
    ("t", "t"),
    ("i", "i"),
    ("m", "m"),
    ("e", "e"),
    ("ret", "uptime:"),
    ("l", "l"),
    ("s", "s"),
    ("spc", " "),
    ("minus", "-"),
    ("l", "l"),
    ("ret", "banner.txt"),
    ("e", "e"),
    ("c", "c"),
    ("h", "h"),
    ("o", "o"),
    ("spc", " "),
    ("n", "n"),
    ("o", "o"),
    ("t", "t"),
    ("e", "e"),
    ("6", "6"),
    ("6", "6"),
    ("ret", "note66\n"),
    ("h", "h"),
    ("i", "i"),
    ("s", "s"),
    ("t", "t"),
    ("o", "o"),
    ("r", "r"),
    ("y", "y"),
    ("ret", "note66"),
    ("e", "e"),
    ("c", "c"),
    ("h", "h"),
    ("o", "o"),
    ("spc", " "),
    ("h", "h"),
    ("i", "i"),
    ("spc", " "),
    ("shift+7", "&"),
    ("ret", "admin@galexy> "),
    ("j", "j"),
    ("o", "o"),
    ("b", "b"),
    ("s", "s"),
    ("ret", "[1] echo"),
];

#[test]
fn shell_userland_typing_e2e() {
    let keys = with_login(USERLAND_KEYS);
    let serial = boot_and_type(
        &image("galexy-os"),
        &keys,
        "[boot] main loop ready",
        "[1] echo",
        Duration::from_millis(30),
        Duration::from_secs(180),
    );
    assert!(
        serial.matches("zed-66").count() >= 2,
        "pipeline did not print zed-66; serial:\n{serial}"
    );
    assert!(
        serial.contains("uptime:"),
        "uptime did not print; serial:\n{serial}"
    );
    assert!(
        serial.contains("[1] echo"),
        "jobs did not list the background echo; serial:\n{serial}"
    );
}

#[test]
fn shell_pipeline_glob_typing_e2e() {
    let keys = with_login(PIPE_GLOB_KEYS);
    let serial = boot_and_type(
        &image("galexy-os"),
        &keys,
        "[boot] main loop ready",
        "quiz\n",
        Duration::from_millis(30),
        Duration::from_secs(180),
    );
    let missed = serial
        .find("cd: no such directory")
        .unwrap_or_else(|| panic!("failed cd did not report; serial:\n{serial}"));
    assert!(
        serial[missed..].contains("admin@galexy:/box> "),
        "failed cd changed the prompt; serial:\n{serial}"
    );
    assert!(
        serial.contains("aa bb\n"),
        "glob did not expand echo *; serial:\n{serial}"
    );
    assert!(
        serial.contains("quiz\n"),
        "echo | cat did not print the payload; serial:\n{serial}"
    );
}

/// `cat banner.txt`, a scratch file written with `echo`, and `mkdir` /
/// `cd` / `ls`. `hello` is not console text. A tar name cannot be replaced.
/// `rm` drops a scratch file and refuses a directory that still has a child.
#[test]
fn shell_util_typing_e2e() {
    util_typing_e2e_on("galexy-os");
}

/// `nano note`: type two lines, move with the arrows, save, leave, reopen,
/// append `!`, save again, then `cat` the file. The status marker
/// `@line,col@` is 1-based and is reprinted on every redraw, so each key
/// syncs on the marker from that redraw.
#[test]
fn shell_nano_typing_e2e() {
    nano_typing_e2e_on("galexy-os");
}

/// Same keystrokes on the `gxld`-linked ramdisk.
#[test]
fn gxld_image_nano_typing_e2e() {
    nano_typing_e2e_on("galexy-os-gxld");
}

const NANO_KEYS: &[(&str, &str)] = &[
    ("n", "n"),
    ("a", "a"),
    ("n", "n"),
    ("o", "o"),
    ("spc", " "),
    ("n", "n"),
    ("o", "o"),
    ("t", "t"),
    ("e", "e"),
    ("ret", "@1,1@"),
    ("h", "@1,2@"),
    ("i", "@1,3@"),
    ("left", "@1,2@"),
    ("right", "@1,3@"),
    ("ret", "@2,1@"),
    ("y", "@2,2@"),
    ("o", "@2,3@"),
    ("ctrl+o", "Wrote 5"),
    ("ctrl+x", "admin@galexy> "),
    ("n", "n"),
    ("a", "a"),
    ("n", "n"),
    ("o", "o"),
    ("spc", " "),
    ("n", "n"),
    ("o", "o"),
    ("t", "t"),
    ("e", "e"),
    ("ret", "@1,1@"),
    ("ctrl+e", "@1,3@"),
    ("shift+1", "@1,4@"),
    ("ctrl+o", "Wrote 6"),
    ("ctrl+x", "admin@galexy> "),
    ("c", "c"),
    ("a", "a"),
    ("t", "t"),
    ("spc", " "),
    ("n", "n"),
    ("o", "o"),
    ("t", "t"),
    ("e", "e"),
    ("ret", "hi!\nyo\n"),
];

fn nano_typing_e2e_on(image_name: &str) {
    let keys = with_login(NANO_KEYS);
    let serial = boot_and_type(
        &image(image_name),
        &keys,
        "[boot] main loop ready",
        "hi!\nyo\n",
        Duration::from_millis(30),
        Duration::from_secs(240),
    );
    assert!(
        serial.contains("Wrote 5"),
        "nano did not save the first buffer; serial:\n{serial}"
    );
    assert!(
        serial.contains("Wrote 6"),
        "nano did not save the reopened buffer; serial:\n{serial}"
    );
    assert!(
        serial.contains("hi!\nyo\n"),
        "cat did not print the edited file; serial:\n{serial}"
    );
    assert!(
        !serial.contains("nano: keyboard denied"),
        "nano was spawned without the keyboard grant; serial:\n{serial}"
    );
    assert!(
        !serial.contains("[pf] ring-3 task fault"),
        "nano faulted; serial:\n{serial}"
    );
}

/// Milestone 69 differential: the `galexy-os-gxld` image carries init,
/// shell, hello and every util re-linked by `gxld` from the exact objects
/// and rlibs rustc hands `rust-lld`. Same keystrokes, same output.
#[test]
fn gxld_image_util_typing_e2e() {
    util_typing_e2e_on("galexy-os-gxld");
}

/// `hello` typed into the gxld-linked shell on the gxld-linked image.
#[test]
fn gxld_image_run_hello_typing_e2e() {
    let keys = with_login(RUN_HELLO_KEYS);
    let serial = boot_and_type(
        &image("galexy-os-gxld"),
        &keys,
        "[boot] main loop ready",
        "exited (syscall)",
        Duration::from_millis(30),
        Duration::from_secs(90),
    );
    assert!(
        serial.contains(HELLO_TEXT),
        "typed `hello` on the gxld image never produced user output; serial:\n{serial}"
    );
    assert!(
        serial.contains("exited (syscall)"),
        "user task exit marker missing on the gxld image; serial:\n{serial}"
    );
    assert!(
        !serial.contains("[pf] ring-3 task fault"),
        "a gxld-linked program faulted; serial:\n{serial}"
    );
}

fn util_typing_e2e_on(image_name: &str) {
    let keys = with_login(UTIL_KEYS);
    let serial = boot_and_type(
        &image(image_name),
        &keys,
        "[boot] main loop ready",
        "rm: directory not empty",
        Duration::from_millis(30),
        Duration::from_secs(180),
    );
    assert!(
        serial.contains("plumbing works"),
        "typed `cat banner.txt` missed the archive text; serial:\n{serial}"
    );
    assert!(
        serial.contains("scratch-hi\n"),
        "typed `echo >` did not round-trip; serial:\n{serial}"
    );
    assert!(
        serial.contains("more\n"),
        "typed `echo >>` did not append; serial:\n{serial}"
    );
    assert!(
        serial.contains("admin@galexy:/box> "),
        "typed `cd box` did not enter the directory; serial:\n{serial}"
    );
    assert!(
        serial.contains("inbox\n"),
        "typed create/write/read inside the directory failed; serial:\n{serial}"
    );
    assert!(
        serial.contains("box/"),
        "typed `ls` after `cd ..` missed the directory; serial:\n{serial}"
    );
    assert!(
        serial.contains("cat: not text"),
        "typed `cat hello` should fail the console charset; serial:\n{serial}"
    );
    assert!(
        serial.contains("echo: cannot replace"),
        "typed `echo > banner.txt` should be rejected; serial:\n{serial}"
    );
    assert!(
        serial.contains("cat: no such file"),
        "typed `rm note` should drop the scratch file; serial:\n{serial}"
    );
    assert!(
        serial.contains("rm: directory not empty"),
        "typed `rm box` should refuse a directory that still has a child; serial:\n{serial}"
    );
}

/// Cap-wait exit status surfaces as `echo $?`.
#[test]
fn shell_echo_status_typing_e2e() {
    let keys = with_login(STATUS_AFTER_LOGIN);
    let serial = boot_and_type(
        &image("galexy-os"),
        &keys,
        "[boot] main loop ready",
        "",
        Duration::from_millis(30),
        Duration::from_secs(90),
    );
    assert_passwords_masked(&serial);
    assert!(
        serial.contains("hi\n"),
        "typed `echo hi` never printed; serial:\n{serial}"
    );
    // Builtin `echo $?` after a successful utility Cap-wait.
    assert!(
        serial.contains("0\n"),
        "typed `echo $?` never printed 0; serial:\n{serial}"
    );
}

/// `help` during must-change (before `passwd`) — sectioned output.
/// Sync on a late keys blurb so the console budget can drain across ticks.
const HELP_DURING_MUST_CHANGE: &[(&str, &str)] = &[
    ("h", "h"),
    ("e", "e"),
    ("l", "l"),
    ("p", "p"),
    ("ret", "history (saved on logout)"),
];

#[test]
fn shell_help_typing_e2e() {
    // Login only — do not clear the default password so this also proves
    // `help` is allowed under the must-change gate.
    let mut keys = LOGIN_ADMIN_KEYS.to_vec();
    keys.extend(HELP_DURING_MUST_CHANGE.iter().copied());
    let serial = boot_and_type(
        &image("galexy-os"),
        &keys,
        "[boot] main loop ready",
        "default admin/admin must passwd",
        Duration::from_millis(30),
        Duration::from_secs(90),
    );
    assert!(
        serial.contains("[kbd] virtio-input"),
        "QMP typing must land on virtio-input; serial:\n{serial}"
    );
    assert_passwords_masked(&serial);
    assert!(
        serial.contains("change the default password"),
        "must-change banner missing after admin/admin; serial:\n{serial}"
    );
    assert!(
        serial.contains("Galexy.OS shell\n"),
        "typed `help` missing title; serial:\n{serial}"
    );
    for section in ["files\n", "session\n", "caps\n", "system\n", "keys\n"] {
        assert!(
            serial.contains(section),
            "typed `help` missing section {section:?}; serial:\n{serial}"
        );
    }
    assert!(
        serial.contains("list directory") && serial.contains("flush galfs to disk"),
        "typed `help` missing files blurbs; serial:\n{serial}"
    );
    assert!(
        serial.contains("dashboard (alias: dashboard)"),
        "typed `help` missing fetch row; serial:\n{serial}"
    );
    assert!(
        serial.contains("masked password + confirm prompts"),
        "typed `help` missing passwd blurb; serial:\n{serial}"
    );
    assert!(
        serial.contains("history (saved on logout)"),
        "typed `help` missing keys blurb; serial:\n{serial}"
    );
    assert!(
        serial.contains("default admin/admin must passwd"),
        "typed `help` missing notes; serial:\n{serial}"
    );
}

/// Username plus Enter on the i8042, so QMP is not only hitting virtio-input.
/// `Password:` cannot be spelled by the boot log, unlike a single letter.
#[test]
fn shell_ps2_typing_e2e() {
    let serial = boot_and_type_ps2(
        &image("galexy-os"),
        &LOGIN_ADMIN_KEYS[..6],
        "[boot] main loop ready",
        "Password: ",
        Duration::from_millis(30),
        Duration::from_secs(90),
    );
    assert!(
        serial.contains("[kbd] ps/2 i8042"),
        "PS/2 fallback was not selected; serial:\n{serial}"
    );
    assert!(
        !serial.contains("[kbd] virtio-input"),
        "PS/2 boot must not claim virtio-input; serial:\n{serial}"
    );
}

/// Login dashboard + in-session up-arrow recall, then logout persists
/// `shell.history` for the next login.
const COMMAND_CENTER_KEYS: &[(&str, &str)] = &[
    ("e", "e"),
    ("c", "c"),
    ("h", "h"),
    ("o", "o"),
    ("spc", " "),
    ("a", "a"),
    ("a", "a"),
    ("ret", "aa\n"),
    ("e", "e"),
    ("c", "c"),
    ("h", "h"),
    ("o", "o"),
    ("spc", " "),
    ("b", "b"),
    ("b", "b"),
    ("ret", "bb\n"),
    // ArrowUp recalls the newest line onto the prompt.
    ("up", "echo bb"),
    ("ret", "bb\n"),
    ("up", "echo bb"),
    ("up", "echo aa"),
    ("ret", "aa\n"),
    ("l", "l"),
    ("o", "o"),
    ("g", "g"),
    ("o", "o"),
    ("u", "u"),
    ("t", "t"),
    ("ret", "Login as: "),
];

#[test]
fn shell_command_center_typing_e2e() {
    let mut keys = with_login(COMMAND_CENTER_KEYS);
    // History is flushed on logout; next login loads shell.history.
    keys.extend(LOGIN_ADMIN_TESTPASS_KEYS.iter().copied());
    keys.extend([
        ("c", "c"),
        ("a", "a"),
        ("t", "t"),
        ("spc", " "),
        ("s", "s"),
        ("h", "h"),
        ("e", "e"),
        ("l", "l"),
        ("l", "l"),
        ("dot", "."),
        ("h", "h"),
        ("i", "i"),
        ("s", "s"),
        ("t", "t"),
        ("o", "o"),
        ("r", "r"),
        ("y", "y"),
        ("ret", "echo aa\n"),
    ]);
    let serial = boot_and_type(
        &image("galexy-os"),
        &keys,
        "[boot] main loop ready",
        "",
        Duration::from_millis(30),
        Duration::from_secs(150),
    );
    assert_passwords_masked(&serial);
    assert!(
        serial.contains("command center"),
        "login dashboard missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("Uptime"),
        "login dashboard missing uptime; serial:\n{serial}"
    );
    assert!(
        serial.contains("Galfs"),
        "login dashboard missing galfs; serial:\n{serial}"
    );
    assert!(
        !console_shows_init(&serial),
        "init must stay off the console beside login; serial:\n{serial}"
    );
    // History file persisted across logout (plus passwd from with_login).
    assert!(
        serial.contains("echo aa\n") && serial.contains("echo bb\n"),
        "shell.history / recall missed echo lines; serial:\n{serial}"
    );
}

/// Sealed-disk boot: the volume passphrase prompt comes before the login
/// banner. Default bring-up passphrase, masked.
const UNLOCK_VOLUME_KEYS: &[(&str, &str)] = &[
    ("g", "*"),
    ("a", "*"),
    ("l", "*"),
    ("f", "*"),
    ("s", "*"),
    ("ret", "Login as: "),
];

/// `sync` (explicit commit, silent) then `whoami` after a `passwd`: proves
/// the shell is live and accepting commands after the password change.
const SYNC_WHOAMI_KEYS: &[(&str, &str)] = &[
    ("s", "s"),
    ("y", "y"),
    ("n", "n"),
    ("c", "c"),
    ("ret", "admin@galexy> "),
    ("w", "w"),
    ("h", "h"),
    ("o", "o"),
    ("a", "a"),
    ("m", "m"),
    ("i", "i"),
    ("ret", "admin\n"),
];

#[test]
fn passwd_on_sealed_disk_typing_e2e() {
    // galexy.os#86: on the production image with the data disk attached
    // (the `cargo run` shape), `passwd` froze the guest after the audit
    // line. Boot 1 unlocks a fresh sealed disk, logs in with the default
    // password, changes it, runs commands, and must see the commit. Boot 2
    // reopens the same disk and logs in with the new password.
    let disk = common::fresh_galfs_image("passwd");
    let mut keys: Vec<(&str, &str)> = UNLOCK_VOLUME_KEYS.to_vec();
    keys.extend(LOGIN_ADMIN_KEYS.iter().copied());
    keys.extend(CLEAR_DEFAULT_PASSWD.iter().copied());
    keys.extend(SYNC_WHOAMI_KEYS.iter().copied());
    let serial = common::boot_and_type_galfs(
        &image("galexy-os"),
        &disk,
        &keys,
        "Volume passphrase: ",
        "",
        Duration::from_millis(30),
        Duration::from_secs(180),
    );
    assert_passwords_masked(&serial);
    assert!(
        !serial.contains("[PANIC]"),
        "boot 1 panicked; serial:\n{serial}"
    );
    assert!(
        serial.contains("[galfs] formatted sealed disk"),
        "unlock did not format the fresh disk; serial:\n{serial}"
    );
    let audit = serial
        .find("[auth] passwd user=admin")
        .unwrap_or_else(|| panic!("passwd audit line missing; serial:\n{serial}"));
    let after = &serial[audit..];
    assert!(
        after.contains("admin@galexy> "),
        "no prompt after passwd (galexy.os#86 shape); serial:\n{serial}"
    );
    assert!(
        after.contains("[galfs] committing slot"),
        "the new hash never committed to disk; serial:\n{serial}"
    );
    assert!(
        !after.contains("sync skipped"),
        "unlocked volume must not skip its sync; serial:\n{serial}"
    );
    assert!(
        serial.contains("passwd: change the default password"),
        "boot 1 must have started on the default password; serial:\n{serial}"
    );

    // Boot 2: same disk, new password, no must-change gate.
    let mut keys: Vec<(&str, &str)> = UNLOCK_VOLUME_KEYS.to_vec();
    keys.extend(LOGIN_ADMIN_TESTPASS_KEYS.iter().copied());
    keys.extend([
        ("w", "w"),
        ("h", "h"),
        ("o", "o"),
        ("a", "a"),
        ("m", "m"),
        ("i", "i"),
        ("ret", "admin\n"),
    ]);
    let serial2 = common::boot_and_type_galfs(
        &image("galexy-os"),
        &disk,
        &keys,
        "Volume passphrase: ",
        "",
        Duration::from_millis(30),
        Duration::from_secs(180),
    );
    let _ = std::fs::remove_file(&disk);
    assert_passwords_masked(&serial2);
    assert!(
        !serial2.contains("[PANIC]"),
        "boot 2 panicked; serial:\n{serial2}"
    );
    assert!(
        serial2.contains("[galfs] unlocked slot"),
        "boot 2 did not reopen the sealed volume; serial:\n{serial2}"
    );
    assert!(
        !serial2.contains("passwd: change the default password"),
        "the new password must clear the must-change gate across reboot; serial:\n{serial2}"
    );
    assert!(
        serial2.contains("[auth] session login user=admin"),
        "boot 2 login with the new password failed; serial:\n{serial2}"
    );
}

/// Init's own log lines are kernel serial (`Ns: [init] …`), not the seat
/// console. A login screen that contains `[init]` without that prefix
/// means init wrote on the TTY it shares with F1.
fn console_shows_init(serial: &str) -> bool {
    serial
        .lines()
        .any(|line| line.contains("[init]") && !kernel_init_line(line))
}

fn kernel_init_line(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] == b'*' || bytes[i] == b' ' || bytes[i] == 0x0c) {
        i += 1;
    }
    let rest = &line[i..];
    let rb = rest.as_bytes();
    let mut j = 0;
    if j >= rb.len() || !rb[j].is_ascii_digit() {
        return false;
    }
    while j < rb.len() && rb[j].is_ascii_digit() {
        j += 1;
    }
    rest[j..].starts_with("s: [init]")
}

/// True end-to-end: TYPES `hello` into the running kernel through
/// QEMU's QMP `send-key` (real PS/2 IRQs into the keyboard driver) and
/// asserts the user program's console output on COM1 (screen+serial
/// mirror make the result observable headless).
#[test]
fn shell_run_hello_typing_e2e() {
    // Typing starts only after the kernel's main loop (the shell's key
    // consumer) is live — serial marker, not a sleep: init timing under
    // TCG varies. Each key syncs on the guest's echo, so host load can
    // never overflow the i8042 queue between keys.
    // After `crash` the seat is logged out again — login, then probe.
    let mut keys = with_login(SUPERVISOR_AFTER_LOGIN);
    // New shell is logged out; password was changed before crash.
    keys.extend(LOGIN_ADMIN_TESTPASS_KEYS.iter().copied());
    keys.extend([("x", "x"), ("y", "beat\n")]);
    let serial = boot_and_type(
        &image("galexy-os-crashseam"),
        &keys,
        "[boot] main loop ready",
        "beat\n",
        Duration::from_millis(30),
        Duration::from_secs(120),
    );
    assert!(
        serial.contains(HELLO_TEXT),
        "typed `hello` never produced user output; serial:\n{serial}"
    );
    assert!(
        serial.contains("exited (syscall)"),
        "user task exit marker missing after typed run; serial:\n{serial}"
    );
    let fault_at = serial
        .find("[pf] ring-3 task fault")
        .expect("typed `crash` never faulted the shell");
    let after_fault = &serial[fault_at..];
    let prompt_at = after_fault.find("admin@galexy> ").unwrap_or_else(|| {
        panic!("login after fault never produced admin prompt; serial:\n{serial}")
    });
    assert!(
        after_fault[prompt_at..].contains("beat\n"),
        "linger stopped printing after the shell restarted; serial:\n{serial}"
    );
}

/// True end-to-end under UEFI: the same `hello` typing flow, into the
/// OVMF-booted kernel — the APIC delivery path (LAPIC timer + I/O APIC
/// keyboard) drives the whole thing there. OVMF boot flakiness → 3 retries.
#[test]
fn shell_run_hello_typing_e2e_uefi() {
    let image = image("galexy-os");
    let keys = with_login(RUN_HELLO_KEYS);
    let mut last = String::new();
    for _ in 0..3 {
        last = boot_and_type_uefi(
            &image,
            &keys,
            "[boot] main loop ready",
            "exited (syscall)",
            Duration::from_millis(30),
            Duration::from_secs(90),
        );
        if last.contains(HELLO_TEXT) && last.contains("exited (syscall)") {
            break;
        }
    }
    assert!(
        last.contains(HELLO_TEXT),
        "typed `hello` under UEFI never produced user output; serial:\n{last}"
    );
    assert!(
        last.contains("exited (syscall)"),
        "user task exit marker missing after typed run (UEFI); serial:\n{last}"
    );
}

/// F2 runs a command on the second shell; F1 returns to the first.
#[test]
fn shell_tty_switch_e2e() {
    // F1 clears the default password first so F2's login uses testpass.
    let mut keys = with_login(&[]);
    keys.extend(TTY_AFTER_F2.iter().copied());
    keys.extend(LOGIN_ADMIN_TESTPASS_KEYS.iter().copied());
    keys.extend(TTY_AFTER_LOGIN.iter().copied());
    let serial = boot_and_type(
        &image("galexy-os"),
        &keys,
        "[boot] main loop ready",
        "hi",
        Duration::from_millis(30),
        Duration::from_secs(120),
    );
    let tty2 = serial.find("[tty] 2").expect("F2 never switched consoles");
    let hi = serial[tty2..]
        .find("hi")
        .expect("echo on tty 2 never printed");
    let tty1 = serial[tty2 + hi..]
        .find("[tty] 1")
        .expect("F1 never switched back");
    assert!(
        serial[tty2 + hi + tty1..].contains('z'),
        "the first shell did not echo after F1; serial:\n{serial}"
    );
    assert_passwords_masked(&serial);
}

#[test]
fn smp_test_passes() {
    let (code, serial) = boot(&image("test-smp"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-smp should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-smp] passed"),
        "test-smp success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-smp] online=2"),
        "the SMP substrate must count 2 PCs; serial:\n{serial}"
    );
    assert!(
        serial.contains("owners: t1=0 t2=1 t3=0 t4=1"),
        "pin-at-spawn distribution marker missing; serial:\n{serial}"
    );
}

#[test]
fn ipi_test_passes() {
    // The shootdown IPI machinery (M19): broadcast + lock-free ack, end to
    // end across the two CPUs the harness always provides.
    let (code, serial) = boot(&image("test-ipi"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-ipi should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-ipi] passed"),
        "test-ipi success marker missing; serial:\n{serial}"
    );
}

#[test]
fn lockgrow_test_passes() {
    // galexy.os#86: heap growth under a held lock must not deadlock
    // against a CPU spinning IF=0 on that lock. The AP's lock spin acks
    // the BSP's shootdown without the IPI.
    let (code, serial) = boot(&image("test-lockgrow"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-lockgrow should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-lockgrow] passed"),
        "test-lockgrow success marker missing; serial:\n{serial}"
    );
    assert!(
        !serial.contains("[shootdown] cpu 0 waiting on cpu 1"),
        "the AP must ack from its lock spin, not stall the BSP; serial:\n{serial}"
    );
}

#[test]
fn smpuser_test_passes() {
    // The ring-3 blobs print via the console mirror (screen + serial), so
    // the fact the AP-side task's WRITE syscall ran is visible headless.
    let (code, serial) = boot(&image("test-smpuser"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-smpuser should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-smpuser] passed"),
        "test-smpuser success marker missing; serial:\n{serial}"
    );
    // Ring-3 completion proof, matching the milestone pattern (the blob
    // prints to the SCREEN; the observe-headless path is the scratch page
    // — see test-user's design note): both DONE marks must appear.
    assert!(
        serial.contains("[test-smpuser] task 0 marked done")
            && serial.contains("[test-smpuser] task 1 marked done"),
        "both ring-3 tasks must reach their scratch-mark (both CPUs ran a program); serial:\n{serial}"
    );
}

#[test]
fn smpstress_test_passes() {
    // M19 stress: idle-CPU steal proof + concurrent heap growth (shootdown
    // crossings) + hammering with exact frame closure.
    let (code, serial) = boot(&image("test-smpstress"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-smpstress should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-smpstress] passed"),
        "test-smpstress success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-smpstress] phase A: steal proven"),
        "the steal proof must show the owner flip; serial:\n{serial}"
    );
}

#[test]
fn uefi_image_boots_and_timer_ticks() {
    // Since the APIC work (M17), the timer is LAPIC-delivered on every boot
    // path — the UEFI image is live, not just booting: assert the heartbeat.
    //
    // OVMF's first-boot device enumeration is flaky under QEMU (the disk is
    // sometimes "Not Found" when BDS builds boot options) — retry up to 3x.
    let image = image("galexy-os");
    let mut last = String::new();
    for _ in 0..3 {
        last = boot_uefi(&image, Duration::from_secs(45));
        if serial_uptime_secs(&last) >= 1 {
            break;
        }
    }
    assert!(
        last.contains("boot info: rsdp_addr"),
        "UEFI boot info marker missing after 3 attempts; serial:\n{last}"
    );
    assert!(
        last.contains("[mm] frame allocator ready"),
        "UEFI memory bring-up marker missing; serial:\n{last}"
    );
    assert!(
        serial_uptime_secs(&last) >= 1,
        "UEFI timer uptime prefix missing (LAPIC timer must tick under OVMF); serial:\n{last}"
    );
    assert!(
        last.contains("[acpi] madt ready"),
        "UEFI MADT discovery marker missing; serial:\n{last}"
    );
    assert!(
        last.contains("[apic] lapic up"),
        "UEFI LAPIC enable marker missing; serial:\n{last}"
    );
}

#[test]
fn main_kernel_boots_and_timer_ticks() {
    // The interactive kernel never exits; verify liveness markers instead.
    // (Generous window: TCG boot + banner render stretch badly when the
    // host is loaded — 20 s was observed to cut the first heartbeat off.)
    let serial = boot_liveness(&image("galexy-os"), Duration::from_secs(45));
    assert!(
        serial.contains("boot info: rsdp_addr"),
        "boot info marker missing; serial:\n{serial}"
    );
    assert!(
        serial_uptime_secs(&serial) >= 1,
        "timer uptime prefix missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[loader] program 'shell' ready"),
        "ring-3 shell was not spawned; serial:\n{serial}"
    );
}

/// KVM-only ceilings (µs). TCG is recorded in `docs/PERF.md` and is not
/// gated. These are loose until a host whose KVM can create a vCPU
/// records a release run; this landing host's `kvm_arch_vcpu_create`
/// BUGs, so the numbers were not tightened here.
const BENCH_YIELD_US: u64 = 2_000_000;
const BENCH_SPAWN_US: u64 = 2_000_000;
const BENCH_PIPE_US: u64 = 2_000_000;
const BENCH_GALFS_US: u64 = 5_000_000;
const BENCH_REPAINT_US: u64 = 5_000_000;

fn bench_us(serial: &str, name: &str) -> u64 {
    let key = format!("[bench] name={name} us=");
    let line = serial
        .lines()
        .find(|line| line.contains(&key))
        .unwrap_or_else(|| panic!("missing {key}; serial:\n{serial}"));
    let raw = line
        .split("us=")
        .nth(1)
        .unwrap_or_else(|| panic!("bad bench line {line}"));
    raw.trim()
        .parse()
        .unwrap_or_else(|_| panic!("bad bench us in {line}"))
}

#[test]
fn bench_test_passes() {
    let (code, serial) = boot(&image("test-bench"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-bench should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-bench] passed"),
        "test-bench success marker missing; serial:\n{serial}"
    );
    let yield_us = bench_us(&serial, "yield");
    let spawn_us = bench_us(&serial, "spawn");
    let pipe_us = bench_us(&serial, "pipe");
    let galfs_us = bench_us(&serial, "galfs");
    let repaint_us = bench_us(&serial, "repaint");
    assert!(yield_us > 0 && spawn_us > 0 && pipe_us > 0);
    if use_kvm() {
        assert!(yield_us < BENCH_YIELD_US, "yield {yield_us} us");
        assert!(spawn_us < BENCH_SPAWN_US, "spawn {spawn_us} us");
        assert!(pipe_us < BENCH_PIPE_US, "pipe {pipe_us} us");
        assert!(galfs_us < BENCH_GALFS_US, "galfs {galfs_us} us");
        assert!(repaint_us < BENCH_REPAINT_US, "repaint {repaint_us} us");
    }
}

/// Highest `Ns:` uptime prefix seen in a serial transcript (tickless may
/// skip printing on the exact `1s:` boundary).
fn serial_uptime_secs(serial: &str) -> u64 {
    let mut best = 0u64;
    for line in serial.lines() {
        let Some((head, _)) = line.split_once("s: ") else {
            continue;
        };
        if let Ok(n) = head.parse::<u64>() {
            best = best.max(n);
        }
    }
    best
}
