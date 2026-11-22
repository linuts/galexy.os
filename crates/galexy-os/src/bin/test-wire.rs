//! Virtio-net: one descriptor per physical run, and only `net` holds the wire.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{CapRights, SysError, Syscall};
use galexy_os::arch::mm;
use galexy_os::drivers::virtio_net;
use galexy_os::{drivers::screen, exit_qemu, serial_println, QemuExitCode};
use x86_64::structures::paging::{Page, PhysFrame, Size4KiB};
use x86_64::VirtAddr;

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const WINDOW: u64 = 101 << 39;
const SENTINEL: u8 = 0xEE;
const MARK: u32 = 0xD00D;

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    galexy_os::sched::init();
    serial_println!("[test-wire] running");

    assert!(virtio_net::present(), "virtio-net must be attached");
    let mac = virtio_net::mac();
    assert_eq!(mac, [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    serial_println!(
        "[test-wire] mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5]
    );

    let (frame_a, frame_b, frame_other) = pick_frames();
    let page0 = Page::<Size4KiB>::containing_address(VirtAddr::new(WINDOW));
    let page1 = Page::<Size4KiB>::containing_address(VirtAddr::new(WINDOW + 4096));
    mm::map_page(page0, frame_a).expect("map page 0");
    mm::map_page(page1, frame_other).expect("map page 1");
    frame_bytes(frame_b).fill(SENTINEL);
    // SAFETY: both pages were mapped above.
    let window = unsafe { core::slice::from_raw_parts_mut(WINDOW as *mut u8, 8192) };
    let start = 4096 - 30;
    for (i, byte) in window[start..start + 60].iter_mut().enumerate() {
        *byte = 0x40 + (i as u8);
    }
    let irqs = virtio_net::irq_count();
    virtio_net::transmit(&window[start..start + 60]).expect("transmit");
    assert!(
        virtio_net::last_tx_data_descs() >= 2,
        "a frame that straddles two frames needs two data descriptors, got {}",
        virtio_net::last_tx_data_descs()
    );
    assert!(
        frame_bytes(frame_b).iter().all(|b| *b == SENTINEL),
        "transmit must not touch the frame physically next to page 0"
    );
    assert!(virtio_net::irq_count() > irqs, "transmit must complete");
    serial_println!("[test-wire] split descriptors");

    let cap = galexy_abi::reserved::wire(CapRights::WRITE);
    let (region, _) = galexy_os::sched::spawn_user_task_on("shell", 0, |gr| {
        let mut code = alloc::vec::Vec::new();
        code.extend_from_slice(&[0x48, 0xBF]);
        code.extend_from_slice(&cap.bits().to_le_bytes());
        code.extend_from_slice(&[0xB8]);
        code.extend_from_slice(&(Syscall::Write as u32).to_le_bytes());
        code.extend_from_slice(&[0xBA, 0, 0, 0, 0]);
        code.extend_from_slice(&[0x0F, 0x05]);
        code.extend_from_slice(&[0x48, 0xBF]);
        code.extend_from_slice(&gr.scratch.as_u64().to_le_bytes());
        code.extend_from_slice(&[0x89, 0x07]);
        code.extend_from_slice(&[0xB8]);
        code.extend_from_slice(&MARK.to_le_bytes());
        code.extend_from_slice(&[0x89, 0x47, 0x04]);
        code.extend_from_slice(&[0xB8]);
        code.extend_from_slice(&(Syscall::Exit as u32).to_le_bytes());
        code.extend_from_slice(&[0x0F, 0x05]);
        code
    });
    let scratch = mm::frame_virt(region.scratch_phys).as_u64() as *const u32;
    loop {
        x86_64::instructions::hlt();
        // SAFETY: scratch stays mapped until this loop observes the mark.
        let mark = unsafe { core::ptr::read_volatile(scratch.add(1)) };
        if mark == MARK {
            break;
        }
        galexy_os::sched::reap();
    }
    let code = unsafe { core::ptr::read_volatile(scratch) };
    assert_eq!(code, SysError::AccessDenied as u32);
    serial_println!("[test-wire] other task denied");
    loop {
        x86_64::instructions::hlt();
        galexy_os::sched::reap();
        if galexy_os::sched::unreaped_threads() == 0 {
            break;
        }
    }
    serial_println!("[test-wire] passed");
    exit_qemu(QemuExitCode::Success);
}

fn pick_frames() -> (
    PhysFrame<Size4KiB>,
    PhysFrame<Size4KiB>,
    PhysFrame<Size4KiB>,
) {
    const BATCH: usize = 32;
    let mut frames = [None; BATCH];
    for slot in frames.iter_mut() {
        *slot = Some(mm::allocate_frame().expect("batch frame"));
    }
    let frames = frames.map(|f| f.expect("allocated"));
    let pair = (0..BATCH - 1)
        .find(|&i| frames[i + 1].start_address() == frames[i].start_address() + 4096u64)
        .expect("adjacent frames");
    let other = (0..BATCH)
        .find(|&i| i != pair && i != pair + 1)
        .expect("third frame");
    for (i, frame) in frames.iter().enumerate() {
        if i != pair && i != pair + 1 && i != other {
            mm::deallocate_frame(*frame);
        }
    }
    (frames[pair], frames[pair + 1], frames[other])
}

fn frame_bytes(frame: PhysFrame<Size4KiB>) -> &'static mut [u8] {
    let virt = mm::frame_virt(frame.start_address());
    // SAFETY: the frame is held by this test.
    unsafe { core::slice::from_raw_parts_mut(virt.as_mut_ptr::<u8>(), 4096) }
}
