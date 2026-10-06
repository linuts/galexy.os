//! Integration test kernel: screen rendering — glyph ink reaches full
//! foreground brightness (per-glyph normalization), backspace truly erases,
//! and scrolling shifts by exactly one line (stride pixels-vs-bytes
//! regression guard).

#![no_std]
#![no_main]

use bootloader_api::info::PixelFormat;
use bootloader_api::{entry_point, BootInfo};
use galexy_os::drivers::console;
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

/// Max value of one byte inside each pixel (0 = first byte of the pixel).
///
/// # Safety
///
/// Same contract as [`region_max_intensity`].
unsafe fn region_byte_max(
    addr: u64,
    info: bootloader_api::info::FrameBufferInfo,
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    byte_in_pixel: usize,
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
            let idx = ((y0 + row) * info.stride + x0 + col) * bpp + byte_in_pixel;
            if fb[idx] > max {
                max = fb[idx];
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

    /* --- scroll: text moves, the status row stays --- */

    // One glyph at the top, a marker on the status row, then exactly one
    // scroll of the text area (the newline that lands past the last text
    // row). The status row is not part of that shift.
    let (rows, _) = screen::terminal_size().expect("terminal size");
    assert!(rows > 1, "the status row needs a row of its own");
    let text_rows = rows - 1;
    screen::clear_screen();
    screen::set_pos(0, rows - 1);
    screen::out_plain("S");
    screen::set_pos(0, 0);
    screen::out_str("A");
    for _ in 0..text_rows {
        screen::out_char('\n');
    }

    let top_band = unsafe { region_max_intensity(addr, info, 0, 0, info.width, LINE_HEIGHT) };
    assert_eq!(top_band, 0, "top band must be ink-free after one scroll");

    let last_text = (text_rows - 1) * LINE_HEIGHT;
    let text_bottom =
        unsafe { region_max_intensity(addr, info, 0, last_text, info.width, LINE_HEIGHT) };
    assert_eq!(
        text_bottom, 0,
        "last text row must be cleared by the scroll"
    );

    let status_y = (rows - 1) * LINE_HEIGHT;
    let status = unsafe { region_max_intensity(addr, info, 0, status_y, CHAR_WIDTH, 16) };
    assert!(
        status >= 200,
        "status-row marker scrolled away (intensity {status})"
    );

    /* --- tab, CR, CSI color, erase leaves the status row --- */

    screen::clear_screen();
    console::out_str("\tX");
    let tab_x = 8 * CHAR_WIDTH;
    let tabbed = unsafe { region_max_intensity(addr, info, tab_x, 0, CHAR_WIDTH, 16) };
    let before_tab = unsafe { region_max_intensity(addr, info, 0, 0, CHAR_WIDTH, 16) };
    assert!(
        tabbed >= 200,
        "tab did not land on column 8 (intensity {tabbed})"
    );
    assert_eq!(before_tab, 0, "columns before the tab stop must stay blank");

    screen::clear_screen();
    console::out_str("AB\rC");
    let cr_c = unsafe { region_max_intensity(addr, info, 0, 0, CHAR_WIDTH, 16) };
    let cr_b = unsafe { region_max_intensity(addr, info, CHAR_WIDTH, 0, CHAR_WIDTH, 16) };
    assert!(cr_c >= 200, "CR did not redraw column 0");
    assert!(cr_b >= 200, "CR cleared the rest of the line");

    screen::clear_screen();
    screen::set_pos(0, rows - 1);
    screen::out_plain("S");
    screen::set_pos(0, 0);
    // Raw bytes also go to COM1; the harness checks this string.
    console::out_str("\u{1b}[31mZ\u{1b}[2J");
    let cleared = unsafe { region_max_intensity(addr, info, 0, 0, CHAR_WIDTH, 16) };
    assert_eq!(cleared, 0, "CSI 2J must clear the text area");
    let status_after = unsafe { region_max_intensity(addr, info, 0, status_y, CHAR_WIDTH, 16) };
    assert!(
        status_after >= 200,
        "CSI 2J cleared the status row (intensity {status_after})"
    );

    screen::clear_screen();
    console::out_str("\u{1b}[31mZ");
    let (red_i, green_i) = match info.pixel_format {
        PixelFormat::Rgb => (0, 1),
        PixelFormat::Bgr => (2, 1),
        other => panic!("color test needs RGB or BGR pixels, got {other:?}"),
    };
    let red = unsafe { region_byte_max(addr, info, 0, 0, CHAR_WIDTH, 16, red_i) };
    let green = unsafe { region_byte_max(addr, info, 0, 0, CHAR_WIDTH, 16, green_i) };
    assert!(red >= 200, "SGR 31 red channel too dim: {red}");
    assert!(
        green < 80,
        "SGR 31 should not light the green channel ({green})"
    );

    screen::clear_screen();
    console::out_str("\u{1b}[2;3HQ");
    let cup =
        unsafe { region_max_intensity(addr, info, 2 * CHAR_WIDTH, LINE_HEIGHT, CHAR_WIDTH, 16) };
    let home = unsafe { region_max_intensity(addr, info, 0, 0, CHAR_WIDTH, 16) };
    assert!(
        cup >= 200,
        "CSI H did not place the glyph (intensity {cup})"
    );
    assert_eq!(home, 0, "CSI H left ink at the home cell");

    /* --- F2 hides tty 0; F1 paints the saved cells, including a write that
    landed while it was hidden --- */

    screen::clear_screen();
    console::out_str("A");
    let tty0 = unsafe { region_max_intensity(addr, info, 0, 0, CHAR_WIDTH, 16) };
    assert!(tty0 >= 200, "tty 0 glyph missing (intensity {tty0})");

    // Scancode set 1: F2 make 0x3C, break 0xBC. The IRQ only records the
    // switch; the main loop's apply paints it.
    use galexy_os::drivers::keyboard;
    keyboard::add_scancode(0x3C);
    keyboard::add_scancode(0xBC);
    screen::apply_tty_switch();
    let hidden = unsafe { region_max_intensity(addr, info, 0, 0, CHAR_WIDTH, 16) };
    assert_eq!(hidden, 0, "F2 still shows tty 0");

    screen::out_str_tty(0, "B");
    let still = unsafe { region_max_intensity(addr, info, CHAR_WIDTH, 0, CHAR_WIDTH, 16) };
    assert_eq!(still, 0, "a background write painted the visible screen");

    keyboard::add_scancode(0x3B);
    keyboard::add_scancode(0xBB);
    screen::apply_tty_switch();
    let restored = unsafe { region_max_intensity(addr, info, 0, 0, CHAR_WIDTH, 16) };
    assert!(
        restored >= 200,
        "F1 did not restore tty 0 (intensity {restored})"
    );
    let kept = unsafe { region_max_intensity(addr, info, CHAR_WIDTH, 0, CHAR_WIDTH, 16) };
    assert!(
        kept >= 200,
        "the background write was not kept (intensity {kept})"
    );

    println!("[test-screen] ink={} erased={} scroll-ok", ink_a, erased_b);
    serial_println!("[test-screen] passed");
    exit_qemu(QemuExitCode::Success);
}
