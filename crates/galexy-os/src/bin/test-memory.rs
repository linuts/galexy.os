//! Integration test kernel: exercises the physical frame allocator end to
//! end — allocate, write/read back through the physical-memory mapping,
//! deallocate, and verify first-fit reuse.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{arch::mm, drivers::screen, println, serial_println};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-memory] running");
    serial_println!("[test-memory] running");

    let phys_offset = boot_info
        .physical_memory_offset
        .into_option()
        .expect("physical memory must be mapped (see BOOTLOADER_CONFIG)");

    // Fault handlers BEFORE memory work: a fault reports over serial
    // instead of triple-faulting into a silent reset.
    galexy_os::arch::init();

    mm::init(boot_info);
    let initial_free = mm::free_frames();
    assert!(
        initial_free > 100,
        "expected a meaningful number of free frames"
    );

    // Allocate two frames, write patterns through the physical mapping.
    let frame_a = mm::allocate_frame().expect("frame A alloc");
    let frame_b = mm::allocate_frame().expect("frame B alloc");
    assert_ne!(frame_a, frame_b, "distinct frames");
    assert_eq!(mm::free_frames(), initial_free - 2);

    let addr_a = mm::phys_to_virt(frame_a.start_address(), phys_offset);
    let addr_b = mm::phys_to_virt(frame_b.start_address(), phys_offset);
    // SAFETY: allocated frames are Usable and otherwise unmapped; exclusive
    // access is guaranteed by the allocator contract.
    unsafe {
        (addr_a as *mut u64).write(0x1111_2222_3333_4444);
        (addr_b as *mut u64).write(0xaaaa_bbbb_cccc_dddd);
    }
    // SAFETY: same frames, read back what we wrote.
    let (read_a, read_b) =
        unsafe { ((addr_a as *const u64).read(), (addr_b as *const u64).read()) };
    assert_eq!(read_a, 0x1111_2222_3333_4444, "frame A roundtrip");
    assert_eq!(read_b, 0xaaaa_bbbb_cccc_dddd, "frame B roundtrip");

    // Deallocate A and B; first-fit must hand them back in order.
    mm::deallocate_frame(frame_a);
    mm::deallocate_frame(frame_b);
    assert_eq!(mm::free_frames(), initial_free, "free count restored");

    let reuse_a = mm::allocate_frame().expect("reuse alloc");
    assert_eq!(reuse_a, frame_a, "first-fit reuses frame A");
    mm::deallocate_frame(reuse_a);

    serial_println!("[test-memory] passed");

    // Double-free must panic (would exit Failed otherwise); the marker
    // matches the panic location (the assert lives in arch/mm.rs — note:
    // assert! payloads are fmt::Arguments, not str, so we match the file).
    galexy_os::expect_panic("arch/mm.rs");
    mm::deallocate_frame(frame_b);
    unreachable!("double free should have panicked");
}
