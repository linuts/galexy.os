//! Idle logout uses `timer_ticks`. The test backdates the last keystroke
//! (tick injection) instead of waiting out [`sched::IDLE_LOGOUT_MS`].

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{Syscall, USER_LOGIN};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const TRIPPED: u64 = 0x10C0_1D1E;
const SEEN: u64 = 2;
const POLL_LIMIT: u64 = 80_000;

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-idle] running");
    serial_println!("[test-idle] running");

    let Some(ramdisk_addr) = boot_info.ramdisk_addr.into_option() else {
        panic!("no ramdisk");
    };
    mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    sched::init();
    let archive = unsafe {
        core::slice::from_raw_parts(
            x86_64::VirtAddr::new(ramdisk_addr).as_ptr::<u8>(),
            boot_info.ramdisk_len as usize,
        )
    };
    sched::ramdisk::init(archive);

    let (region, owner) = sched::spawn_user_task("idle1", |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_blob(gr.code.as_u64(), gr.scratch.as_u64())
    });
    assert_eq!(owner, 0, "first blob stays on the BSP");

    let base = mm::frame_virt(region.scratch_phys);
    let mut elapsed = 0u64;
    loop {
        x86_64::instructions::hlt();
        let tripped = unsafe { core::ptr::read_volatile(base.as_ptr::<u64>().add(1)) };
        if tripped == TRIPPED {
            break;
        }
        elapsed += 1;
        if elapsed > POLL_LIMIT {
            panic!("login never finished");
        }
    }

    // A second blob sleeps, then the kernel retargets that park into a
    // Cap-wait on idle1 — the shape of init blocked in `wait`. Idle
    // logout must resume it with 137 and drop the process Cap. Pin it
    // to the BSP so this loop is the one that runs the resume.
    let (watch_region, watch_owner) = sched::spawn_user_task_on("watch", 0, |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_watch(gr.scratch.as_u64())
    });
    assert_eq!(watch_owner, 0, "watcher stays on the BSP");
    let watch_base = mm::frame_virt(watch_region.scratch_phys);
    elapsed = 0;
    loop {
        x86_64::instructions::hlt();
        if sched::test_task_waiting("watch") {
            break;
        }
        elapsed += 1;
        if elapsed > POLL_LIMIT {
            panic!("watcher never parked");
        }
    }
    assert!(
        sched::test_arm_exit_wait("watch", "idle1"),
        "Cap-wait on the logged-in seat"
    );

    // Boot is only a few seconds in, so a 60 s backdate saturates to 0
    // and the scan treats that as "no sample". Shrink the window instead.
    sched::test_set_idle_limit(1);
    sched::test_backdate_input("idle1", 5);
    sched::test_poll_idle_all();
    assert!(
        sched::test_auth_flags("idle1").is_none(),
        "idle logout must exit the seat"
    );
    assert!(
        !sched::test_task_waiting("watch"),
        "idle logout must resume the Cap-waiter"
    );
    assert!(
        !sched::test_holds_child("watch", "idle1"),
        "exit wake drops the process Cap"
    );

    elapsed = 0;
    loop {
        x86_64::instructions::hlt();
        let phase = unsafe { core::ptr::read_volatile(watch_base.as_ptr::<u64>()) };
        if phase == SEEN {
            break;
        }
        elapsed += 1;
        if elapsed > POLL_LIMIT {
            panic!("waiter never observed the exit");
        }
    }
    let stamped = unsafe { core::ptr::read_volatile(watch_base.as_ptr::<u64>().add(1)) };
    let ok = unsafe { core::ptr::read_volatile(watch_base.as_ptr::<u64>().add(2)) };
    assert_eq!(stamped, 137, "Cap-waiter sees exit 137");
    assert_eq!(ok, 1, "exit wake is a successful syscall");
    serial_println!("[test-idle] waiter saw 137");

    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }

    println!("[test-idle] logged out");
    serial_println!("[test-idle] passed");
    exit_qemu(QemuExitCode::Success);
}

fn build_blob(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let admin = b"admin";
    let mut code = alloc::vec::Vec::new();
    let data_len = admin.len() * 2;
    code.push(0xEB);
    code.push(data_len as u8);
    let name_addr = code_base + 2;
    code.extend_from_slice(admin);
    let pass_addr = name_addr + admin.len() as u64;
    code.extend_from_slice(admin);

    mov_r64_imm(&mut code, 15, scratch);
    mov_eax(&mut code, Syscall::User as u32);
    mov_r64_imm(&mut code, 7, name_addr);
    mov_r64_imm(&mut code, 6, admin.len() as u64);
    mov_r64_imm(&mut code, 2, USER_LOGIN);
    mov_r64_imm(&mut code, 8, pass_addr);
    mov_r64_imm(&mut code, 9, admin.len() as u64);
    code.extend_from_slice(&[0x0F, 0x05]);

    mov_r64_imm(&mut code, 0, TRIPPED);
    store(&mut code, 0, 0x08);

    // Park so the BSP can observe TRIPPED and idle-exit this task.
    mov_eax(&mut code, Syscall::Sleep as u32);
    mov_r64_imm(&mut code, 7, 30_000);
    code.extend_from_slice(&[0x0F, 0x05]);

    mov_eax(&mut code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF, 0x0F, 0x05]);
    code
}

/// Sleep, then publish the syscall result. The kernel turns the sleep
/// into a Cap-wait before the deadline, so `rax`/`rdx` are the exit wake.
fn build_watch(scratch: u64) -> alloc::vec::Vec<u8> {
    let mut code = alloc::vec::Vec::new();
    mov_r64_imm(&mut code, 15, scratch);
    mov_eax(&mut code, Syscall::Sleep as u32);
    mov_r64_imm(&mut code, 7, 30_000);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 0, 0x08);
    store(&mut code, 2, 0x10);
    mov_r64_imm(&mut code, 0, SEEN);
    store(&mut code, 0, 0x00);
    mov_eax(&mut code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF, 0x0F, 0x05]);
    code
}

fn mov_r64_imm(code: &mut alloc::vec::Vec<u8>, reg: u8, imm: u64) {
    let rex = 0x48 | u8::from(reg >= 8);
    code.push(rex);
    code.push(0xB8 + (reg & 7));
    code.extend_from_slice(&imm.to_le_bytes());
}

fn mov_eax(code: &mut alloc::vec::Vec<u8>, imm: u32) {
    code.push(0xB8);
    code.extend_from_slice(&imm.to_le_bytes());
}

fn store(code: &mut alloc::vec::Vec<u8>, reg: u8, disp: i32) {
    let modrm = 0x80 | (reg << 3) | 7;
    code.extend_from_slice(&[0x49, 0x89, modrm]);
    code.extend_from_slice(&disp.to_le_bytes());
}
