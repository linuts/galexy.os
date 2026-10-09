//! `sgdt` from ring 3 must #GP under UMIP and kill only that task.
//!
//! The blob stores a marker after the `sgdt`. A kill before the store
//! leaves the marker zero. A successful `sgdt` sets it and loops.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{arch::mm, drivers::screen, exit_qemu, sched, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    serial_println!("[test-umip] running");
    mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    sched::init();

    let baseline = mm::free_frames();
    let _ = sched::spawn_user_task("umip", |gr| {
        let mut code = alloc::vec::Vec::new();
        // movabs rax, scratch
        code.extend_from_slice(&[0x48, 0xB8]);
        code.extend_from_slice(&gr.scratch.as_u64().to_le_bytes());
        // sgdt [rax]
        code.extend_from_slice(&[0x0F, 0x01, 0x00]);
        // mov byte ptr [rax+16], 1
        code.extend_from_slice(&[0xC6, 0x40, 0x10, 0x01]);
        // jmp $
        code.extend_from_slice(&[0xEB, 0xFE]);
        code
    });

    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }

    // A completed `sgdt` stores the marker and loops, so this loop would
    // not finish. Reaching here means the task was killed. The runner
    // also requires the ring-3 fault line.
    assert_eq!(
        mm::free_frames(),
        baseline,
        "UMIP fault must reclaim the task"
    );
    serial_println!("[test-umip] passed");
    exit_qemu(QemuExitCode::Success);
}
