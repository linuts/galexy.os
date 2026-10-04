//! PS/2 keyboard input: scancode decoding and a small key queue.
//!
//! The IRQ1 handler feeds scancodes in via [`add_scancode`]; consumers drain
//! decoded characters through [`pop_key`]. Locks are kept tiny and never
//! nested, so this is safe to call from interrupt context.

use galexy_core::Ring;
use pc_keyboard::{layouts, DecodedKey, HandleControl, PS2Keyboard, ScancodeSet1};
use spin::Mutex;

/// Keyboard queue capacity in characters.
const QUEUE_CAPACITY: usize = 64;

static KEYBOARD: Mutex<PS2Keyboard<layouts::Us104Key, ScancodeSet1>> =
    Mutex::new(PS2Keyboard::new(
        ScancodeSet1::new(),
        layouts::Us104Key,
        HandleControl::Ignore,
    ));

static KEY_QUEUE: Mutex<Ring<char, QUEUE_CAPACITY>> = Mutex::new(Ring::new());

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
    interrupts::without_interrupts(|| KEY_QUEUE.lock().pop())
}
