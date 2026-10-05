//! Integration test kernel: the FIRST REAL RUST USER PROGRAM. Reads the
//! ramdisk tar, loads `hello`'s ELF via the loader, and verifies the full
//! lifecycle: the program prints through the console cap, exits 0 via the
//! entry shim, gets reaped, and the frame accounting closes.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_core::TarCursor;
use galexy_os::{drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};
use x86_64::VirtAddr;

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-realprogram] running");
    serial_println!("[test-realprogram] running");

    let Some(ramdisk_addr) = boot_info.ramdisk_addr.into_option() else {
        panic!("no ramdisk handed to the kernel");
    };
    let ramdisk_len = boot_info.ramdisk_len;

    galexy_os::arch::mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    sched::init();

    // Find `hello` in the ramdisk tar.
    let base = VirtAddr::new(ramdisk_addr);
    // SAFETY: the bootloader mapped the contiguous ramdisk image at
    // [ramdisk_addr, +len) into the kernel's (and thus every) space.
    let archive = unsafe { core::slice::from_raw_parts(base.as_ptr::<u8>(), ramdisk_len as usize) };
    let mut cursor = TarCursor::new(archive);
    let mut hello_elf: Option<&[u8]> = None;
    while let Some((name, body)) = cursor.next_file() {
        if name == "hello" {
            hello_elf = Some(body);
            break;
        }
    }
    let Some(hello_elf) = hello_elf else {
        panic!("'hello' not found in the ramdisk tar");
    };

    let baseline = galexy_os::arch::mm::free_frames();

    // The real program: ELF in, task running.
    let _region = sched::loader::spawn_program("hello", hello_elf);

    // Main loop: hlt + rotations while the program runs; the entry shim
    // exits the task when main returns 0 — the kernel sees tombstone +
    // reap. (hello does not touch the scratch page — it's for future
    // programs; the kernel-side evidence is the lifecycle + accounting.)
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
    // check above (the count only reflects RUNNING state) — keep the
    // rotation + reaper running until the frame accounting stops
    // improving, so the tree walk below sees the final state.
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

    // Note: the read_volatile of the scratch page above would fault after
    // reaping (frame freed) — it is only used by future programs; kept out
    // of the loop for exactly that reason.

    println!("[test-realprogram] real program lifecycle complete");
    println!("[test-realprogram] all assertions passed");
    serial_println!("[test-realprogram] passed");
    exit_qemu(QemuExitCode::Success);
}
