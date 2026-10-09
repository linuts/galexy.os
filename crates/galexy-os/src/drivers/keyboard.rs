//! PS/2 keyboard input: scancode decoding and one key queue per TTY.
//!
//! The IRQ1 handler feeds scancodes in via [`add_scancode`]. Unicode
//! characters go to the TTY that is on screen. F1–F12 only record which
//! TTY to show; the main loop paints it. This handler never takes the
//! screen lock. Locks are kept tiny and never nested, so this is safe to
//! call from interrupt context.

use core::sync::atomic::{AtomicI8, AtomicU64, AtomicU8, Ordering};

use crate::sync::Mutex;
use galexy_core::Ring;
use pc_keyboard::{
    layouts, DecodedKey, HandleControl, KeyCode, KeyState, PS2Keyboard, ScancodeSet1,
};
use x86_64::instructions::port::Port;

/// How many text consoles F1–F12 select.
pub const TTY_COUNT: usize = 12;

/// Keyboard queue capacity in characters, per TTY.
///
/// A full queue drops the newest character. [`drops`] counts those
/// losses; the first drop and every 16th print one serial line.
pub const QUEUE_CAPACITY: usize = 64;

/// No TTY switch is waiting.
const SWITCH_NONE: u8 = 0xff;

static KEYBOARD: Mutex<PS2Keyboard<layouts::Us104Key, ScancodeSet1>> =
    Mutex::new(PS2Keyboard::new(
        ScancodeSet1::new(),
        layouts::Us104Key,
        // Deliver Ctrl+A..Z as U+0001..U+001A so shells can cancel
        // password prompts with Ctrl-C.
        HandleControl::MapLettersToUnicode,
    ));

static KEY_QUEUES: [Mutex<Ring<char, QUEUE_CAPACITY>>; TTY_COUNT] =
    [const { Mutex::new(Ring::new()) }; TTY_COUNT];
/// One key put back by a short `read` that could not fit its UTF-8, per
/// TTY. `pop_key_tty` returns it before that TTY's queue.
static UNGOTS: Mutex<[Option<char>; TTY_COUNT]> = Mutex::new([None; TTY_COUNT]);
/// TTY that receives the next Unicode character. The screen updates this
/// when it paints a switch.
static ACTIVE: AtomicU8 = AtomicU8::new(0);
/// TTY the main loop should paint. `SWITCH_NONE` means nothing is waiting.
static PENDING: AtomicU8 = AtomicU8::new(SWITCH_NONE);
/// Characters dropped because a TTY queue was full.
static DROPS: AtomicU64 = AtomicU64::new(0);
/// Scrollback pages waiting for the main loop. Negative is older text.
static SCROLL: AtomicI8 = AtomicI8::new(0);

/// Brings the PS/2 controller's first port (keyboard) online: the enable
/// command + stale-buffer drain. Formerly part of `arch::pics::init` — it
/// is about the i8042, not about which controller delivers IRQ1, so it
/// lives with the keyboard driver and runs on EVERY boot path (BIOS and
/// UEFI alike; OVMF may leave the port disabled).
pub fn init() {
    if crate::drivers::virtio_input::probe() {
        return;
    }
    crate::serial_println!("[kbd] ps/2 i8042");
    crate::arch::ioapic::wire_ps2_keyboard();
    // SAFETY: fixed controller command/data ports.
    unsafe {
        Port::new(0x64).write(0xAE_u8);
        // Drain any bytes the firmware left in the output buffer: a full
        // buffer never re-asserts the keyboard line, so the first real
        // keystroke would black-hole.
        let mut status = Port::<u8>::new(0x64);
        let mut data = Port::<u8>::new(0x60);
        let mut guard = 0u32;
        while status.read() & 0x01 != 0 && guard < 64 {
            let _ = data.read();
            guard += 1;
        }
    }
}

/// Feeds a raw scancode into the decoder. Called from the IRQ1 handler.
///
/// Lock-audit rule: the keyboard locks may never be held across a preemption
/// — `without_interrupts` is a no-op here (IRQ gate) but keeps the rule
/// explicit if this ever gains a non-IRQ caller.
pub fn add_scancode(scancode: u8) {
    use x86_64::instructions::interrupts;
    interrupts::without_interrupts(|| {
        let decoded = {
            let mut keyboard = KEYBOARD.lock();
            match keyboard.add_byte(scancode) {
                Ok(Some(key_event)) => {
                    // Modifiers update inside `process_keyevent`, so read
                    // them first. Shift+PageUp must not take the screen lock.
                    let mods = keyboard.get_modifiers();
                    let shift = mods.lshift || mods.rshift;
                    let down = matches!(key_event.state, KeyState::Down | KeyState::SingleShot);
                    let page_up = key_event.code == KeyCode::PageUp;
                    let page_down = key_event.code == KeyCode::PageDown;
                    let decoded = keyboard.process_keyevent(key_event);
                    if shift && down && (page_up || page_down) {
                        let dir: i8 = if page_up { -1 } else { 1 };
                        let _ = SCROLL.try_update(Ordering::AcqRel, Ordering::Relaxed, |cur| {
                            Some(cur.saturating_add(dir))
                        });
                        None
                    } else {
                        decoded
                    }
                }
                _ => None,
            }
        };
        match decoded {
            Some(DecodedKey::Unicode(c)) => push_char(c),
            Some(DecodedKey::RawKey(key)) => {
                if let Some(tty) = tty_index(key) {
                    PENDING.store(tty, Ordering::Release);
                } else if let Some(seq) = arrow_csi(key) {
                    // Arrow keys become CSI so the ring-3 shell can browse
                    // history (ESC [ A/B) without a custom scancode ABI.
                    let tty = (ACTIVE.load(Ordering::Relaxed) as usize).min(TTY_COUNT - 1);
                    for c in seq.chars() {
                        enqueue(tty, c);
                    }
                    crate::sched::wake_keyboard_waiters(tty as u8);
                }
            }
            None => {}
        }
    });
}

/// Delivers one character to the active TTY, the same way a decoded
/// PS/2 key does. COM1 receive uses this too, so a headless terminal and
/// the framebuffer keyboard share one queue. Ctrl-C stops the foreground
/// job and is not queued.
pub fn push_char(c: char) {
    use x86_64::instructions::interrupts;
    interrupts::without_interrupts(|| {
        let tty = (ACTIVE.load(Ordering::Relaxed) as usize).min(TTY_COUNT - 1);
        if c == '\u{3}' && crate::sched::interrupt_foreground(tty as u8) {
            return;
        }
        // Overflow drops the newest key. The queue lock is not held
        // across the serial warning.
        enqueue(tty, c);
        // Milestone 57: wake a reader parked on this TTY.
        crate::sched::wake_keyboard_waiters(tty as u8);
    });
}

/// F1 is 0. Other keys are not a console switch.
fn tty_index(key: KeyCode) -> Option<u8> {
    let index = match key {
        KeyCode::F1 => 0,
        KeyCode::F2 => 1,
        KeyCode::F3 => 2,
        KeyCode::F4 => 3,
        KeyCode::F5 => 4,
        KeyCode::F6 => 5,
        KeyCode::F7 => 6,
        KeyCode::F8 => 7,
        KeyCode::F9 => 8,
        KeyCode::F10 => 9,
        KeyCode::F11 => 10,
        KeyCode::F12 => 11,
        _ => return None,
    };
    Some(index)
}

/// Pushes one character. A full queue increments [`drops`] and, on the
/// first drop and every 16th, prints a rate-limited serial line.
fn enqueue(tty: usize, c: char) {
    let stored = KEY_QUEUES[tty].lock().push(c).is_some();
    if !stored {
        note_drop();
    }
}

fn note_drop() {
    let n = DROPS.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 || n.is_multiple_of(16) {
        crate::serial_println!("[kbd] queue full; dropped {} (cap {})", n, QUEUE_CAPACITY);
    }
}

/// Characters dropped because a per-TTY queue was full.
pub fn drops() -> u64 {
    DROPS.load(Ordering::Relaxed)
}

/// Pushes `n` characters into `tty` the way a full keyboard IRQ would.
/// Returns how many of those pushes were dropped. Test kernels use this
/// instead of a PS/2 flood.
pub fn test_inject(tty: u8, n: usize) -> u64 {
    let tty = (tty as usize).min(TTY_COUNT - 1);
    let before = drops();
    for i in 0..n {
        let c = char::from(b'a' + (i as u8 % 26));
        enqueue(tty, c);
    }
    drops().saturating_sub(before)
}

/// Discards every queued character on `tty` (including one ungot key).
pub fn test_drain(tty: u8) {
    while pop_key_tty(tty).is_some() {}
}

/// CSI sequences for cursor keys (`ESC [ A` … `D`).
fn arrow_csi(key: KeyCode) -> Option<&'static str> {
    match key {
        KeyCode::ArrowUp => Some("\u{1b}[A"),
        KeyCode::ArrowDown => Some("\u{1b}[B"),
        KeyCode::ArrowRight => Some("\u{1b}[C"),
        KeyCode::ArrowLeft => Some("\u{1b}[D"),
        _ => None,
    }
}

/// TTY that should receive typed characters.
pub fn set_active(tty: u8) {
    ACTIVE.store(tty.min(TTY_COUNT as u8 - 1), Ordering::Relaxed);
}

/// Takes accumulated Shift+Page scroll pages. Negative is older text.
pub fn take_scroll() -> i8 {
    SCROLL.swap(0, Ordering::AcqRel)
}

/// Takes the console switch the keyboard recorded, if one is waiting.
pub fn take_switch() -> Option<u8> {
    let tty = PENDING.swap(SWITCH_NONE, Ordering::AcqRel);
    if tty == SWITCH_NONE {
        None
    } else {
        Some(tty.min(TTY_COUNT as u8 - 1))
    }
}

/// Drains one decoded character from TTY 0, if any.
///
/// The in-kernel line editor is that console. A ring-3 shell reads its
/// own TTY through [`pop_key_tty`].
pub fn pop_key() -> Option<char> {
    pop_key_tty(0)
}

/// Drains one decoded character from `tty`, if any.
///
/// Lock-audit rule: queued-lock access must not be preemptable.
pub fn pop_key_tty(tty: u8) -> Option<char> {
    let tty = tty as usize;
    if tty >= TTY_COUNT {
        return None;
    }
    use x86_64::instructions::interrupts;
    interrupts::without_interrupts(|| {
        let mut ungot = UNGOTS.lock();
        if let Some(c) = ungot[tty].take() {
            return Some(c);
        }
        drop(ungot);
        KEY_QUEUES[tty].lock().pop()
    })
}

/// Puts `c` back on TTY 0. See [`unget_key_tty`].
pub fn unget_key(c: char) {
    unget_key_tty(0, c);
}

/// Puts `c` back so the next [`pop_key_tty`] for `tty` returns it. Only
/// the key just popped may be returned, and only when a `read` buffer
/// cannot hold it.
pub fn unget_key_tty(tty: u8, c: char) {
    let tty = tty as usize;
    if tty >= TTY_COUNT {
        return;
    }
    use x86_64::instructions::interrupts;
    interrupts::without_interrupts(|| {
        let mut ungot = UNGOTS.lock();
        assert!(ungot[tty].is_none(), "keyboard: unget already holds a key");
        ungot[tty] = Some(c);
    });
}
