//! Integration test kernel: screen rendering — glyph ink reaches full
//! foreground brightness (per-glyph normalization), backspace truly erases,
//! and scrolling shifts by exactly one line (stride pixels-vs-bytes
//! regression guard).

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{drivers::screen, exit_qemu, println, serial_println, QemuExitCode};
use noto_sans_mono_bitmap::RasterHeight;

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const CHAR_WIDTH: usize = 7;
const LINE_HEIGHT: usize = RasterHeight::Size16.val() + 2; // must match screen.rs

/// Max channel brightness in a pixel region of the framebuffer.
///
/// # Safety
///
/// The framebuffer address/layout must come from `screen`'s accessors while
/// the screen is idle (single-threaded test).
unsafe fn region_max_intensity(
    addr: u64,
    info: bootloader_api::info::FrameBufferInfo,
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
) -> u8 {
    let bpp = info.bytes_per_pixel;
    let fb = unsafe {
        core::slice::from_raw_parts(
            core::ptr::with_exposed_provenance(addr as usize),
            info.byte_len,
        )
    };
    let mut max = 0u8;
    for row in 0..h {
        for col in 0..w {
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

    /* --- ink + backspace --- */

    // Print "AB" on the CURRENT line (line 1: line 0 holds the banner
    // print), then erase the 'B' via backspace.
    screen::out_str("AB");
    screen::backspace(); // cursor was at cell 2 -> back to 1, clear cell

    // SAFETY: screen is idle; regions below are exactly the ones touched.
    let ink_a = unsafe { region_max_intensity(addr, info, 0, LINE_HEIGHT, CHAR_WIDTH, 16) };
    let erased_b =
        unsafe { region_max_intensity(addr, info, CHAR_WIDTH, LINE_HEIGHT, CHAR_WIDTH, 16) };

    assert!(
        ink_a >= 200,
        "glyph 'A' ink too dim: {ink_a} (normalization should reach full fg color)"
    );
    assert_eq!(erased_b, 0, "backspaced cell must be fully black");

    /* --- scroll regression (stride is pixels, not bytes) --- */

    // Fresh screen, one glyph at the top, then EXACTLY one scroll
    // (39 plain newlines move to the bottom line, the 40th scrolls).
    screen::clear_screen();
    screen::out_str("A");
    for _ in 0..40 {
        screen::out_char('\n');
    }

    // With the fix: the 'A' is fully scrolled off (top band clean) and the
    // bottom line's pixels were cleared. With the old bug (shift by 6 rows
    // instead of 18): 'A' residue remains in the top band and old content
    // is left at the bottom.
    let top_band = unsafe { region_max_intensity(addr, info, 0, 0, info.width, LINE_HEIGHT) };
    assert_eq!(top_band, 0, "top band must be ink-free after one scroll");

    let last_row = (info.height / LINE_HEIGHT - 1) * LINE_HEIGHT;
    let bottom_band =
        unsafe { region_max_intensity(addr, info, 0, last_row, info.width, LINE_HEIGHT) };
    assert_eq!(
        bottom_band, 0,
        "bottom band must be fully cleared after scroll"
    );

    println!("[test-screen] ink={} erased={} scroll-ok", ink_a, erased_b);
    serial_println!("[test-screen] passed");
    exit_qemu(QemuExitCode::Success);
}
