//! PS/2 keyboard input: scancode decoding and a small key queue.
//!
//! The IRQ1 handler feeds scancodes in via [`add_scancode`]; consumers drain
//! decoded characters through [`pop_key`]. Locks are kept tiny and never
//! nested, so this is safe to call from interrupt context.

use galexy_core::Ring;
use pc_keyboard::{layouts, DecodedKey, HandleControl, PS2Keyboard, ScancodeSet1};
use spin::Mutex;
use x86_64::instructions::port::Port;

/// Keyboard queue capacity in characters.
const QUEUE_CAPACITY: usize = 64;

static KEYBOARD: Mutex<PS2Keyboard<layouts::Us104Key, ScancodeSet1>> =
    Mutex::new(PS2Keyboard::new(
        ScancodeSet1::new(),
        layouts::Us104Key,
        HandleControl::Ignore,
    ));

static KEY_QUEUE: Mutex<Ring<char, QUEUE_CAPACITY>> = Mutex::new(Ring::new());
/// One key put back by a short `read` that could not fit its UTF-8.
/// `pop_key` returns it before the queue. At most one: the reader just
/// took it.
static UNGOT: Mutex<Option<char>> = Mutex::new(None);

/// Brings the PS/2 controller's first port (keyboard) online: the enable
/// command + stale-buffer drain. Formerly part of `arch::pics::init` — it
/// is about the i8042, not about which controller delivers IRQ1, so it
/// lives with the keyboard driver and runs on EVERY boot path (BIOS and
/// UEFI alike; OVMF may leave the port disabled).
pub fn init() {
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
                Ok(Some(key_event)) => keyboard.process_keyevent(key_event),
                _ => None,
            }
        };
        if let Some(DecodedKey::Unicode(c)) = decoded {
            // Overflow drops the key by design; not an error for the decoder.
            let _ = KEY_QUEUE.lock().push(c);
        }
    });
}

/// Drains one decoded character, if any.
///
/// Lock-audit rule: queued-lock access must not be preemptable.
pub fn pop_key() -> Option<char> {
    use x86_64::instructions::interrupts;
    interrupts::without_interrupts(|| {
        if let Some(c) = UNGOT.lock().take() {
            return Some(c);
        }
        KEY_QUEUE.lock().pop()
    })
}

/// Puts `c` back so the next [`pop_key`] returns it. Only the key just
/// popped may be returned, and only when a `read` buffer cannot hold it.
pub fn unget_key(c: char) {
    use x86_64::instructions::interrupts;
    interrupts::without_interrupts(|| {
        let mut ungot = UNGOT.lock();
        assert!(ungot.is_none(), "keyboard: unget already holds a key");
        *ungot = Some(c);
    });
}
