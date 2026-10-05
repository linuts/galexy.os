//! Integration test kernel: fresh page-table trees (ring-3 isolation
//! groundwork, roadmap Step B). Proves:
//! 1. `FreshL4` copies the active table (kernel higher half reachable).
//! 2. The fresh table's recursive entry self-points at the fresh frame.
//! 3. Mappings made into the fresh tree via `with_table` are invisible to
//!    the active tree (independence).
//! 4. Translation through the fresh tree matches the active tree for kernel
//!    addresses (shared higher half).

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_os::arch::mm;
use galexy_os::{drivers::screen, exit_qemu, println, serial_println, QemuExitCode};
use x86_64::structures::paging::{
    mapper::MapToError, Mapper, OffsetPageTable, Page, PageTableFlags, PhysFrame, Size4KiB,
    Translate,
};
use x86_64::VirtAddr;

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

/// Test virtual area: P4 entry 100 (canonical, unused — see docs/DESIGN.md).
const TEST_P4_INDEX: u16 = 100;
const TEST_ADDR: u64 = (TEST_P4_INDEX as u64) << 39;

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-freshl4] running");
    serial_println!("[test-freshl4] running");

    galexy_os::arch::init(boot_info);
    galexy_os::arch::mm::init(boot_info);

    // 1+2: fresh tree + self-pointing recursive entry.
    let fresh = mm::FreshL4::new().expect("fresh L4 allocation");
    let fresh_phys = fresh.frame.start_address().as_u64();
    let entry511 = unsafe {
        let table: *const x86_64::structures::paging::PageTable = fresh.virt().as_ptr();
        // Entry storage: PageTable is an array of entries starting at the
        // table pointer itself (511 * 8 bytes offset).
        let entry_ptr: *const x86_64::structures::paging::page_table::PageTableEntry =
            table.byte_add(511 * 8).cast();
        core::ptr::read(entry_ptr)
    };
    assert_eq!(
        entry511.addr().as_u64(),
        fresh_phys,
        "fresh L4[511] must self-point at the fresh frame"
    );
    assert!(
        entry511.flags().contains(PageTableFlags::PRESENT),
        "fresh L4[511] must be present"
    );

    // 4: kernel higher half is shared — heap start translates identically in
    // both trees.
    let heap_virt = VirtAddr::new(0x0000_5555_5555_0000);
    let active_translation = mm::translate(heap_virt).expect("heap mapped in active tree");
    let mut fresh_translation = None;
    // SAFETY: the fresh tree is coherent (freshly cloned) and not CR3-active.
    unsafe {
        mm::with_table(fresh.frame, |mapper: &mut OffsetPageTable<'static>| {
            fresh_translation = mapper.translate_addr(heap_virt);
        });
    }
    assert_eq!(
        fresh_translation,
        Some(active_translation),
        "kernel higher half must translate identically in the fresh tree"
    );

    // 3: map a page ONLY into the fresh tree; the active tree must not see it.
    let test_page = Page::<Size4KiB>::containing_address(VirtAddr::new(TEST_ADDR));
    let data_frame = mm::allocate_frame().expect("data frame");
    // SAFETY: fresh tree, not active.
    unsafe {
        mm::with_table(fresh.frame, |mapper| {
            let flags = PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::USER_ACCESSIBLE
                | PageTableFlags::NO_EXECUTE;
            let mut alloc = NeverFrameAlloc;
            match mapper.map_to(test_page, data_frame, flags, &mut alloc) {
                Ok(flush) => flush.flush(),
                Err(MapToError::PageAlreadyMapped(_)) => panic!("test page already mapped"),
                Err(e) => panic!("fresh-tree map failed: {e:?}"),
            }
        });
    }
    // Active tree: must NOT know this page.
    assert!(
        mm::translate(VirtAddr::new(TEST_ADDR)).is_none(),
        "fresh-tree mapping leaked into the active tree"
    );
    // Fresh tree: must know it.
    let mut fresh_sees = None;
    // SAFETY: fresh tree, not active.
    unsafe {
        mm::with_table(fresh.frame, |mapper| {
            fresh_sees = mapper.translate_addr(VirtAddr::new(TEST_ADDR));
        });
    }
    assert_eq!(
        fresh_sees,
        Some(data_frame.start_address()),
        "fresh tree must translate the user page to the data frame"
    );

    println!("[test-freshl4] fresh tree cloned, self-recursive, independent");
    println!("[test-freshl4] all checks passed");
    serial_println!("[test-freshl4] passed");
    exit_qemu(QemuExitCode::Success);
}

/// Frame allocator adapter for maps into non-active trees: the trait impl in
/// the paging module is private, so the test kernel carries its own.
struct NeverFrameAlloc;

unsafe impl x86_64::structures::paging::FrameAllocator<Size4KiB> for NeverFrameAlloc {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        mm::allocate_frame()
    }
}
