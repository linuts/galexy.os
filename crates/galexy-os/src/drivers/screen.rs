//! The galexy.os "screen": text rendering on the pixel framebuffer handed to
//! us by the bootloader.
//!
//! The bootloader maps the framebuffer into our address space and reports its
//! location/layout via `BootInfo::framebuffer`. Since bootloader v0.11 there
//! is no legacy VGA text mode, so we render bitmap glyphs ourselves.
//!
//! Public surface stays terminal-shaped (`out_char`, `out_str`, `clear_screen`,
//! `set_color`), so callers don't care about pixels or fonts. Must be
//! [`init`](self::init)ed with the `BootInfo` framebuffer before output.

use core::fmt;
use spin::Mutex;

use noto_sans_mono_bitmap::{get_raster, FontWeight, RasterHeight};

/// Glyph raster used for all text.
const FONT_WEIGHT: FontWeight = FontWeight::Regular;
/// Glyph raster height in pixels.
const FONT_HEIGHT: RasterHeight = RasterHeight::Size16;
/// Vertical padding between text lines, in pixels.
const LINE_SPACING: usize = 2;
/// Advance of one text line, in pixels.
const LINE_HEIGHT: usize = FONT_HEIGHT.val() + LINE_SPACING;

/// A 24-bit RGB color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

impl Color {
    /// Creates a color from channel values.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Color { r, g, b }
    }

    /// Scales the color by `intensity`/255 (anti-aliased glyph coverage).
    const fn scaled(self, intensity: u8) -> Self {
        Color {
            r: ((self.r as u16 * intensity as u16) / 255) as u8,
            g: ((self.g as u16 * intensity as u16) / 255) as u8,
            b: ((self.b as u16 * intensity as u16) / 255) as u8,
        }
    }
}

/// Snapshot of the framebuffer, taken from `BootInfo`.
struct FramebufferSpec {
    /// The framebuffer bytes; lives for the whole kernel run.
    buffer: &'static mut [u8],
    /// Layout of the framebuffer.
    info: bootloader_api::info::FrameBufferInfo,
}

/// Terminal state + framebuffer handle, guarded by the global lock.
struct ScreenWriter {
    fb: FramebufferSpec,
    /// Glyph advance width in pixels (mono font: same for all glyphs).
    char_width: usize,
    /// Cursor position, in characters.
    char_x: usize,
    char_y: usize,
    fg: Color,
}

impl ScreenWriter {
    /// Writes one glyph, advancing the cursor.
    fn write_char(&mut self, c: char) {
        match c {
            '\n' => self.new_line(),
            c => {
                if self.char_x >= self.max_char_x() {
                    self.new_line();
                }
                if self.char_y < self.max_char_y() {
                    self.draw_glyph(c);
                }
                self.char_x += 1;
            }
        }
    }

    /// Renders one glyph at the cursor.
    ///
    /// Glyph rasters at this size have thin strokes whose pixels never reach
    /// full intensity (measured: glyph 'a' peaks at 223/255), so intensities
    /// are normalized per glyph to the raster's peak — glyph cores render at
    /// full fg color instead of a washed-out fraction of it.
    fn draw_glyph(&mut self, c: char) {
        let raster = match get_raster(c, FONT_WEIGHT, FONT_HEIGHT) {
            Some(raster) => raster,
            // Unknown glyph (or unsupported char): draw a blank.
            None => match get_raster(' ', FONT_WEIGHT, FONT_HEIGHT) {
                Some(raster) => raster,
                None => return,
            },
        };
        let peak = raster
            .raster()
            .iter()
            .flat_map(|line| line.iter().copied())
            .max()
            .unwrap_or(0);
        if peak == 0 {
            return; // blank glyph (e.g. space)
        }

        let x_origin = self.char_x * self.char_width;
        let y_origin = self.char_y * LINE_HEIGHT;

        // SAFETY: single access at a time, enforced by the global lock.
        let buffer = &mut *self.fb.buffer;
        let info = self.fb.info;

        for (row, line) in raster.raster().iter().enumerate() {
            for (col, &intensity) in line.iter().enumerate() {
                if intensity == 0 {
                    continue;
                }
                let x = x_origin + col;
                let y = y_origin + row;
                if x >= info.width || y >= info.height {
                    continue;
                }
                // Normalize to the raster's own peak: cores → full color.
                let boosted = ((intensity as u32 * 255) / peak as u32).min(255) as u8;
                let pixel = self.fg.scaled(boosted);
                let idx = (y * info.stride + x) * info.bytes_per_pixel;
                Self::write_pixel(buffer, idx, info, pixel);
            }
        }
    }

    /// Writes one RGB pixel at byte offset `idx` in the framebuffer,
    /// respecting the reported pixel format.
    fn write_pixel(
        buffer: &mut [u8],
        idx: usize,
        info: bootloader_api::info::FrameBufferInfo,
        pixel: Color,
    ) {
        match info.pixel_format {
            bootloader_api::info::PixelFormat::Rgb => {
                buffer[idx] = pixel.r;
                buffer[idx + 1] = pixel.g;
                buffer[idx + 2] = pixel.b;
            }
            bootloader_api::info::PixelFormat::Bgr => {
                buffer[idx] = pixel.b;
                buffer[idx + 1] = pixel.g;
                buffer[idx + 2] = pixel.r;
            }
            bootloader_api::info::PixelFormat::U8 => {
                let lum = (pixel.r as u16 + pixel.g as u16 + pixel.b as u16) / 3;
                buffer[idx] = lum as u8;
            }
            bootloader_api::info::PixelFormat::Unknown {
                red_position,
                green_position,
                blue_position,
            } => {
                buffer[idx + (red_position / 8) as usize] = pixel.r;
                buffer[idx + (green_position / 8) as usize] = pixel.g;
                buffer[idx + (blue_position / 8) as usize] = pixel.b;
            }
            _ => {}
        }
    }

    /// Clears one character cell to background (black) — used by backspace,
    /// because drawing a space glyph writes nothing (zero-intensity skip).
    fn clear_cell(&mut self) {
        let x_origin = self.char_x * self.char_width;
        let y_origin = self.char_y * LINE_HEIGHT;

        // SAFETY: single access at a time, enforced by the global lock.
        let buffer = &mut *self.fb.buffer;
        let info = self.fb.info;
        let bpp = info.bytes_per_pixel;

        for row in 0..FONT_HEIGHT.val() {
            let y = y_origin + row;
            if y >= info.height {
                break;
            }
            for col in 0..self.char_width {
                let x = x_origin + col;
                if x >= info.width {
                    break;
                }
                let idx = (y * info.stride + x) * bpp;
                for byte in &mut buffer[idx..idx + bpp] {
                    *byte = 0;
                }
            }
        }
    }

    /// Moves to the next line, scrolling the whole framebuffer up when the
    /// bottom is reached.
    fn new_line(&mut self) {
        if self.char_y + 1 >= self.max_char_y() {
            self.scroll_up();
        } else {
            self.char_y += 1;
        }
        self.char_x = 0;
    }

    /// Erases the glyph left of the cursor, moving the cursor back one cell.
    /// Does not cross line boundaries (backspace is line-local by design).
    fn backspace(&mut self) {
        if self.char_x == 0 {
            return;
        }
        self.char_x -= 1;
        self.clear_cell();
    }

    /// Shifts all pixels up by one line height and clears the last line.
    ///
    /// NOTE: `stride` is in PIXELS (bootloader doc), not bytes — the shift
    /// amount must include `bytes_per_pixel`. (This used to be the overlap
    /// bug: each scroll moved 6 pixel-rows instead of 18.)
    fn scroll_up(&mut self) {
        let buffer = &mut *self.fb.buffer;
        let info = self.fb.info;

        let line_bytes = LINE_HEIGHT * info.stride * info.bytes_per_pixel;
        if line_bytes < buffer.len() {
            buffer.copy_within(line_bytes.., 0);
        }
        // Clear the freed lines at the bottom.
        let tail_start = buffer.len().saturating_sub(line_bytes);
        for byte in &mut buffer[tail_start..] {
            *byte = 0;
        }
    }

    /// Fills one text line's pixels with a solid color, cursor to its start.
    fn fill_row(&mut self, row: usize) {
        self.clear_row_pixels(row);
        self.char_x = 0;
        self.char_y = row;
    }

    /// Paints all pixels of one text line's slot with the current fg color.
    fn clear_row_pixels(&mut self, row: usize) {
        let buffer = &mut *self.fb.buffer;
        let info = self.fb.info;
        let bpp = info.bytes_per_pixel;
        let y0 = row * LINE_HEIGHT;
        for row_px in 0..LINE_HEIGHT {
            let y = y0 + row_px;
            if y >= info.height {
                break;
            }
            for x in 0..info.width {
                let idx = (y * info.stride + x) * bpp;
                Self::write_pixel(buffer, idx, info, self.fg);
            }
        }
    }

    /// Terminal width in characters.
    fn max_char_x(&self) -> usize {
        self.fb.info.width / self.char_width
    }

    /// Terminal height in characters.
    fn max_char_y(&self) -> usize {
        self.fb.info.height / LINE_HEIGHT
    }

    /// Fills the whole framebuffer with black and resets the cursor.
    fn clear(&mut self) {
        let buffer = &mut *self.fb.buffer;
        let info = self.fb.info;
        for pixel in buffer.chunks_exact_mut(info.bytes_per_pixel) {
            for byte in pixel {
                *byte = 0;
            }
        }
        self.char_x = 0;
        self.char_y = 0;
    }
}

impl fmt::Write for ScreenWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            self.write_char(c);
        }
        Ok(())
    }
}

/// The global screen, uninitialized until [`init`] is called.
static SCREEN: Mutex<Option<ScreenWriter>> = Mutex::new(None);

// SAFETY: the framebuffer is device memory; all concurrent access is
// serialized through the `SCREEN` lock, and the buffer outlives everything.
unsafe impl Send for ScreenWriter {}
unsafe impl Sync for ScreenWriter {}

/// Hooks the framebuffer reported by the bootloader into the screen module.
///
/// Copies the framebuffer location/layout out of `boot_info`; later output
/// goes to pixels.
pub fn init(boot_info: &mut bootloader_api::info::BootInfo) {
    let Some(fb) = boot_info.framebuffer.take() else {
        return;
    };
    // Mono font: every glyph has the same advance width.
    let char_width = get_raster('A', FONT_WEIGHT, FONT_HEIGHT)
        .map(|raster| raster.width())
        .unwrap_or(8);

    let info = fb.info();
    let buffer = fb.into_buffer();
    // Geometry invariant (scroll math depends on it): the reported byte
    // length must exactly cover height rows of stride pixels.
    assert_eq!(
        buffer.len(),
        info.height * info.stride * info.bytes_per_pixel,
        "framebuffer geometry mismatch (byte_len vs height*stride*bpp)"
    );

    let mut writer = ScreenWriter {
        fb: FramebufferSpec { buffer, info },
        char_width,
        char_x: 0,
        char_y: 0,
        fg: Color::new(0xE0, 0xE0, 0xE0),
    };
    writer.clear();
    *SCREEN.lock() = Some(writer);
}

/// Runs `f` with exclusive access to the screen, if initialized.
fn with_lock<F>(f: F)
where
    F: FnOnce(&mut ScreenWriter),
{
    // Lock-audit rule (docs/DESIGN.md): the screen lock is reached from
    // preemptable code (main loop shell output) AND IRQ-context code (a
    // user task's write syscall runs at IF=0) — the gate lives here, in
    // the module's lock-taking core, so no holder can ever be preempted
    // mid-hold (a held lock + IF=0 syscalls would wedge the timer).
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut guard = SCREEN.lock();
        if let Some(writer) = guard.as_mut() {
            f(writer);
        }
    });
}

/// Current cursor position, in character cells.
pub fn pos() -> (usize, usize) {
    let mut pos = (0, 0);
    // Same gate as the writers (shared lock).
    x86_64::instructions::interrupts::without_interrupts(|| {
        if let Some(writer) = SCREEN.lock().as_ref() {
            pos = (writer.char_x, writer.char_y);
        }
    });
    pos
}

/// Moves the cursor to a character cell (no output). For status-bar redraws
/// and fixed-position UI.
pub fn set_pos(char_x: usize, char_y: usize) {
    with_lock(|screen| {
        screen.char_x = char_x;
        screen.char_y = char_y;
    });
}

/// Fills one text line's pixels with a solid color and moves the cursor to
/// that line's start — the base for status bars (write text over it after).
pub fn fill_row(row: usize, color: Color) {
    with_lock(|screen| {
        screen.fg = color;
        screen.fill_row(row);
    });
}

/// Writes one character to the screen.
pub fn out_char(c: char) {
    with_lock(|screen| screen.write_char(c));
}

/// Writes a string to the screen.
pub fn out_str(s: &str) {
    with_lock(|screen| {
        for c in s.chars() {
            screen.write_char(c);
        }
    });
}

/// Clears the screen.
pub fn clear_screen() {
    with_lock(|screen| screen.clear());
}

/// Changes the foreground color for subsequent output.
pub fn set_color(color: Color) {
    with_lock(|screen| screen.fg = color);
}

/// Erases the character left of the cursor (within the current line only).
pub fn backspace() {
    with_lock(|screen| screen.backspace());
}

/// Returns the framebuffer layout the screen renders into, if initialized.
pub fn framebuffer_info() -> Option<bootloader_api::info::FrameBufferInfo> {
    let guard = SCREEN.lock();
    guard.as_ref().map(|screen| screen.fb.info)
}

/// Terminal size in character cells (rows, cols), if initialized.
pub fn terminal_size() -> Option<(usize, usize)> {
    let guard = SCREEN.lock();
    guard
        .as_ref()
        .map(|screen| (screen.max_char_y(), screen.max_char_x()))
}

/// Returns the (bootloader-mapped) virtual address of the framebuffer, if
/// initialized. Read-only inspection (tests); writes must go through the
/// screen API.
pub fn framebuffer_addr() -> Option<u64> {
    let guard = SCREEN.lock();
    guard
        .as_ref()
        .map(|screen| screen.fb.buffer.as_ptr() as u64)
}

/// Format-hook used by the `print!`/`println!` macros.
///
/// Lock-audit rule (docs/DESIGN.md): the screen lock must not be held across
/// a preemption (the timer IRQ) — printing happens with interrupts off.
#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use core::fmt::Write;
    use x86_64::instructions::interrupts;
    interrupts::without_interrupts(|| {
        with_lock(|screen| screen.write_fmt(args).expect("printing to screen failed"))
    });
}
