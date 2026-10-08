//! Integration test: user-map W^X — jumping to the scratch page faults.
//!
//! Blob maps are RX (code) and RW|NX (stack/scratch). Writing a `ret` into
//! scratch and jumping there must #PF (instruction fetch on NX); the OS
//! reaps the task and returns frames to baseline. Also checks the ELF
//! loader rejects a forged W|X PT_LOAD flag word.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-wx] running");
    serial_println!("[test-wx] running");

    let Some(ramdisk_addr) = boot_info.ramdisk_addr.into_option() else {
        panic!("no ramdisk");
    };
    let ramdisk_len = boot_info.ramdisk_len as usize;

    mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    sched::init();

    let archive = unsafe {
        core::slice::from_raw_parts(
            x86_64::VirtAddr::new(ramdisk_addr).as_ptr::<u8>(),
            ramdisk_len,
        )
    };
    sched::ramdisk::init(archive);

    // Minimal ET_EXEC with one PT_LOAD flagged W|X — must be refused.
    let mut bad = [0u8; 128];
    bad[0..4].copy_from_slice(&0x464C_457Fu32.to_le_bytes()); // ELFMAG
    bad[4] = 2; // ELFCLASS64
    bad[5] = 1; // ELFDATA2LSB
    bad[6] = 1; // EV_CURRENT
    bad[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    bad[18..20].copy_from_slice(&0x3Eu16.to_le_bytes()); // EM_X86_64
    bad[20..24].copy_from_slice(&1u32.to_le_bytes()); // EV_CURRENT
    bad[24..32].copy_from_slice(&0x0040_0000u64.to_le_bytes()); // e_entry
    bad[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    bad[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    bad[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    bad[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
                                                      // Program header at 64: PT_LOAD, PF_R|PF_W|PF_X
    bad[64..68].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    bad[68..72].copy_from_slice(&7u32.to_le_bytes()); // PF_X|W|R
    bad[72..80].copy_from_slice(&0u64.to_le_bytes()); // p_offset
    bad[80..88].copy_from_slice(&0x0040_0000u64.to_le_bytes()); // p_vaddr
    bad[88..96].copy_from_slice(&0x0040_0000u64.to_le_bytes()); // p_paddr
    bad[96..104].copy_from_slice(&8u64.to_le_bytes()); // p_filesz
    bad[104..112].copy_from_slice(&8u64.to_le_bytes()); // p_memsz
    bad[112..120].copy_from_slice(&0x1000u64.to_le_bytes()); // p_align
    assert!(
        !sched::loader::elf_bytes_wx_ok(&bad),
        "W|X PT_LOAD must fail elf_bytes_wx_ok"
    );
    let hello = sched::ramdisk::find("hello").expect("hello in ramdisk");
    assert!(
        sched::loader::elf_bytes_wx_ok(hello),
        "hello must satisfy W^X"
    );

    let baseline = mm::free_frames();
    let main_before = sched::main_ticks();

    // Blob: write `ret` (0xC3) into scratch, then jmp scratch — NX #PF.
    let (_region, _) = sched::spawn_user_task("wxboom", |gr| {
        let mut code = alloc::vec::Vec::new();
        // movabs rax, scratch
        code.extend_from_slice(&[0x48, 0xB8]);
        code.extend_from_slice(&gr.scratch.as_u64().to_le_bytes());
        // mov byte ptr [rax], 0xC3
        code.extend_from_slice(&[0xC6, 0x00, 0xC3]);
        // jmp rax
        code.extend_from_slice(&[0xFF, 0xE0]);
        code
    });

    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }

    assert!(
        sched::main_ticks() > main_before + 1,
        "main loop must keep running after NX fault"
    );
    assert_eq!(
        mm::free_frames(),
        baseline,
        "NX-faulted task must reclaim all frames"
    );

    println!("[test-wx] W^X: scratch NX fault + ELF check ok");
    serial_println!("[test-wx] passed");
    exit_qemu(QemuExitCode::Success);
}
