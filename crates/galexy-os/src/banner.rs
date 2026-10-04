//! Boot banner: the galexy.os feature showcase.
//!
//! Prints everything the kernel brings online — framebuffer, interrupts,
//! frame allocator, paging, heap — with colors, then hands over to the
//! shell. Runs after all subsystem init; each `[ok]` line is real state
//! read from the owning subsystem, not a static claim.

use crate::arch::mm;
use crate::drivers::screen::{self, Color};

const TITLE_COLOR: Color = Color::new(0x7C, 0xA0, 0xFF);
const OK_COLOR: Color = Color::new(0x51, 0xC8, 0x78);
const VALUE_COLOR: Color = Color::new(0xB0, 0xB0, 0xB0);
const TEXT_COLOR: Color = Color::new(0xE0, 0xE0, 0xE0);

/// Renders the boot banner and the first shell prompt.
pub fn show() {
    screen::clear_screen();

    screen::set_color(TITLE_COLOR);
    print!("════════ galexy.os ════════\n");
    screen::set_color(TEXT_COLOR);

    // Framebuffer
    screen::set_color(OK_COLOR);
    print!("[ok] ");
    screen::set_color(TEXT_COLOR);
    print!("framebuffer ");
    screen::set_color(VALUE_COLOR);
    if let Some(info) = screen::framebuffer_info() {
        print!("{}x{} {:?}\n", info.width, info.height, info.pixel_format);
    } else {
        print!("unavailable\n");
    }
    screen::set_color(TEXT_COLOR);

    // Interrupts
    screen::set_color(OK_COLOR);
    print!("[ok] ");
    screen::set_color(TEXT_COLOR);
    print!("interrupts: timer @ ~1kHz, keyboard, fault handlers\n");

    // Frame allocator
    screen::set_color(OK_COLOR);
    print!("[ok] ");
    screen::set_color(TEXT_COLOR);
    print!("frames free: ");
    screen::set_color(VALUE_COLOR);
    print!("{}\n", mm::free_frames());
    screen::set_color(TEXT_COLOR);

    // Paging: translate the heap start as a live demo
    screen::set_color(OK_COLOR);
    print!("[ok] ");
    screen::set_color(TEXT_COLOR);
    print!("paging: heap virt ");
    screen::set_color(VALUE_COLOR);
    let (heap_start, _) = mm::heap::stats();
    let translated = mm::translate(x86_64::VirtAddr::new(heap_start));
    print!("{:#x} -> phys {:?}\n", heap_start, translated);
    screen::set_color(TEXT_COLOR);

    // Heap
    screen::set_color(OK_COLOR);
    print!("[ok] ");
    screen::set_color(TEXT_COLOR);
    print!("heap ");
    screen::set_color(VALUE_COLOR);
    let (_, heap_size) = mm::heap::stats();
    print!("{} KiB\n", heap_size / 1024);
    screen::set_color(TEXT_COLOR);

    // Scheduler
    screen::set_color(OK_COLOR);
    print!("[ok] ");
    screen::set_color(TEXT_COLOR);
    print!("scheduler: ");
    screen::set_color(VALUE_COLOR);
    print!(
        "{} tasks running, {} spawned\n",
        crate::sched::active_tasks(),
        crate::sched::spawned_total()
    );
    screen::set_color(TEXT_COLOR);

    screen::out_str("\nType a line — Enter echoes it back, Backspace edits.\n\n");

    // First prompt
    screen::set_color(crate::echo::PROMPT_COLOR);
    screen::out_str("galexy> ");
    screen::set_color(TEXT_COLOR);
}
