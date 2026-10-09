//! Integration test kernel: virtio-blk DMA stays inside the buffer when
//! the buffer's pages are not physically adjacent.
//!
//! Builds a two-page kernel buffer whose second page is deliberately NOT
//! the frame after the first one, and plants a sentinel in the frame that
//! IS physically next. Then reads and writes sectors that straddle the
//! page boundary. The sentinel must stay untouched and every byte must
//! land where the virtual buffer says — which only holds when the driver
//! emits one descriptor per physically contiguous run.
//!
//! Before the fix the read spilled into the sentinel frame (on a real boot
//! that frame was a page table: `[pf] PAGE FAULT in ring 0` inside galfs
//! `DISK_BUF` whenever KASLR put it across a 2 MiB boundary).

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::drivers::block::SECTOR;
use galexy_os::drivers::virtio_blk;
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};
use x86_64::structures::paging::{Page, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

/// Scratch window in P4 entry 101 (canonical; unused by the kernel).
const WINDOW: u64 = 101 << 39;
/// LBAs beyond anything galfs would touch on the 2048-sector test disk.
const LBA_READ: u32 = 1024;
const LBA_WRITE: u32 = 1100;
/// Sentinel byte in the frame that is physically next to page 0.
const SENTINEL: u8 = 0xEE;

fn pattern(lba: u32, i: usize) -> u8 {
    (lba as usize)
        .wrapping_mul(31)
        .wrapping_add(i.wrapping_mul(7)) as u8
        ^ 0x5A
}

/// `(frame_a, frame_b, other)` with `frame_b` physically right after
/// `frame_a` and `other` any third frame. The allocator is first-fit, so
/// a freed triple would come straight back; hold a batch, pick from it,
/// and return what is not used.
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
        .expect("no two adjacent frames in a 32-frame batch");
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
    // SAFETY: the frame is allocator-owned by this test and only aliased
    // through the physical map here.
    unsafe { core::slice::from_raw_parts_mut(virt.as_mut_ptr::<u8>(), 4096) }
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-dmasplit] running");
    serial_println!("[test-dmasplit] running");

    mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    sched::init();

    assert!(virtio_blk::present(), "virtio-blk must be attached");
    assert!(
        virtio_blk::capacity_sectors() > u64::from(LBA_WRITE) + 64,
        "test disk too small"
    );

    // Page 0 -> frame A, page 1 -> frame OTHER. Frame B (physically after
    // A) holds the sentinel: a single-descriptor DMA past page 0 lands here.
    let (frame_a, frame_b, frame_other) = pick_frames();
    let page0 = Page::<Size4KiB>::containing_address(VirtAddr::new(WINDOW));
    let page1 = Page::<Size4KiB>::containing_address(VirtAddr::new(WINDOW + 4096));
    mm::map_page(page0, frame_a).expect("map page 0");
    mm::map_page(page1, frame_other).expect("map page 1");
    assert_eq!(
        mm::translate(page1.start_address()),
        Some(PhysAddr::new(frame_other.start_address().as_u64())),
        "page 1 must translate to the non-adjacent frame"
    );
    frame_bytes(frame_b).fill(SENTINEL);
    serial_println!(
        "[test-dmasplit] window {:#x}: page0 -> {:#x}, page1 -> {:#x}, sentinel {:#x}",
        WINDOW,
        frame_a.start_address().as_u64(),
        frame_other.start_address().as_u64(),
        frame_b.start_address().as_u64()
    );

    // SAFETY: both pages were mapped above and nothing else uses the window.
    let window = unsafe { core::slice::from_raw_parts_mut(WINDOW as *mut u8, 8192) };
    window.fill(0);

    // Seed LBA_READ.. with a known pattern through an aligned buffer.
    const SEED_SECTORS: usize = 8;
    let mut seed = [[0u8; SECTOR]; SEED_SECTORS];
    for (k, sector) in seed.iter_mut().enumerate() {
        for (i, b) in sector.iter_mut().enumerate() {
            *b = pattern(LBA_READ + k as u32, i);
        }
    }
    virtio_blk::write_sectors(LBA_READ, &seed).expect("seed write");
    // FLUSH is only negotiated on the 1.x path; the data path is what is
    // under test and `cache=writethrough` keeps the host image current.
    let _ = virtio_blk::flush();

    // 1. One sector straddling the boundary: 0x100 bytes on each page.
    let straddle_off = 4096 - 0x100;
    {
        let ptr = window[straddle_off..].as_mut_ptr().cast::<[u8; SECTOR]>();
        // SAFETY: 512 bytes from straddle_off lie inside the 8 KiB window.
        let dst = unsafe { core::slice::from_raw_parts_mut(ptr, 1) };
        virtio_blk::read_sectors(LBA_READ + 2, dst).expect("straddle read");
    }
    for i in 0..SECTOR {
        assert_eq!(
            window[straddle_off + i],
            pattern(LBA_READ + 2, i),
            "straddling sector byte {i} must land in the virtual buffer"
        );
    }
    assert!(
        frame_bytes(frame_b).iter().all(|&b| b == SENTINEL),
        "DMA spilled into the physically adjacent frame"
    );
    serial_println!("[test-dmasplit] straddling read stayed in the buffer");

    // 2. Eight sectors (4 KiB) starting mid-page: two descriptors.
    window.fill(0);
    let multi_off = 0x800;
    {
        let ptr = window[multi_off..].as_mut_ptr().cast::<[u8; SECTOR]>();
        // SAFETY: 4096 bytes from multi_off lie inside the 8 KiB window.
        let dst = unsafe { core::slice::from_raw_parts_mut(ptr, SEED_SECTORS) };
        virtio_blk::read_sectors(LBA_READ, dst).expect("multi read");
    }
    for k in 0..SEED_SECTORS {
        for i in 0..SECTOR {
            assert_eq!(
                window[multi_off + k * SECTOR + i],
                pattern(LBA_READ + k as u32, i),
                "multi-sector read sector {k} byte {i}"
            );
        }
    }
    assert!(
        frame_bytes(frame_b).iter().all(|&b| b == SENTINEL),
        "multi-sector DMA spilled into the physically adjacent frame"
    );
    serial_println!("[test-dmasplit] multi-page read stayed in the buffer");

    // 3. Write direction: the device must source the straddling sector
    // from the virtual buffer, not from the sentinel frame.
    for (i, b) in window[straddle_off..straddle_off + SECTOR]
        .iter_mut()
        .enumerate()
    {
        *b = pattern(LBA_WRITE, i);
    }
    {
        let ptr = window[straddle_off..].as_ptr().cast::<[u8; SECTOR]>();
        // SAFETY: 512 bytes from straddle_off lie inside the 8 KiB window.
        let src = unsafe { core::slice::from_raw_parts(ptr, 1) };
        virtio_blk::write_sectors(LBA_WRITE, src).expect("straddle write");
    }
    let _ = virtio_blk::flush();
    let mut back = [[0u8; SECTOR]; 1];
    virtio_blk::read_sectors(LBA_WRITE, &mut back).expect("read back");
    for (i, &b) in back[0].iter().enumerate() {
        assert_eq!(
            b,
            pattern(LBA_WRITE, i),
            "written sector byte {i} must come from the virtual buffer"
        );
    }
    serial_println!("[test-dmasplit] straddling write came from the buffer");

    // 4. A request longer than the batch cap splits cleanly and reads back.
    let mut big = [[0u8; SECTOR]; 48];
    for (k, sector) in big.iter_mut().enumerate() {
        for (i, b) in sector.iter_mut().enumerate() {
            *b = pattern(LBA_WRITE + 8 + k as u32, i);
        }
    }
    virtio_blk::write_sectors(LBA_WRITE + 8, &big).expect("big write");
    let mut big_back = [[0u8; SECTOR]; 48];
    virtio_blk::read_sectors(LBA_WRITE + 8, &mut big_back).expect("big read");
    assert!(
        big.iter().zip(big_back.iter()).all(|(a, b)| a == b),
        "48-sector round trip"
    );
    serial_println!("[test-dmasplit] 48-sector round trip");

    println!("[test-dmasplit] passed");
    serial_println!("[test-dmasplit] passed");
    exit_qemu(QemuExitCode::Success);
}
