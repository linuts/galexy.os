//! Integration test kernel: the shell's `run <program>` command. Drives the
//! shell's command dispatcher DIRECTLY (no keystrokes): `run hello` must
//! find hello's ELF in the ramdisk, spawn it, print through the console
//! syscall (screen + serial mirror), and reap the exited task with the
//! frame accounting closed.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{drivers::screen, exit_qemu, println, sched, serial_println, shell, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-runshell] running");
    serial_println!("[test-runshell] running");

    let Some(ramdisk_addr) = boot_info.ramdisk_addr.into_option() else {
        panic!("no ramdisk handed to the kernel");
    };
    let ramdisk_len = boot_info.ramdisk_len;

    galexy_os::arch::mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    sched::init();

    // SAFETY: the bootloader mapped the contiguous ramdisk image at
    // [ramdisk_addr, +len) into the kernel's (and thus every) space.
    let archive = unsafe {
        core::slice::from_raw_parts(
            x86_64::VirtAddr::new(ramdisk_addr).as_ptr::<u8>(),
            ramdisk_len as usize,
        )
    };
    sched::ramdisk::init(archive);

    // Sanity: the service finds hello (and misses a bogus name).
    let Some(hello_elf) = sched::ramdisk::find("hello") else {
        panic!("'hello' not found in the ramdisk tar");
    };
    assert!(
        sched::ramdisk::find("no-such-program").is_none(),
        "ramdisk::find returned a phantom entry"
    );
    serial_println!(
        "[test-runshell] ramdisk ok: hello = {} bytes",
        hello_elf.len()
    );

    let baseline = galexy_os::arch::mm::free_frames();

    // The shell seam: same dispatch the typing flow reaches via poll().
    shell::exec("run hello");
    serial_println!("[test-runshell] dispatch done");

    // Main loop: hlt + rotations while the program runs; the entry shim
    // exits the task when main returns 0 — tombstone + reap.
    let mut polls = 0u64;
    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::threads_count() == 0 {
            break;
        }
        polls += 1;
        if polls > 4000 {
            panic!("hello never exited");
        }
    }

    // Drain: the exit handoff can land between `reap()` and the count
    // check above (the count only reflects RUNNING state), leaving the
    // tree unfreed at this point — keep the rotation + reaper running
    // until the frame accounting stops improving.
    let mut stable = 0u32;
    while stable < 16 {
        x86_64::instructions::hlt();
        let before = galexy_os::arch::mm::free_frames();
        sched::reap();
        stable = if galexy_os::arch::mm::free_frames() == before {
            stable + 1
        } else {
            0
        };
    }

    // Accounting: the program's tree (data + tables) returned wholesale.
    let now = galexy_os::arch::mm::free_frames();
    assert!(
        now >= baseline,
        "program frames must return to the allocator: baseline={baseline} now={now}"
    );

    println!("[test-runshell] run-command lifecycle complete");
    println!("[test-runshell] all assertions passed");
    serial_println!("[test-runshell] passed");
    exit_qemu(QemuExitCode::Success);
}
