//! Integration test kernel: screen rendering — glyph ink reaches full
//! foreground brightness (per-glyph normalization) and backspace truly
//! erases (cell cleared to black, verified by reading the framebuffer).

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{drivers::screen, exit_qemu, println, serial_println, QemuExitCode};
use noto_sans_mono_bitmap::{get_raster, FontWeight, RasterHeight};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

/// Max channel brightness found in one cell region of the framebuffer.
///
/// # Safety
///
/// The framebuffer address/layout must come from `screen`'s accessors while
/// the screen is idle (single-threaded test).
unsafe fn cell_max_intensity(
    addr: u64,
    info: bootloader_api::info::FrameBufferInfo,
    cell_x: usize,
    cell_y: usize,
) -> u8 {
    let char_width = get_raster('A', FontWeight::Regular, RasterHeight::Size16)
        .map(|r| r.width())
        .unwrap_or(8);
    let line_height = RasterHeight::Size16.val();
    let bpp = info.bytes_per_pixel;
    let x0 = cell_x * char_width;
    let y0 = cell_y * line_height;

    let fb = unsafe {
        core::slice::from_raw_parts(
            core::ptr::with_exposed_provenance(addr as usize),
            info.byte_len,
        )
    };
    let mut max = 0u8;
    for row in 0..line_height {
        for col in 0..char_width {
            let idx = ((y0 + row) * info.stride + x0 + col) * bpp;
            for &b in &fb[idx..idx + bpp] {
                if b > max {
                    max = b;
                }
            }
        }
    }
    max
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-screen] running");
    serial_println!("[test-screen] running");

    let info = screen::framebuffer_info().expect("framebuffer initialized");
    let addr = screen::framebuffer_addr().expect("framebuffer initialized");

    // Print "AB" on the CURRENT line (line 1: line 0 holds the banner
    // print), then erase the 'B' via backspace.
    screen::out_str("AB");
    screen::backspace(); // cursor was at cell 2 -> back to 1, clear cell

    // SAFETY: screen is idle; cells below are exactly the ones just drawn.
    let ink_a = unsafe { cell_max_intensity(addr, info, 0, 1) };
    let erased_b = unsafe { cell_max_intensity(addr, info, 1, 1) };

    assert!(
        ink_a >= 200,
        "glyph 'A' ink too dim: {ink_a} (normalization should reach full fg color)"
    );
    assert_eq!(erased_b, 0, "backspaced cell must be fully black");

    println!("[test-screen] ink={} erased={}", ink_a, erased_b);
    serial_println!("[test-screen] passed");
    exit_qemu(QemuExitCode::Success);
}
