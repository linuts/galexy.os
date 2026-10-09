//! Ring 0 must not read a user virtual address without `stac` when SMAP
//! is on. The #PF handler exits Success. A completed read is a failure.

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

extern "x86-interrupt" fn smap_fault(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    serial_println!(
        "[test-smap] fault rip={:#x} err={:?}",
        stack_frame.instruction_pointer.as_u64(),
        error_code
    );
    serial_println!("[test-smap] passed");
    exit_qemu(QemuExitCode::Success);
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    serial_println!("[test-smap] running");
    mm::init(boot_info);
    galexy_os::arch::init(boot_info);

    let frame = mm::allocate_frame().expect("frame");
    let page = (1..256u64)
        .map(|idx| Page::<Size4KiB>::containing_address(VirtAddr::new(idx << 39)))
        .find(|page| mm::translate(page.start_address()).is_none())
        .expect("free page for the SMAP probe");
    mm::map_page_flags(
        page,
        frame,
        PageTableFlags::PRESENT
            | PageTableFlags::WRITABLE
            | PageTableFlags::USER_ACCESSIBLE
            | PageTableFlags::NO_EXECUTE,
    )
    .expect("map user data page");

    set_page_fault_handler(smap_fault);
    let ptr = page.start_address().as_u64() as *const u8;
    // SAFETY: the page is mapped user-accessible. SMAP must fault this
    // read because `stac` was not executed. The handler does not return.
    let _byte = unsafe { ptr.read_volatile() };

    serial_println!("[test-smap] read returned — SMAP did not fault");
    exit_qemu(QemuExitCode::Failed);
}
