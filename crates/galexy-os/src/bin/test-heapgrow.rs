//! Integration test kernel: heap grow-on-demand — allocates far past the
//! initial 400 KiB, verifies the page-mapped growth, and confirms frames are
//! actually consumed (the stat that reaches the status bar).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::{vec, vec::Vec};
use bootloader_api::{entry_point, BootInfo};
use galexy_os::{drivers::screen, exit_qemu, println, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-heapgrow] running");
    serial_println!("[test-heapgrow] running");

    galexy_os::arch::mm::init(boot_info);
    galexy_os::arch::init(boot_info);

    let initial_size = galexy_os::arch::mm::heap::stats().1;
    let frames_before = galexy_os::arch::mm::free_frames();

    // Allocate well past the initial heap, touching every byte so growth has
    // to have actually mapped fresh pages. Hold everything live until the
    // end so mapped heap memory can never be reused for scratch.
    let mut keep: Vec<Vec<u8>> = Vec::new();
    for _ in 0..40 {
        // 4 buffers of 32 KiB = one 128 KiB batch per iteration; 40 iterations
        // = 5 MiB, ~12.8x the initial heap. Each alloc failure inside alloc's
        // grow path triggers the page mapping.
        let mut batch: Vec<Vec<u8>> = Vec::new();
        for _ in 0..4 {
            batch.push(vec![0xAB_u8; 32 * 1024]);
        }
        for buf in &batch {
            for (i, b) in buf.iter().enumerate() {
                assert_eq!(*b, 0xAB, "fresh heap memory readable at {i}");
            }
        }
        keep.extend(batch);
    }

    let end_size = galexy_os::arch::mm::heap::stats().1;
    let frames_after = galexy_os::arch::mm::free_frames();

    // Growth must be visible: (a) more frames consumed than the initial heap
    // mapping could have used, and (b) page mapping + roundtrips returned
    // sane data. The heap size readout grows if the growth path fires.
    let used_frames_delta = frames_before - frames_after;
    // Initial heap = 100 pages; growth must push that meaningfully past it.
    assert!(
        used_frames_delta > 200,
        "expected growth to consume >200 frames, got {used_frames_delta}"
    );
    // All buffers kept their pattern.
    for (bi, buf) in keep.iter().enumerate() {
        assert!(buf.iter().all(|&b| b == 0xAB), "buffer {bi} corrupted");
    }

    // SMP M19 marker: every growth chunk is mapped kernel-half, so the
    // shootdown IPI broadcast must have run for real (the harness boots at
    // -smp 2 — the AP's lock-free handler acked every one of these).
    let broadcasts = galexy_os::arch::mm::shootdown::broadcast_count();
    serial_println!("[test-heapgrow] shootdown broadcasts: {}", broadcasts);
    assert!(
        broadcasts >= 1,
        "heap growth must broadcast shootdowns (got {})",
        broadcasts
    );

    println!(
        "[test-heapgrow] heap {} -> {} bytes",
        initial_size, end_size
    );
    println!("[test-heapgrow] all growth checks passed");
    serial_println!("[test-heapgrow] passed");
    exit_qemu(QemuExitCode::Success);
}
