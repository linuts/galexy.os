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
    fn draw_glyph(&mut self, c: char) {
        let raster = match get_raster(c, FONT_WEIGHT, FONT_HEIGHT) {
            Some(raster) => raster,
            // Unknown glyph (or unsupported char): draw a blank.
            None => match get_raster(' ', FONT_WEIGHT, FONT_HEIGHT) {
                Some(raster) => raster,
                None => return,
            },
        };

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
                let pixel = self.fg.scaled(intensity);
                let idx = (y * info.stride + x) * info.bytes_per_pixel;
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
        self.draw_glyph(' ');
    }

    /// Shifts all pixels up by one line height and clears the last line.
    fn scroll_up(&mut self) {
        let buffer = &mut *self.fb.buffer;
        let info = self.fb.info;

        let line_bytes = LINE_HEIGHT * info.stride;
        if line_bytes < buffer.len() {
            buffer.copy_within(line_bytes.., 0);
        }
        // Clear the freed lines at the bottom.
        let tail_start = buffer.len().saturating_sub(line_bytes);
        for byte in &mut buffer[tail_start..] {
            *byte = 0;
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
    let mut guard = SCREEN.lock();
    if let Some(writer) = guard.as_mut() {
        f(writer);
    }
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
// Part of the terminal-shaped module API; no consumer yet.
#[allow(dead_code)]
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

/// Format-hook used by the `print!`/`println!` macros.
#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use core::fmt::Write;
    with_lock(|screen| screen.write_fmt(args).expect("printing to screen failed"));
}
