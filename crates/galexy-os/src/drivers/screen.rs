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

use crate::serial_println;
use noto_sans_mono_bitmap::{get_raster, FontWeight, RasterHeight};

/// Glyph raster used for all text.
const FONT_WEIGHT: FontWeight = FontWeight::Regular;
/// Glyph raster height in pixels.
const FONT_HEIGHT: RasterHeight = RasterHeight::Size16;
/// Vertical padding between text lines, in pixels.
const LINE_SPACING: usize = 2;
/// Advance of one text line, in pixels.
const LINE_HEIGHT: usize = FONT_HEIGHT.val() + LINE_SPACING;
/// Foreground restored by SGR 0 / SGR 39.
const DEFAULT_FG: Color = Color::new(0xE0, 0xE0, 0xE0);
/// Columns between tab stops.
const TAB_WIDTH: usize = 8;

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

/// Where the byte stream is inside an escape sequence.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AnsiState {
    Ground,
    Esc,
    Csi,
}

/// Fixed-size CSI parser. No allocation: a sequence that spans two
/// `write` calls keeps its place here.
#[derive(Clone, Copy)]
struct AnsiParser {
    state: AnsiState,
    params: [u16; 4],
    present: [bool; 4],
    count: u8,
    current: u16,
    any_digit: bool,
}

impl AnsiParser {
    const fn ground() -> Self {
        Self {
            state: AnsiState::Ground,
            params: [0; 4],
            present: [false; 4],
            count: 0,
            current: 0,
            any_digit: false,
        }
    }

    fn enter_csi(&mut self) {
        *self = Self::ground();
        self.state = AnsiState::Csi;
    }

    fn push(&mut self) {
        let i = self.count as usize;
        if i < self.params.len() {
            self.params[i] = self.current;
            self.present[i] = self.any_digit;
            self.count += 1;
        }
        self.current = 0;
        self.any_digit = false;
    }

    fn param(&self, index: usize, default: u16) -> u16 {
        match self.present.get(index) {
            Some(true) => self.params[index],
            _ => default,
        }
    }
}

/// Columns remembered per TTY. Wider than the 1280-wide QEMU framebuffer
/// at this font's advance.
const TTY_COLS: usize = 200;
/// Text rows remembered per TTY, not counting the status bar.
/// Sized for tall framebuffers (4K / 18px line ≈ 120 text rows).
const TTY_ROWS: usize = 128;

/// One saved character. A zero `ch` is an empty cell.
#[derive(Clone, Copy)]
struct Cell {
    ch: char,
    fg: Color,
}

impl Cell {
    const BLANK: Self = Self {
        ch: '\0',
        fg: Color { r: 0, g: 0, b: 0 },
    };
}

/// Saved text for every TTY. Touched only while [`SCREEN`] is already held
/// (lock order: screen, then this). The framebuffer is painted from the
/// TTY that is on screen; the others keep their cells.
struct TtyGrids {
    cells: [[[Cell; TTY_COLS]; TTY_ROWS]; super::keyboard::TTY_COUNT],
    x: [usize; super::keyboard::TTY_COUNT],
    y: [usize; super::keyboard::TTY_COUNT],
    fg: [Color; super::keyboard::TTY_COUNT],
    ansi: [AnsiParser; super::keyboard::TTY_COUNT],
}

static GRIDS: Mutex<TtyGrids> = Mutex::new(TtyGrids {
    cells: [[[Cell::BLANK; TTY_COLS]; TTY_ROWS]; super::keyboard::TTY_COUNT],
    x: [0; super::keyboard::TTY_COUNT],
    y: [0; super::keyboard::TTY_COUNT],
    fg: [DEFAULT_FG; super::keyboard::TTY_COUNT],
    ansi: [AnsiParser::ground(); super::keyboard::TTY_COUNT],
});

/// Terminal state + framebuffer handle, guarded by the global lock.
struct ScreenWriter {
    fb: FramebufferSpec,
    /// Glyph advance width in pixels (mono font: same for all glyphs).
    char_width: usize,
    /// Cursor position, in characters. Belongs to [`Self::focus`].
    char_x: usize,
    char_y: usize,
    fg: Color,
    ansi: AnsiParser,
    /// TTY whose cursor and cells the next glyph updates.
    focus: usize,
    /// TTY currently painted on the framebuffer.
    shown: usize,
    /// Underscore phase. The main loop toggles this every 500 ms.
    blink_on: bool,
    /// `apply_blink` has run at least once (so the first phase paints).
    blink_seen: bool,
    /// An underscore is drawn at [`Self::mark_x`] / [`Self::mark_y`].
    mark_shown: bool,
    mark_x: usize,
    mark_y: usize,
}

impl ScreenWriter {
    /// Writes one character, including tab, CR, and CSI sequences.
    ///
    /// The blink mark is cleared before the glyph and redrawn after, so
    /// a block cannot stick on a cell the cursor has left.
    fn write_char(&mut self, c: char) {
        let paint = self.focus == self.shown;
        if paint {
            self.hide_mark();
        }
        self.dispatch_char(c);
        if paint && self.blink_on {
            self.show_mark();
        }
    }

    fn dispatch_char(&mut self, c: char) {
        match self.ansi.state {
            AnsiState::Esc => {
                self.feed_esc(c);
                return;
            }
            AnsiState::Csi => {
                self.feed_csi(c);
                return;
            }
            AnsiState::Ground => {}
        }
        match c {
            '\u{1b}' => self.ansi.state = AnsiState::Esc,
            '\t' => self.tab(),
            '\r' => self.char_x = 0,
            '\n' => self.new_line(),
            '\u{0008}' => self.backspace(),
            '\u{000c}' => self.clear(),
            c => self.put_glyph(c),
        }
    }

    /// Draws one glyph and advances. Normal text stays in the rows above
    /// the status bar; if the cursor ever drifts onto the status row
    /// (legacy `set_pos` paths), snap it back so typing stays visible.
    fn put_glyph(&mut self, c: char) {
        self.clamp_to_text();
        let rows = self.text_rows();
        if rows == 0 {
            return;
        }
        if self.char_x >= self.max_char_x() {
            self.new_line();
            self.clamp_to_text();
        }
        if self.char_y < rows {
            self.store_cell(c);
            self.draw_glyph(c);
        }
        self.char_x += 1;
    }

    /// Keeps the text cursor on a scrollable text row, never the status bar.
    fn clamp_to_text(&mut self) {
        let rows = self.text_rows();
        if rows == 0 {
            return;
        }
        if self.char_y >= rows {
            self.char_y = rows - 1;
        }
    }

    fn feed_esc(&mut self, c: char) {
        match c {
            '[' => self.ansi.enter_csi(),
            '\u{1b}' => {}
            _ => self.ansi.state = AnsiState::Ground,
        }
    }

    fn feed_csi(&mut self, c: char) {
        match c {
            '0'..='9' => {
                self.ansi.any_digit = true;
                let digit = (c as u8 - b'0') as u16;
                self.ansi.current = self.ansi.current.saturating_mul(10).saturating_add(digit);
            }
            ';' => self.ansi.push(),
            '\u{1b}' => self.ansi.state = AnsiState::Esc,
            ch if ('\u{40}'..='\u{7e}').contains(&ch) => {
                self.ansi.push();
                self.dispatch_csi(ch);
                self.ansi.state = AnsiState::Ground;
            }
            _ => self.ansi.state = AnsiState::Ground,
        }
    }

    fn dispatch_csi(&mut self, final_byte: char) {
        match final_byte {
            'H' | 'f' => self.cursor_pos(),
            'A' => self.cursor_move(0, -self.csi_n()),
            'B' => self.cursor_move(0, self.csi_n()),
            'C' => self.cursor_move(self.csi_n(), 0),
            'D' => self.cursor_move(-self.csi_n(), 0),
            'J' => self.erase_display(),
            'K' => self.erase_line(),
            'm' => self.apply_sgr(),
            _ => {}
        }
    }

    /// CSI count, at least 1 when the parameter was omitted.
    fn csi_n(&self) -> isize {
        self.ansi.param(0, 1).max(1) as isize
    }

    fn cursor_pos(&mut self) {
        let rows = self.text_rows().max(1);
        let cols = self.max_char_x().max(1);
        let row = (self.ansi.param(0, 1).max(1) as usize).min(rows);
        let col = (self.ansi.param(1, 1).max(1) as usize).min(cols);
        self.char_y = row - 1;
        self.char_x = col - 1;
    }

    fn cursor_move(&mut self, dx: isize, dy: isize) {
        let rows = self.text_rows();
        let cols = self.max_char_x();
        if rows == 0 || cols == 0 {
            return;
        }
        let y = self.char_y.min(rows - 1) as isize + dy;
        let x = self.char_x as isize + dx;
        self.char_y = y.clamp(0, (rows - 1) as isize) as usize;
        self.char_x = x.clamp(0, (cols - 1) as isize) as usize;
    }

    fn erase_display(&mut self) {
        match self.ansi.param(0, 0) {
            2 => {
                let rows = self.text_rows();
                for row in 0..rows {
                    self.blank_row(row);
                }
            }
            0 => {
                let row = self.char_y;
                let col = self.char_x;
                if row < self.text_rows() {
                    self.erase_to_eol();
                    for r in (row + 1)..self.text_rows() {
                        self.blank_row(r);
                    }
                }
                self.char_y = row;
                self.char_x = col;
            }
            _ => {}
        }
    }

    fn erase_line(&mut self) {
        match self.ansi.param(0, 0) {
            0 => self.erase_to_eol(),
            2 => {
                let saved = self.char_x;
                self.char_x = 0;
                self.erase_to_eol();
                self.char_x = saved;
            }
            _ => {}
        }
    }

    fn erase_to_eol(&mut self) {
        let saved = self.char_x;
        let end = self.max_char_x();
        while self.char_x < end {
            self.clear_cell();
            self.char_x += 1;
        }
        self.char_x = saved;
    }

    fn apply_sgr(&mut self) {
        let n = self.ansi.count as usize;
        let any = self.ansi.present[..n].contains(&true);
        if n == 0 || !any {
            self.fg = DEFAULT_FG;
            return;
        }
        for i in 0..n {
            if self.ansi.present[i] {
                self.apply_sgr_one(self.ansi.params[i]);
            }
        }
    }

    fn apply_sgr_one(&mut self, code: u16) {
        self.fg = match code {
            0 | 39 => DEFAULT_FG,
            30 => Color::new(0x00, 0x00, 0x00),
            31 => Color::new(0xE0, 0x40, 0x40),
            32 => Color::new(0x40, 0xE0, 0x40),
            33 => Color::new(0xE0, 0xE0, 0x40),
            34 => Color::new(0x60, 0x80, 0xE0),
            35 => Color::new(0xE0, 0x60, 0xE0),
            36 => Color::new(0x40, 0xE0, 0xE0),
            37 => DEFAULT_FG,
            90 => Color::new(0x80, 0x80, 0x80),
            91 => Color::new(0xFF, 0x80, 0x80),
            92 => Color::new(0x80, 0xFF, 0x80),
            93 => Color::new(0xFF, 0xFF, 0x80),
            94 => Color::new(0x80, 0xA0, 0xFF),
            95 => Color::new(0xFF, 0x80, 0xFF),
            96 => Color::new(0x80, 0xFF, 0xFF),
            97 => Color::new(0xFF, 0xFF, 0xFF),
            _ => self.fg,
        };
    }

    fn tab(&mut self) {
        self.clamp_to_text();
        let next = (self.char_x / TAB_WIDTH + 1) * TAB_WIDTH;
        if next >= self.max_char_x() {
            self.new_line();
        } else {
            self.char_x = next;
        }
    }

    /// Renders one glyph at the cursor.
    ///
    /// Glyph rasters at this size have thin strokes whose pixels never reach
    /// full intensity (measured: glyph 'a' peaks at 223/255), so intensities
    /// are normalized per glyph to the raster's peak — glyph cores render at
    /// full fg color instead of a washed-out fraction of it.
    fn draw_glyph(&mut self, c: char) {
        if !self.paint_pixels() {
            return;
        }
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
        self.store_cell('\0');
        self.blank_cell_pixels();
    }

    /// Blacks out the cell at the cursor. Does not change the cell grid.
    fn blank_cell_pixels(&mut self) {
        if !self.paint_pixels() {
            return;
        }
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

    /// Moves to the next text line. The last row is the status bar: text
    /// scrolls above it and never lands on it.
    fn new_line(&mut self) {
        let rows = self.text_rows();
        if rows == 0 {
            self.char_x = 0;
            return;
        }
        self.clamp_to_text();
        if self.char_y + 1 >= rows {
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

    /// Shifts the text rows up by one line and clears the last text row.
    ///
    /// The status row is not part of the shift. `stride` is in pixels
    /// (bootloader doc), so the byte count includes `bytes_per_pixel`.
    fn scroll_up(&mut self) {
        let rows = self.text_rows();
        if rows == 0 {
            return;
        }
        self.scroll_cells();
        if self.focus == self.shown {
            let info = self.fb.info;
            let line_bytes = LINE_HEIGHT * info.stride * info.bytes_per_pixel;
            let text_bytes = rows * line_bytes;
            let buffer = &mut *self.fb.buffer;
            if line_bytes < text_bytes && text_bytes <= buffer.len() {
                buffer.copy_within(line_bytes..text_bytes, 0);
            }
        }
        self.blank_row(rows - 1);
    }

    /// Paints one text row black without moving the cursor.
    fn blank_row(&mut self, row: usize) {
        self.clear_row_cells(row);
        if self.focus != self.shown {
            return;
        }
        let saved = self.fg;
        self.fg = Color::new(0, 0, 0);
        self.clear_row_pixels(row);
        self.fg = saved;
    }

    /// Fills one text line's pixels with a solid color. Does not move the
    /// text cursor — status-bar redraws must not steal the typing position.
    fn fill_row(&mut self, row: usize) {
        self.clear_row_pixels(row);
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

    /// Terminal height in characters, including the status row.
    fn max_char_y(&self) -> usize {
        self.fb.info.height / LINE_HEIGHT
    }

    /// Rows normal text may use. The last row belongs to the status bar
    /// when the screen has more than one row.
    fn text_rows(&self) -> usize {
        match self.max_char_y() {
            0 | 1 => self.max_char_y(),
            n => n - 1,
        }
    }

    /// Fills the focused TTY with black and resets its cursor.
    ///
    /// The framebuffer changes only when that TTY is the one on screen, so
    /// a background clear leaves the visible console alone.
    fn clear(&mut self) {
        if self.focus == self.shown {
            self.mark_shown = false;
        }
        self.clear_all_cells();
        if self.focus == self.shown {
            let buffer = &mut *self.fb.buffer;
            let info = self.fb.info;
            for pixel in buffer.chunks_exact_mut(info.bytes_per_pixel) {
                for byte in pixel {
                    *byte = 0;
                }
            }
        }
        self.char_x = 0;
        self.char_y = 0;
        self.ansi = AnsiParser::ground();
    }

    /// True when the next glyph should touch the framebuffer.
    ///
    /// Text pixels follow the focused TTY only while it is visible. The
    /// status row is global, so it always paints.
    fn paint_pixels(&self) -> bool {
        self.focus == self.shown || self.char_y >= self.text_rows()
    }

    fn store_cell(&self, ch: char) {
        if self.char_y >= TTY_ROWS || self.char_x >= TTY_COLS {
            return;
        }
        let focus = self.focus;
        if focus >= super::keyboard::TTY_COUNT {
            return;
        }
        let x = self.char_x;
        let y = self.char_y;
        GRIDS.lock().cells[focus][y][x] = Cell { ch, fg: self.fg };
    }

    fn clear_row_cells(&self, row: usize) {
        if row >= TTY_ROWS || self.focus >= super::keyboard::TTY_COUNT {
            return;
        }
        let focus = self.focus;
        let mut grids = GRIDS.lock();
        grids.cells[focus][row].fill(Cell::BLANK);
    }

    fn clear_all_cells(&self) {
        if self.focus >= super::keyboard::TTY_COUNT {
            return;
        }
        let focus = self.focus;
        let mut grids = GRIDS.lock();
        for row in &mut grids.cells[focus] {
            row.fill(Cell::BLANK);
        }
    }

    fn scroll_cells(&self) {
        let rows = self.text_rows().min(TTY_ROWS);
        if rows == 0 || self.focus >= super::keyboard::TTY_COUNT {
            return;
        }
        let focus = self.focus;
        let mut grids = GRIDS.lock();
        let cells = &mut grids.cells[focus];
        for row in 1..rows {
            cells[row - 1] = cells[row];
        }
        cells[rows - 1] = [Cell::BLANK; TTY_COLS];
    }

    fn save_focus(&self) {
        if self.focus >= super::keyboard::TTY_COUNT {
            return;
        }
        let mut grids = GRIDS.lock();
        let i = self.focus;
        grids.x[i] = self.char_x;
        grids.y[i] = self.char_y;
        grids.fg[i] = self.fg;
        grids.ansi[i] = self.ansi;
    }

    fn load_focus(&mut self) {
        if self.focus >= super::keyboard::TTY_COUNT {
            return;
        }
        let grids = GRIDS.lock();
        let i = self.focus;
        self.char_x = grids.x[i];
        self.char_y = grids.y[i];
        self.fg = grids.fg[i];
        self.ansi = grids.ansi[i];
    }

    fn focus_on(&mut self, tty: usize) {
        if self.focus == tty {
            return;
        }
        self.save_focus();
        self.focus = tty;
        self.load_focus();
    }

    /// Paints the focused TTY's text rows onto the framebuffer.
    fn repaint_text(&mut self) {
        let saved_x = self.char_x;
        let saved_y = self.char_y;
        let saved_fg = self.fg;
        let saved_ansi = self.ansi;
        let rows = self.text_rows().min(TTY_ROWS);
        let cols = self.max_char_x().min(TTY_COLS);
        for row in 0..rows {
            self.fg = Color::new(0, 0, 0);
            self.clear_row_pixels(row);
            let mut row_cells = [Cell::BLANK; TTY_COLS];
            {
                let grids = GRIDS.lock();
                row_cells[..cols].copy_from_slice(&grids.cells[self.focus][row][..cols]);
            }
            for (col, cell) in row_cells.iter().enumerate().take(cols) {
                if cell.ch == '\0' || cell.ch == ' ' {
                    continue;
                }
                self.char_x = col;
                self.char_y = row;
                self.fg = cell.fg;
                self.draw_glyph(cell.ch);
            }
        }
        self.char_x = saved_x;
        self.char_y = saved_y;
        self.fg = saved_fg;
        self.ansi = saved_ansi;
    }

    fn shown_cursor(&self) -> (usize, usize) {
        if self.focus == self.shown {
            (self.char_x, self.char_y)
        } else {
            let grids = GRIDS.lock();
            let i = self.shown.min(super::keyboard::TTY_COUNT - 1);
            (grids.x[i], grids.y[i])
        }
    }

    /// Restores the cell under the blink mark so the underscore cannot stick.
    fn hide_mark(&mut self) {
        if !self.mark_shown {
            return;
        }
        let x = self.mark_x;
        let y = self.mark_y;
        self.mark_shown = false;
        self.redraw_shown_cell(x, y);
    }

    fn show_mark(&mut self) {
        let (x, y) = self.shown_cursor();
        let cols = self.max_char_x();
        let rows = self.text_rows();
        if cols == 0 || rows == 0 || x >= cols || y >= rows {
            return;
        }
        self.draw_underscore(x, y);
        self.mark_x = x;
        self.mark_y = y;
        self.mark_shown = true;
    }

    fn apply_blink(&mut self, on: bool) {
        self.blink_on = on;
        self.blink_seen = true;
        self.hide_mark();
        if on {
            self.show_mark();
        }
    }

    /// Two-pixel bar at the bottom of a cell. Does not change the grid.
    fn draw_underscore(&mut self, x: usize, y: usize) {
        let x_origin = x * self.char_width;
        let y_origin = y * LINE_HEIGHT + FONT_HEIGHT.val().saturating_sub(2);
        let buffer = &mut *self.fb.buffer;
        let info = self.fb.info;
        let pixel = DEFAULT_FG;
        for row in 0..2 {
            let py = y_origin + row;
            if py >= info.height {
                break;
            }
            for col in 0..self.char_width {
                let px = x_origin + col;
                if px >= info.width {
                    break;
                }
                let idx = (py * info.stride + px) * info.bytes_per_pixel;
                Self::write_pixel(buffer, idx, info, pixel);
            }
        }
    }

    fn redraw_shown_cell(&mut self, x: usize, y: usize) {
        if y >= TTY_ROWS || x >= TTY_COLS || self.shown >= super::keyboard::TTY_COUNT {
            return;
        }
        let cell = GRIDS.lock().cells[self.shown][y][x];
        let saved_x = self.char_x;
        let saved_y = self.char_y;
        let saved_fg = self.fg;
        let saved_focus = self.focus;
        self.focus = self.shown;
        self.char_x = x;
        self.char_y = y;
        self.blank_cell_pixels();
        if cell.ch != '\0' && cell.ch != ' ' {
            self.fg = cell.fg;
            self.draw_glyph(cell.ch);
        }
        self.char_x = saved_x;
        self.char_y = saved_y;
        self.fg = saved_fg;
        self.focus = saved_focus;
    }

    fn underscore_ink(&self, x: usize, y: usize) -> bool {
        let x_origin = x * self.char_width;
        let y_origin = y * LINE_HEIGHT + FONT_HEIGHT.val().saturating_sub(2);
        let buffer = &*self.fb.buffer;
        let info = self.fb.info;
        for row in 0..2 {
            let py = y_origin + row;
            if py >= info.height {
                continue;
            }
            for col in 0..self.char_width {
                let px = x_origin + col;
                if px >= info.width {
                    continue;
                }
                let idx = (py * info.stride + px) * info.bytes_per_pixel;
                let end = idx + info.bytes_per_pixel;
                if buffer
                    .get(idx..end)
                    .is_some_and(|px| px.iter().any(|b| *b != 0))
                {
                    return true;
                }
            }
        }
        false
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
        fg: DEFAULT_FG,
        ansi: AnsiParser::ground(),
        focus: 0,
        shown: 0,
        blink_on: false,
        blink_seen: false,
        mark_shown: false,
        mark_x: 0,
        mark_y: 0,
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

/// Moves the cursor to a character cell (no output). Clamped to the text
/// area — use [`draw_status_bar`] for the status row.
pub fn set_pos(char_x: usize, char_y: usize) {
    with_lock(|screen| {
        if screen.focus == screen.shown {
            screen.hide_mark();
        }
        screen.char_x = char_x;
        screen.char_y = char_y;
        screen.clamp_to_text();
        if screen.focus == screen.shown && screen.blink_on {
            screen.show_mark();
        }
    });
}

/// Toggles the shown TTY's underscore from the main loop.
///
/// Phase is `timer_ticks / 500`. Typing clears the mark before the next
/// glyph, so a block cannot remain on a cell the cursor has left.
pub fn blink_cursor() {
    let on = (crate::arch::timer_ticks() / 500).is_multiple_of(2);
    with_lock(|screen| {
        if screen.blink_seen && screen.blink_on == on {
            return;
        }
        screen.apply_blink(on);
    });
}

/// Draws the underscore and reports whether those pixels are lit.
/// Test kernels call this; production uses [`blink_cursor`].
pub fn test_cursor_bar_lit() -> bool {
    let mut ink = false;
    with_lock(|screen| {
        screen.apply_blink(true);
        ink = screen.mark_shown && screen.underscore_ink(screen.mark_x, screen.mark_y);
    });
    ink
}

/// Clears the underscore and reports that the cursor cell's bar is dark.
pub fn test_cursor_bar_clear() -> bool {
    let mut dark = false;
    with_lock(|screen| {
        let (x, y) = screen.shown_cursor();
        screen.apply_blink(false);
        dark = !screen.mark_shown && !screen.underscore_ink(x, y);
    });
    dark
}

/// Fills one text line's pixels with a solid color without moving the
/// text cursor.
pub fn fill_row(row: usize, color: Color) {
    with_lock(|screen| {
        screen.fg = color;
        screen.fill_row(row);
    });
}

/// Paints the status row in-place without touching the text cursor.
///
/// The shell's input line must stay on a text row; hijacking `char_x` /
/// `char_y` for the bar (then failing to restore) made typed characters
/// land on the status row and vanish on the next redraw.
pub fn draw_status_bar(bg: Color, fg: Color, text: &str) {
    with_lock(|screen| {
        let row = match screen.max_char_y() {
            0 => return,
            n => n - 1,
        };
        let saved_x = screen.char_x;
        let saved_y = screen.char_y;
        let saved_fg = screen.fg;
        let cols = screen.max_char_x();

        screen.fg = bg;
        screen.clear_row_pixels(row);
        screen.fg = fg;
        screen.char_y = row;
        for (i, c) in text.chars().take(cols).enumerate() {
            screen.char_x = i;
            screen.draw_glyph(c);
        }

        screen.char_x = saved_x;
        screen.char_y = saved_y;
        screen.fg = saved_fg;
        screen.clamp_to_text();
    });
}

/// Writes one character to the screen's visible TTY.
pub fn out_char(c: char) {
    with_lock(|screen| screen.write_char(c));
}

/// Writes a string to the screen's visible TTY, interpreting tab, CR, and CSI.
pub fn out_str(s: &str) {
    with_lock(|screen| {
        for c in s.chars() {
            screen.write_char(c);
        }
    });
}

/// Writes `s` into TTY `tty`. Pixels update only when that TTY is visible.
pub fn out_str_tty(tty: u8, s: &str) {
    let tty = (tty as usize).min(super::keyboard::TTY_COUNT - 1);
    with_lock(|screen| {
        let home = screen.focus;
        screen.focus_on(tty);
        for c in s.chars() {
            screen.write_char(c);
        }
        // Persist this TTY's cursor even when focus never changed (the
        // common shell path). Without this, a later focus switch reloads
        // a stale grid cursor and the input line looks "lost".
        screen.save_focus();
        screen.focus_on(home);
    });
}

/// TTY currently painted on the framebuffer. `0` before [`init`].
pub fn shown_tty() -> u8 {
    let mut tty = 0u8;
    x86_64::instructions::interrupts::without_interrupts(|| {
        if let Some(writer) = SCREEN.lock().as_ref() {
            tty = writer.shown as u8;
        }
    });
    tty
}

/// Paints TTY `index` and sends later keystrokes there.
pub fn show_tty(index: u8) {
    let index = (index as usize).min(super::keyboard::TTY_COUNT - 1);
    super::keyboard::set_active(index as u8);
    with_lock(|screen| {
        screen.mark_shown = false;
        screen.focus_on(index);
        screen.shown = index;
        screen.repaint_text();
        if screen.blink_on {
            screen.show_mark();
        }
    });
    serial_println!("[tty] {}", index + 1);
}

/// Paints a TTY switch the keyboard recorded, if one is waiting.
///
/// The keyboard interrupt only stores the index. This runs from the main
/// loop, which already owns the screen, so the two locks never nest.
pub fn apply_tty_switch() {
    let Some(index) = super::keyboard::take_switch() else {
        return;
    };
    show_tty(index);
}

/// Writes glyphs with no escape parsing (text area only). Prefer
/// [`draw_status_bar`] for the bottom status row.
pub fn out_plain(s: &str) {
    with_lock(|screen| {
        if screen.focus == screen.shown {
            screen.hide_mark();
        }
        for c in s.chars() {
            screen.put_glyph(c);
        }
        if screen.focus == screen.shown && screen.blink_on {
            screen.show_mark();
        }
    });
}

/// Clears the screen.
pub fn clear_screen() {
    with_lock(|screen| {
        screen.clear();
        if screen.blink_on {
            screen.show_mark();
        }
    });
}

/// Changes the foreground color for subsequent output.
pub fn set_color(color: Color) {
    with_lock(|screen| screen.fg = color);
}

/// Erases the character left of the cursor (within the current line only).
pub fn backspace() {
    with_lock(|screen| {
        if screen.focus == screen.shown {
            screen.hide_mark();
        }
        screen.backspace();
        if screen.focus == screen.shown && screen.blink_on {
            screen.show_mark();
        }
    });
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
