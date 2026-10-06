//! A fresh table copies the kernel root, not the table in CR3.
//!
//! A user page is mapped into one task table, that table is installed,
//! and the next `FreshL4` must not contain the page.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_os::arch::mm;
use galexy_os::{drivers::screen, exit_qemu, println, serial_println, QemuExitCode};
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{
    mapper::MapToError, Mapper, Page, PageTableFlags, PhysFrame, Size4KiB, Translate,
};
use x86_64::VirtAddr;

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

/// Canonical user slot. The kernel table leaves it empty.
const TEST_P4_INDEX: u16 = 100;
const TEST_ADDR: u64 = (TEST_P4_INDEX as u64) << 39;

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-cloneroot] running");
    serial_println!("[test-cloneroot] running");

    galexy_os::arch::mm::init(boot_info);
    galexy_os::arch::init(boot_info);

    let kernel = mm::kernel_cr3();
    assert!(
        mm::translate(VirtAddr::new(TEST_ADDR)).is_none(),
        "the kernel table must not already map the test page"
    );

    let parent = mm::FreshL4::new().expect("parent table");
    let data_frame = mm::allocate_frame().expect("data frame");
    let test_page = Page::<Size4KiB>::containing_address(VirtAddr::new(TEST_ADDR));
    // SAFETY: the parent tree is coherent and not CR3-active yet.
    unsafe {
        mm::with_table(parent.frame, |mapper| {
            let flags = PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::USER_ACCESSIBLE
                | PageTableFlags::NO_EXECUTE;
            let mut alloc = NeverFrameAlloc;
            match mapper.map_to(test_page, data_frame, flags, &mut alloc) {
                Ok(flush) => flush.flush(),
                Err(MapToError::PageAlreadyMapped(_)) => panic!("test page already mapped"),
                Err(e) => panic!("parent map failed: {e:?}"),
            }
        });
    }

    mm::install_cr3(parent.frame);
    let (current, _) = Cr3::read();
    assert_eq!(current, parent.frame, "CR3 must be the parent table");

    let child = mm::FreshL4::new().expect("child table");

    let mut child_sees = None;
    // SAFETY: the child tree is coherent. It is not the active CR3.
    unsafe {
        mm::with_table(child.frame, |mapper| {
            child_sees = mapper.translate_addr(VirtAddr::new(TEST_ADDR));
        });
    }
    assert!(
        child_sees.is_none(),
        "child inherited the parent's user mapping"
    );

    let heap_virt = VirtAddr::new(0x0000_5555_5555_0000);
    let kernel_heap = mm::translate(heap_virt).expect("heap mapped in the kernel table");
    let mut child_heap = None;
    // SAFETY: as above.
    unsafe {
        mm::with_table(child.frame, |mapper| {
            child_heap = mapper.translate_addr(heap_virt);
        });
    }
    assert_eq!(
        child_heap,
        Some(kernel_heap),
        "child must still share the kernel half"
    );

    mm::install_cr3(kernel);
    assert!(mm::on_kernel_tree(), "restored the kernel table");

    serial_println!("[test-cloneroot] passed");
    exit_qemu(QemuExitCode::Success);
}

struct NeverFrameAlloc;

unsafe impl x86_64::structures::paging::FrameAllocator<Size4KiB> for NeverFrameAlloc {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        mm::allocate_frame()
    }
}
