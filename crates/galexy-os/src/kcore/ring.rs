//! Fixed-capacity FIFO ring buffer (no heap required).

/// A ring buffer holding up to `N` values, dropping the newest on overflow.
///
/// Capacity is compile-time; all allocation-free. Intended for IRQ-produced /
/// main-loop-consumed queues (keyboard input now; scheduler queues later).
pub struct Ring<T, const N: usize> {
    data: [Option<T>; N],
    head: usize,
    len: usize,
}

impl<T, const N: usize> Ring<T, N> {
    /// Creates an empty ring.
    pub const fn new() -> Self {
        Ring {
            data: [const { None }; N],
            head: 0,
            len: 0,
        }
    }

    /// Pushes a value; drops it (and returns `None`) if the ring is full.
    pub fn push(&mut self, value: T) -> Option<()> {
        if self.len >= N {
            // Overflow: drop the newest rather than corrupt state.
            return None;
        }
        let slot = (self.head + self.len) % N;
        self.data[slot] = Some(value);
        self.len += 1;
        Some(())
    }

    /// Pops the oldest value, if any.
    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let value = self.data[self.head].take();
        self.head = (self.head + 1) % N;
        self.len -= 1;
        value
    }
}
