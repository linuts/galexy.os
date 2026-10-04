//! PS/2 keyboard input: scancode decoding and a small key queue.
//!
//! The IRQ1 handler feeds scancodes in via [`add_scancode`]; consumers drain
//! decoded characters through [`pop_key`]. Locks are kept tiny and never
//! nested, so this is safe to call from interrupt context.

use pc_keyboard::{layouts, DecodedKey, HandleControl, PS2Keyboard, ScancodeSet1};
use spin::Mutex;

/// Ring buffer capacity in characters.
const QUEUE_CAPACITY: usize = 64;

static KEYBOARD: Mutex<PS2Keyboard<layouts::Us104Key, ScancodeSet1>> =
    Mutex::new(PS2Keyboard::new(
        ScancodeSet1::new(),
        layouts::Us104Key,
        HandleControl::Ignore,
    ));

static KEY_QUEUE: Mutex<CharRing> = Mutex::new(CharRing::EMPTY);

/// Fixed-capacity FIFO of characters (no heap yet).
struct CharRing {
    data: [Option<char>; QUEUE_CAPACITY],
    head: usize,
    len: usize,
}

impl CharRing {
    const EMPTY: CharRing = CharRing {
        data: [const { None }; QUEUE_CAPACITY],
        head: 0,
        len: 0,
    };

    fn push(&mut self, c: char) {
        if self.len >= QUEUE_CAPACITY {
            // Queue overflow: drop the newest key rather than corrupt state.
            return;
        }
        let slot = (self.head + self.len) % QUEUE_CAPACITY;
        self.data[slot] = Some(c);
        self.len += 1;
    }

    fn pop(&mut self) -> Option<char> {
        if self.len == 0 {
            return None;
        }
        let c = self.data[self.head].take();
        self.head = (self.head + 1) % QUEUE_CAPACITY;
        self.len -= 1;
        c
    }
}

/// Feeds a raw scancode into the decoder. Called from the IRQ1 handler.
pub fn add_scancode(scancode: u8) {
    let decoded = {
        let mut keyboard = KEYBOARD.lock();
        match keyboard.add_byte(scancode) {
            Ok(Some(key_event)) => keyboard.process_keyevent(key_event),
            _ => None,
        }
    };
    let Some(decoded) = decoded else { return };
    if let DecodedKey::Unicode(c) = decoded {
        KEY_QUEUE.lock().push(c);
    }
}

/// Drains one decoded character, if any.
pub fn pop_key() -> Option<char> {
    KEY_QUEUE.lock().pop()
}
