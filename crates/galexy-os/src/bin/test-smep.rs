//! Ring 0 must not execute a user-mapped page when SMEP is on.
//!
//! Maps one user-accessible executable page, writes `ret` through the
//! physical map, and calls it. The #PF handler exits Success. A return
//! means SMEP did not fault.

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{
    arch::{mm, set_page_fault_handler},
    drivers::screen,
    exit_qemu, serial_println, QemuExitCode,
};
use x86_64::structures::idt::{InterruptStackFrame, PageFaultErrorCode};
use x86_64::structures::paging::{Page, PageTableFlags, Size4KiB};
use x86_64::VirtAddr;

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

extern "x86-interrupt" fn smep_fault(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    serial_println!(
        "[test-smep] fault rip={:#x} err={:?}",
        stack_frame.instruction_pointer.as_u64(),
        error_code
    );
    serial_println!("[test-smep] passed");
    exit_qemu(QemuExitCode::Success);
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    serial_println!("[test-smep] running");
    mm::init(boot_info);
    galexy_os::arch::init(boot_info);

    let frame = mm::allocate_frame().expect("frame");
    // KASLR may occupy any dynamic P4 slot. Take the first unmapped one.
    let page = (1..256u64)
        .map(|idx| Page::<Size4KiB>::containing_address(VirtAddr::new(idx << 39)))
        .find(|page| mm::translate(page.start_address()).is_none())
        .expect("free page for the SMEP probe");
    mm::map_page_flags(
        page,
        frame,
        PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE,
    )
    .expect("map user exec page");
    // SAFETY: the frame is exclusively owned; write `ret` via the phys map
    // so SMAP does not fault the setup.
    unsafe {
        mm::frame_virt(frame.start_address())
            .as_mut_ptr::<u8>()
            .write(0xC3);
    }

    set_page_fault_handler(smep_fault);
    let entry = page.start_address().as_u64();
    // SAFETY: the address is the page just mapped. SMEP must fault the
    // call; the handler does not return.
    let call: extern "C" fn() = unsafe { core::mem::transmute(entry) };
    call();

    serial_println!("[test-smep] call returned — SMEP did not fault");
    exit_qemu(QemuExitCode::Failed);
}
