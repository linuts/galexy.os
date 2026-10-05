//! Integration test kernel: virtual memory paging — map a frame to a fresh
//! virtual page, write/read through it, compare with the physical mapping,
//! unmap, and verify the unmapped access faults.

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{
    arch::{mm, set_page_fault_handler},
    drivers::screen,
    exit_qemu, println, serial_println, QemuExitCode,
};
use x86_64::structures::idt::{InterruptStackFrame, PageFaultErrorCode};
use x86_64::structures::paging::{Page, Size4KiB};
use x86_64::VirtAddr;

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

/// Replaces the report-and-park handler: an (unexpected) page fault during
/// this test must fail the run with a clear marker. (Never returns; the
/// `!`-typed body fits the unit signature because exit_qemu diverges.)
extern "x86-interrupt" fn fault_test_handler(
    _stack_frame: InterruptStackFrame,
    _error_code: PageFaultErrorCode,
) {
    serial_println!("[test-paging] page fault fired as expected");
    exit_qemu(QemuExitCode::Success);
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-paging] running");
    serial_println!("[test-paging] running");

    let phys_offset = boot_info
        .physical_memory_offset
        .into_option()
        .expect("physical memory must be mapped (see BOOTLOADER_CONFIG)");

    galexy_os::arch::init(boot_info); // IDT up first: faults are diagnosable
    mm::init(boot_info); // also initializes the page mapper

    // Map a freshly allocated frame at a fresh virtual page in P4 entry 100
    // (canonical; unused — dynamics fill from index 0, phys mem is 32,
    // recursive is 511).
    let frame = mm::allocate_frame().expect("frame alloc");
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(100 << 39));
    mm::map_page(page, frame).expect("map page");
    let virt = page.start_address().as_u64();
    let virt_ptr = virt as *mut u64;

    // Write through the NEW mapping, read back through BOTH mappings.
    // SAFETY: the frame is Usable and otherwise unmapped (exclusive access);
    // `virt` is mapped exclusively to it by map_page just above.
    unsafe {
        virt_ptr.write(0xC0FF_EE00_1234_5678);
    }
    let via_virt = unsafe { virt_ptr.read() };
    let via_phys = unsafe {
        (mm::phys_to_virt(frame.start_address(), phys_offset).as_u64() as *const u64).read()
    };
    assert_eq!(
        via_virt, 0xC0FF_EE00_1234_5678,
        "read back via virtual page"
    );
    assert_eq!(via_virt, via_phys, "virtual and physical views agree");

    // Translate agrees with the frame we mapped.
    let translated = mm::translate(page.start_address()).expect("translate mapped page");
    assert_eq!(
        translated,
        frame.start_address(),
        "translation returns the mapped frame"
    );

    // Unmap: returns the same frame; access must now fault.
    let unmapped = mm::unmap_page(page).expect("unmap page");
    assert_eq!(unmapped, frame, "unmap returns the mapped frame");

    // From here on, a page fault is the SUCCESS path.
    set_page_fault_handler(fault_test_handler);
    serial_println!("[test-paging] expecting fault on unmapped access");
    // SAFETY: the page was just unmapped; this read must fault.
    let sneaky = unsafe { virt_ptr.read() };
    // Never reached; the handler exits QEMU with Success.
    serial_println!(
        "[test-paging] UNEXPECTED: unmapped read returned {:#x}",
        sneaky
    );
    exit_qemu(QemuExitCode::Failed);
}
