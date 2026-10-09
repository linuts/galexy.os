//! Virtio-input keyboard.
//!
//! QEMU: `-device virtio-keyboard-pci`. Events are Linux evdev key codes.
//! They are translated to i8042 set-1 scancodes and fed to
//! [`super::keyboard::add_scancode`], so shift, ctrl, and arrows share the
//! PS/2 decoder. When no virtio-input function is present the caller keeps
//! the i8042.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use spin::Mutex;
use x86_64::VirtAddr;

use super::pci;
use super::virtio_pci;
use crate::arch::mm;

const VIRTIO_VENDOR: u16 = 0x1AF4;
/// Modern virtio-input (device id 18): `0x1040 + 18`.
const VIRTIO_INPUT_DEVICE: u16 = 0x1052;

/// IDT vector for the virtio-input queue interrupt.
pub const VECTOR: u8 = 0x42;

/// Event slots. A QMP chord is press + release + `EV_SYN` each, and the
/// shell image is still loading when the first keys arrive, so the ring
/// has to absorb a short burst before the IRQ drains it.
const Q: usize = 64;
const DESC_F_WRITE: u16 = 2;
const EV_KEY: u16 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
struct Avail {
    flags: u16,
    idx: u16,
    ring: [u16; Q],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UsedElem {
    id: u32,
    len: u32,
}

#[repr(C)]
struct Used {
    flags: u16,
    idx: u16,
    ring: [UsedElem; Q],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Event {
    type_: u16,
    code: u16,
    value: u32,
}

#[repr(C, align(4096))]
struct Dma {
    desc: [Desc; Q],
    avail: Avail,
    used: Used,
    events: [Event; Q],
}

struct State {
    modern: virtio_pci::Modern,
    notify_off: u16,
    last_used: u16,
    qsize: u16,
}

static PROBED: AtomicBool = AtomicBool::new(false);
static READY: AtomicBool = AtomicBool::new(false);
static INTX: AtomicBool = AtomicBool::new(false);
static ISR: AtomicU64 = AtomicU64::new(0);
static DEV: Mutex<Option<State>> = Mutex::new(None);
static DMA: Mutex<Dma> = Mutex::new(Dma {
    desc: [Desc {
        addr: 0,
        len: 0,
        flags: 0,
        next: 0,
    }; Q],
    avail: Avail {
        flags: 0,
        idx: 0,
        ring: [0; Q],
    },
    used: Used {
        flags: 0,
        idx: 0,
        ring: [UsedElem { id: 0, len: 0 }; Q],
    },
    events: [Event {
        type_: 0,
        code: 0,
        value: 0,
    }; Q],
});

/// True when a virtio-input keyboard owns the key path.
pub fn present() -> bool {
    READY.load(Ordering::Acquire)
}

/// True when the queue interrupt is an INTx line (I/O APIC EOI required).
pub fn intx_routed() -> bool {
    INTX.load(Ordering::Acquire)
}

/// Probes PCI for a virtio keyboard and brings the event queue up.
///
/// Safe to call once, with interrupts still masked, from keyboard init.
pub fn probe() -> bool {
    if PROBED.swap(true, Ordering::AcqRel) {
        return READY.load(Ordering::Acquire);
    }
    let ok = bring_up();
    READY.store(ok, Ordering::Release);
    ok
}

/// Drains used input events. Called from the virtio-input interrupt.
pub fn on_irq() {
    let isr = ISR.load(Ordering::Acquire);
    if isr != 0 {
        let _ = virtio_pci::ack_isr(isr);
    }
    let mut pending = [Event {
        type_: 0,
        code: 0,
        value: 0,
    }; Q];
    let mut n = 0usize;
    {
        let mut dev = DEV.lock();
        let Some(state) = dev.as_mut() else {
            return;
        };
        let mut dma = DMA.lock();
        // SAFETY: the device writes `used.idx` and the event slots.
        let idx = unsafe { core::ptr::read_volatile(&dma.used.idx) };
        let qsize = usize::from(state.qsize).max(1);
        while state.last_used != idx && n < Q {
            let slot = (state.last_used as usize) % qsize;
            let id = unsafe { core::ptr::read_volatile(&dma.used.ring[slot].id) } as usize;
            if id < qsize {
                pending[n] = unsafe { core::ptr::read_volatile(&dma.events[id]) };
                n += 1;
                requeue(&mut dma, id as u16, qsize);
            }
            state.last_used = state.last_used.wrapping_add(1);
        }
        if n > 0 {
            virtio_pci::notify(&state.modern, 0, state.notify_off);
        }
    }
    for ev in pending.iter().take(n) {
        if ev.type_ == EV_KEY {
            emit(ev.code, ev.value);
        }
    }
}

fn bring_up() -> bool {
    let Some(pci_dev) = pci::find_any(VIRTIO_VENDOR, &[VIRTIO_INPUT_DEVICE]) else {
        return false;
    };
    pci::enable_bus_master(pci_dev);
    let Some(modern) = virtio_pci::claim(pci_dev) else {
        crate::serial_println!("[kbd] virtio-input has no 1.x capabilities");
        return false;
    };
    if !virtio_pci::negotiate(&modern, 0) {
        crate::serial_println!("[kbd] virtio-input refused VIRTIO_F_VERSION_1");
        return false;
    }
    let mut dma = DMA.lock();
    let desc = phys_of(&dma.desc);
    let avail = phys_of(&dma.avail);
    let used = phys_of(&dma.used);
    let (Some(desc), Some(avail), Some(used)) = (desc, avail, used) else {
        crate::serial_println!("[kbd] virtio-input queue is not mapped");
        return false;
    };
    let max = virtio_pci::max_queue(&modern, 0);
    let qsize = pow2_floor((usize::from(max)).min(Q));
    if qsize < 8 {
        crate::serial_println!("[kbd] virtio-input queue max {} is too small", max);
        return false;
    }
    let Some(notify_off) = virtio_pci::setup_queue(&modern, 0, qsize as u16, desc, avail, used)
    else {
        crate::serial_println!("[kbd] virtio-input queue setup failed");
        return false;
    };
    for i in 0..qsize {
        let Some(addr) = phys_of(&dma.events[i]) else {
            return false;
        };
        // SAFETY: the descriptor table is host-owned until notify.
        unsafe {
            core::ptr::write_volatile(
                &mut dma.desc[i],
                Desc {
                    addr,
                    len: 8,
                    flags: DESC_F_WRITE,
                    next: 0,
                },
            );
            core::ptr::write_volatile(&mut dma.avail.ring[i], i as u16);
        }
    }
    // SAFETY: publish the filled avail ring.
    unsafe {
        core::ptr::write_volatile(&mut dma.avail.idx, qsize as u16);
    }
    drop(dma);
    ISR.store(modern.isr_addr(), Ordering::Release);
    let msix = virtio_pci::enable_msix(
        pci_dev,
        &modern,
        0,
        0,
        VECTOR,
        crate::arch::apic::lapic_id(),
    );
    let route = if msix {
        "msi-x"
    } else {
        let line = pci::interrupt_line(pci_dev);
        if line != 0 && crate::arch::ioapic::wire_pci_level(u32::from(line), VECTOR, "virtio-input")
        {
            INTX.store(true, Ordering::Release);
            "intx"
        } else {
            "no-irq"
        }
    };
    virtio_pci::notify(&modern, 0, notify_off);
    virtio_pci::driver_ok(&modern);
    *DEV.lock() = Some(State {
        modern,
        notify_off,
        last_used: 0,
        qsize: qsize as u16,
    });
    crate::serial_println!("[kbd] virtio-input ({})", route);
    true
}

fn requeue(dma: &mut Dma, id: u16, qsize: usize) {
    // SAFETY: the avail ring is host-written; the device reads it after notify.
    let avail_idx = unsafe { core::ptr::read_volatile(&dma.avail.idx) };
    let slot = (avail_idx as usize) % qsize;
    unsafe {
        core::ptr::write_volatile(&mut dma.avail.ring[slot], id);
        core::ptr::write_volatile(&mut dma.avail.idx, avail_idx.wrapping_add(1));
    }
}

fn phys_of<T>(r: &T) -> Option<u64> {
    mm::translate(VirtAddr::new(core::ptr::from_ref(r) as u64)).map(|p| p.as_u64())
}

fn emit(code: u16, value: u32) {
    // 2 is autorepeat; a chord from QMP is press then release.
    if value == 2 {
        return;
    }
    let Some((extended, make)) = set1(code) else {
        return;
    };
    let byte = if value == 0 { make | 0x80 } else { make };
    if extended {
        super::keyboard::add_scancode(0xE0);
    }
    super::keyboard::add_scancode(byte);
}

fn pow2_floor(n: usize) -> usize {
    if n < 2 {
        return 0;
    }
    1usize << (usize::BITS - 1 - n.leading_zeros())
}

/// Linux evdev code to a set-1 make code. `true` means an `E0` prefix.
fn set1(code: u16) -> Option<(bool, u8)> {
    // The main block through F10 uses the same byte as the evdev code.
    if (1..=68).contains(&code) {
        return Some((false, code as u8));
    }
    Some(match code {
        87 => (false, 0x57), // F11
        88 => (false, 0x58), // F12
        96 => (true, 0x1C),  // keypad enter
        97 => (true, 0x1D),  // right ctrl
        98 => (true, 0x35),  // keypad slash
        100 => (true, 0x38), // right alt
        102 => (true, 0x47), // home
        103 => (true, 0x48), // up
        104 => (true, 0x49), // page up
        105 => (true, 0x4B), // left
        106 => (true, 0x4D), // right
        107 => (true, 0x4F), // end
        108 => (true, 0x50), // down
        109 => (true, 0x51), // page down
        110 => (true, 0x52), // insert
        111 => (true, 0x53), // delete
        _ => return None,
    })
}
