//! xHCI on q35 (`qemu-xhci`, PCI `1b36:000d`). One controller.
//!
//! The kernel moves transfer rings and copies descriptor bytes to the
//! task named `usb`. It does not match a driver. The interrupt handler
//! acknowledges the interrupter and wakes that task. A buffer that
//! crosses a physical gap is one TRB per contiguous run.

use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};

use x86_64::instructions::interrupts;
use x86_64::VirtAddr;

use super::super::pci;
use crate::arch::mm;
use crate::sync::Mutex;
use galexy_abi::SysError;

pub const VECTOR: u8 = 0x44;

const VENDOR: u16 = 0x1b36;
const DEVICE: u16 = 0x000d;
const MAP_LEN: usize = 64 * 1024;
const SLOTS: usize = 8;
const TRBS: usize = 32;
const EVENTS: usize = 64;
const CFG_STORE: usize = 256;
const STR_STORE: usize = 64;
const DEV_STORE: usize = 18;

const TRB_NORMAL: u32 = 1;
const TRB_SETUP: u32 = 2;
const TRB_DATA: u32 = 3;
const TRB_STATUS: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_ENABLE: u32 = 9;
const TRB_ADDR: u32 = 11;
const TRB_CONFIG: u32 = 12;
const TRB_EVAL: u32 = 13;
const TRB_NOOP: u32 = 23;
const EVT_XFER: u32 = 32;
const EVT_CMD: u32 = 33;

const USBCMD_RS: u32 = 1;
const USBCMD_HCRST: u32 = 1 << 1;
const USBCMD_INTE: u32 = 1 << 2;
const USBSTS_HCH: u32 = 1;
const USBSTS_EINT: u32 = 1 << 3;
const USBSTS_CNR: u32 = 1 << 11;
const PORT_CCS: u32 = 1;
const PORT_PED: u32 = 1 << 1;
const PORT_PR: u32 = 1 << 4;
const PORT_PP: u32 = 1 << 9;
const PORT_PRC: u32 = 1 << 21;
const PORT_CHANGE: u32 = 0x00FE_0000;

static PROBED: AtomicBool = AtomicBool::new(false);
static READY: AtomicBool = AtomicBool::new(false);
static LINK: AtomicBool = AtomicBool::new(false);
static INTX: AtomicBool = AtomicBool::new(false);
static IRQS: AtomicU64 = AtomicU64::new(0);
static LAST_DESCS: AtomicU16 = AtomicU16::new(0);
static CAP_VIRT: AtomicU64 = AtomicU64::new(0);
static OP_VIRT: AtomicU64 = AtomicU64::new(0);
static RT_VIRT: AtomicU64 = AtomicU64::new(0);

#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct Trb {
    raw: [u32; 4],
}

impl Trb {
    const fn empty() -> Self {
        Self { raw: [0; 4] }
    }
}

#[repr(C, align(64))]
struct Ring {
    trb: [Trb; TRBS],
    enq: u8,
    cycle: u8,
}

impl Ring {
    const fn empty() -> Self {
        Self {
            trb: [const { Trb::empty() }; TRBS],
            enq: 0,
            cycle: 1,
        }
    }
}

#[repr(C, align(64))]
struct EventRing {
    trb: [Trb; EVENTS],
    deq: u8,
    cycle: u8,
}

#[repr(C, align(64))]
struct Erst {
    base: u64,
    size: u32,
    _pad: u32,
}

#[repr(C, align(64))]
struct Dcbaa {
    ptr: [u64; SLOTS + 1],
}

#[repr(C, align(64))]
struct Context {
    bytes: [u8; 2048],
}

struct DevRec {
    used: bool,
    port: u8,
    addr: u8,
    slot: u8,
    flags: u8,
    dev: [u8; DEV_STORE],
    cfg_len: u16,
    cfg: [u8; CFG_STORE],
    str_len: u8,
    text: [u8; STR_STORE],
}

impl DevRec {
    const fn empty() -> Self {
        Self {
            used: false,
            port: 0,
            addr: 0,
            slot: 0,
            flags: 0,
            dev: [0; DEV_STORE],
            cfg_len: 0,
            cfg: [0; CFG_STORE],
            str_len: 0,
            text: [0; STR_STORE],
        }
    }
}

/// Device memory. `dma` is first so a 4096-aligned `Ctrl` keeps it in
/// one physical page. Do not move this value; the controller holds its
/// physical addresses.
#[repr(C, align(4096))]
struct Ctrl {
    dma: [u8; 512],
    cap: u64,
    op: u64,
    db: u64,
    rt: u64,
    ports: u8,
    ctx: u32,
    tried: u16,
    cmd: Ring,
    event: EventRing,
    erst: Erst,
    dcbaa: Dcbaa,
    ep: [Ring; SLOTS],
    hid: [Ring; SLOTS],
    report: [u8; 8],
    hid_prev: [u8; 6],
    hid_slot: u8,
    hid_on: bool,
    out_ctx: [Context; SLOTS],
    in_ctx: [Context; SLOTS],
    rec: [DevRec; SLOTS],
    published: u8,
    cursor: u8,
    cmd_done: bool,
    cmd_code: u8,
    cmd_slot: u8,
    xfer_done: bool,
    xfer_code: u8,
}

impl Ctrl {
    const fn empty() -> Self {
        Self {
            dma: [0; 512],
            cap: 0,
            op: 0,
            db: 0,
            rt: 0,
            ports: 0,
            ctx: 32,
            tried: 0,
            cmd: Ring::empty(),
            event: EventRing {
                trb: [const { Trb::empty() }; EVENTS],
                deq: 0,
                cycle: 1,
            },
            erst: Erst {
                base: 0,
                size: EVENTS as u32,
                _pad: 0,
            },
            dcbaa: Dcbaa {
                ptr: [0; SLOTS + 1],
            },
            ep: [const { Ring::empty() }; SLOTS],
            hid: [const { Ring::empty() }; SLOTS],
            report: [0; 8],
            hid_prev: [0; 6],
            hid_slot: 0,
            hid_on: false,
            out_ctx: [const { Context { bytes: [0; 2048] } }; SLOTS],
            in_ctx: [const { Context { bytes: [0; 2048] } }; SLOTS],
            rec: [const { DevRec::empty() }; SLOTS],
            published: 0,
            cursor: 0,
            cmd_done: false,
            cmd_code: 0,
            cmd_slot: 0,
            xfer_done: false,
            xfer_code: 0,
        }
    }
}

static CTRL: Mutex<Ctrl> = Mutex::new(Ctrl::empty());

/// True after the controller has been reset and started.
pub fn present() -> bool {
    if PROBED.load(Ordering::Acquire) {
        return READY.load(Ordering::Acquire);
    }
    PROBED.store(true, Ordering::Release);
    let ok = probe();
    READY.store(ok, Ordering::Release);
    ok
}

/// Controller left the halted state.
pub fn link_up() -> bool {
    present() && LINK.load(Ordering::Acquire)
}

pub fn irq_count() -> u64 {
    IRQS.load(Ordering::Acquire)
}

pub fn intx_routed() -> bool {
    INTX.load(Ordering::Acquire)
}

pub fn last_transfer_descs() -> u16 {
    LAST_DESCS.load(Ordering::Acquire)
}

/// True when a record is waiting. Does not touch the rings.
pub fn has_record() -> bool {
    if !READY.load(Ordering::Acquire) {
        return false;
    }
    let ctrl = CTRL.lock();
    ctrl.cursor < ctrl.published
}

/// Builds the data-TRB chain a straddling buffer would use.
///
/// Does not ring a doorbell. No TRB length reaches a following frame.
pub fn stage_transfer(buf: &[u8]) -> Result<u16, SysError> {
    if buf.is_empty() || buf.len() > 4096 {
        return Err(SysError::BadValue);
    }
    let (_runs, n) = collect_runs(buf)?;
    LAST_DESCS.store(u16::from(n), Ordering::Release);
    Ok(u16::from(n))
}

/// Copies the next device record. `Ok(0)` means none is ready yet.
///
/// Layout: `port u8`, `addr u8`, `flags u8` (bit 0 = bad), 18 device
/// bytes, `cfg_len u16le`, that many configuration bytes, `str_len u8`,
/// that many raw string-descriptor bytes.
pub fn read_device(dst: &mut [u8]) -> Result<usize, SysError> {
    if !present() {
        return Err(SysError::Unsupported);
    }
    poll_keyboard();
    pump(4);
    let mut ctrl = CTRL.lock();
    if ctrl.cursor >= ctrl.published {
        return Ok(0);
    }
    let rec = &ctrl.rec[ctrl.cursor as usize];
    let need = record_len(rec);
    if dst.len() < need {
        return Err(SysError::BadValue);
    }
    let n = encode(rec, dst);
    ctrl.cursor += 1;
    Ok(n)
}

/// Interrupt body. Acks the interrupter. Does not read a descriptor.
pub fn on_irq() {
    let op = OP_VIRT.load(Ordering::Acquire);
    let rt = RT_VIRT.load(Ordering::Acquire);
    if op != 0 {
        let sts = read32(op, 4);
        if sts & USBSTS_EINT != 0 {
            write32(op, 4, USBSTS_EINT);
        }
    }
    if rt != 0 {
        let iman = read32(rt, 0x20);
        write32(rt, 0x20, (iman & !1) | 1 | 2);
    }
    IRQS.fetch_add(1, Ordering::Release);
    crate::sched::wake_usb();
}

/// Posts a no-op so the event ring raises the interrupt.
pub fn ping() -> bool {
    if !READY.load(Ordering::Acquire) {
        return false;
    }
    command(0, 0, (TRB_NOOP << 10) | (1 << 5))
}

fn probe() -> bool {
    let Some(dev) = pci::find(VENDOR, DEVICE) else {
        crate::serial_println!("[xhci] no controller");
        return false;
    };
    pci::enable_bus_master(dev);
    let Some(bar) = pci::mem_bar(dev, 0) else {
        crate::serial_println!("[xhci] BAR0 is not memory");
        return false;
    };
    let base = mm::map_mmio(bar, MAP_LEN).as_u64();
    let cap_len = u64::from(read8(base, 0));
    if cap_len < 0x20 {
        crate::serial_println!("[xhci] short capability length");
        return false;
    }
    let op = base + cap_len;
    let db_off = u64::from(read32(base, 0x14) & !0x3);
    let rt_off = u64::from(read32(base, 0x18) & !0x1f);
    if db_off == 0 || rt_off == 0 || db_off >= MAP_LEN as u64 || rt_off >= MAP_LEN as u64 {
        crate::serial_println!("[xhci] doorbell or runtime offset is outside BAR0");
        return false;
    }
    let hcs1 = read32(base, 4);
    let ports = ((hcs1 >> 24) & 0xff) as u8;
    let max_slots = (hcs1 & 0xff) as u8;
    let hcc = read32(base, 0x10);
    let ctx = if hcc & (1 << 2) != 0 { 64 } else { 32 };
    let slots = (max_slots as usize).clamp(1, SLOTS) as u8;
    if !halt_and_reset(op) {
        crate::serial_println!("[xhci] reset did not finish");
        return false;
    }
    let msix = enable_msix(dev);
    let route = if msix {
        "msi-x"
    } else if wire_intx(dev) {
        "intx"
    } else {
        "no-irq"
    };
    {
        let mut ctrl = CTRL.lock();
        ctrl.cap = base;
        ctrl.op = op;
        ctrl.db = base + db_off;
        ctrl.rt = base + rt_off;
        ctrl.ports = ports;
        ctrl.ctx = ctx;
        if program(&mut ctrl, slots).is_err() {
            crate::serial_println!("[xhci] program failed");
            return false;
        }
        CAP_VIRT.store(base, Ordering::Release);
        OP_VIRT.store(op, Ordering::Release);
        RT_VIRT.store(ctrl.rt, Ordering::Release);
        write32(ctrl.rt, 0x24, 0);
        write32(ctrl.rt, 0x20, 2);
        write32(op, 0, USBCMD_RS | USBCMD_INTE);
        if !wait_reg(op, 200, |sts| {
            sts & USBSTS_HCH == 0 && sts & USBSTS_CNR == 0
        }) {
            crate::serial_println!("[xhci] controller stayed halted");
            return false;
        }
        LINK.store(true, Ordering::Release);
    }
    let before = IRQS.load(Ordering::Acquire);
    let ping_ok = ping();
    let fired = ping_ok && IRQS.load(Ordering::Acquire) > before;
    crate::serial_println!(
        "[xhci] link up ports={} slots={} ctx={} ({}) irq {}",
        ports,
        slots,
        ctx,
        route,
        if fired { "fired" } else { "quiet" }
    );
    true
}

fn program(ctrl: &mut Ctrl, slots: u8) -> Result<(), SysError> {
    let scratch = scratchpad_count(ctrl.cap);
    if scratch > 0 {
        ctrl.dcbaa.ptr[0] = scratchpad_array(scratch)?;
    }
    ctrl.erst.base = phys_of(&ctrl.event.trb)?;
    ctrl.erst.size = EVENTS as u32;
    let erst_phys = phys_of(&ctrl.erst)?;
    let cmd_phys = phys_of(&ctrl.cmd.trb)?;
    seal(&mut ctrl.cmd);
    let dcbaa_phys = phys_of(&ctrl.dcbaa)?;
    write32(ctrl.op, 0x38, u32::from(slots));
    write64(ctrl.op, 0x30, dcbaa_phys);
    write32(ctrl.rt, 0x28, 1);
    write64(ctrl.rt, 0x30, erst_phys);
    write64(ctrl.rt, 0x38, ctrl.erst.base | (1 << 3));
    write64(ctrl.op, 0x18, cmd_phys | 1);
    let ports = ctrl.ports.min(16);
    for port in 1..=ports {
        let sc = portsc(ctrl, port);
        if sc & PORT_PP == 0 {
            write_port(ctrl, port, (sc & !PORT_CHANGE) | PORT_PP);
        }
    }
    Ok(())
}

fn scratchpad_count(cap: u64) -> u32 {
    let hcs2 = read32(cap, 8);
    let lo = (hcs2 >> 27) & 0x1f;
    let hi = (hcs2 >> 21) & 0x1f;
    (hi << 5) | lo
}

fn scratchpad_array(count: u32) -> Result<u64, SysError> {
    let pages = count.min(4) as usize;
    let array = mm::allocate_frame().ok_or(SysError::NoResource)?;
    let virt = mm::frame_virt(array.start_address());
    // SAFETY: a fresh frame, written before the controller is told about it.
    unsafe { core::ptr::write_bytes(virt.as_mut_ptr::<u8>(), 0, 4096) };
    for i in 0..pages {
        let page = mm::allocate_frame().ok_or(SysError::NoResource)?;
        let page_virt = mm::frame_virt(page.start_address());
        unsafe { core::ptr::write_bytes(page_virt.as_mut_ptr::<u8>(), 0, 4096) };
        unsafe {
            virt.as_mut_ptr::<u64>()
                .add(i)
                .write_volatile(page.start_address().as_u64());
        }
    }
    Ok(array.start_address().as_u64())
}

fn halt_and_reset(op: u64) -> bool {
    if read32(op, 0) & USBCMD_RS != 0 {
        write32(op, 0, read32(op, 0) & !USBCMD_RS);
        if !wait_reg(op, 100, |sts| sts & USBSTS_HCH != 0) {
            return false;
        }
    }
    write32(op, 0, USBCMD_HCRST);
    wait_reg(op, 200, |sts| {
        read32(op, 0) & USBCMD_HCRST == 0 && sts & USBSTS_CNR == 0
    })
}

fn wait_reg(op: u64, ms: u64, pred: impl Fn(u32) -> bool) -> bool {
    let start = crate::arch::timer_ticks();
    let enabled = interrupts::are_enabled();
    for _ in 0..4_000_000 {
        if pred(read32(op, 4)) {
            if enabled {
                interrupts::enable();
            }
            return true;
        }
        if crate::arch::timer_ticks().saturating_sub(start) > ms {
            break;
        }
        if enabled || crate::arch::timer_ticks() != start {
            crate::sched::halt_for_device();
            interrupts::disable();
        } else {
            core::hint::spin_loop();
        }
    }
    if enabled {
        interrupts::enable();
    }
    pred(read32(op, 4))
}

fn pump(steps: u8) {
    for _ in 0..steps {
        let step = {
            let mut ctrl = CTRL.lock();
            drain(&mut ctrl);
            advance(&mut ctrl)
        };
        match step {
            Step::Idle | Step::Wait => break,
            Step::Go(port) => {
                if !enumerate(port) {
                    let mut ctrl = CTRL.lock();
                    ctrl.tried |= 1 << (port.saturating_sub(1));
                }
            }
        }
    }
}

enum Step {
    Idle,
    Wait,
    Go(u8),
}

fn advance(ctrl: &mut Ctrl) -> Step {
    if ctrl.published as usize >= SLOTS {
        return Step::Idle;
    }
    let ports = ctrl.ports.min(16);
    for port in 1..=ports {
        if ctrl.tried & (1 << (port - 1)) != 0 {
            continue;
        }
        if ctrl.rec.iter().any(|r| r.used && r.port == port) {
            continue;
        }
        let sc = portsc(ctrl, port);
        if sc & PORT_CCS == 0 {
            continue;
        }
        if sc & PORT_PED == 0 {
            if sc & PORT_PRC != 0 {
                write_port(ctrl, port, (sc & !PORT_CHANGE) | PORT_PRC);
            }
            if sc & PORT_PR == 0 {
                let fresh = portsc(ctrl, port);
                write_port(
                    ctrl,
                    port,
                    (fresh & !PORT_CHANGE & !PORT_PED) | PORT_PR | PORT_PP,
                );
                crate::serial_println!("[xhci] reset port {} sc={:#x}", port, sc);
            }
            return Step::Wait;
        }
        return Step::Go(port);
    }
    Step::Idle
}

fn enumerate(port: u8) -> bool {
    let speed = {
        let ctrl = CTRL.lock();
        ((portsc(&ctrl, port) >> 10) & 0xf) as u8
    };
    crate::serial_println!("[xhci] enumerate port {} speed {}", port, speed);
    if !command(0, 0, TRB_ENABLE << 10) {
        crate::serial_println!("[xhci] enable slot timed out on port {}", port);
        return false;
    }
    let (slot, code) = {
        let ctrl = CTRL.lock();
        (ctrl.cmd_slot, ctrl.cmd_code)
    };
    if code != 1 || slot == 0 || slot as usize > SLOTS {
        crate::serial_println!("[xhci] enable slot failed port {} code {}", port, code);
        return false;
    }
    let mps = match speed {
        3 => 64,
        4 | 5 => 512,
        _ => 8,
    };
    let input = {
        let mut ctrl = CTRL.lock();
        if prep_slot(&mut ctrl, slot, port, speed, mps, true).is_err() {
            return false;
        }
        match phys_of(&ctrl.in_ctx[slot as usize - 1]) {
            Ok(p) => p,
            Err(_) => return false,
        }
    };
    if !command(input, 0, (TRB_ADDR << 10) | (u32::from(slot) << 24)) {
        crate::serial_println!("[xhci] address device timed out slot {}", slot);
        return false;
    }
    let (code, addr) = {
        let ctrl = CTRL.lock();
        (ctrl.cmd_code, slot_addr(&ctrl, slot))
    };
    if code != 1 {
        crate::serial_println!("[xhci] address device failed slot {} code {}", slot, code);
        return false;
    }
    if !control_in(slot, setup(0x80, 6, 0x0100, 0, 8), 8) {
        crate::serial_println!("[xhci] device descriptor failed slot {}", slot);
        return false;
    }
    let got_mps = {
        let ctrl = CTRL.lock();
        dma_byte(&ctrl, 7)
    };
    if got_mps != 0 && u16::from(got_mps) != mps {
        {
            let mut ctrl = CTRL.lock();
            let _ = prep_slot(&mut ctrl, slot, port, speed, u16::from(got_mps), false);
        }
        let _ = command(input, 0, (TRB_EVAL << 10) | (u32::from(slot) << 24));
    }
    if !control_in(slot, setup(0x80, 6, 0x0100, 0, 18), 18) {
        crate::serial_println!("[xhci] full device descriptor failed slot {}", slot);
        return false;
    }
    let mut dev = [0u8; DEV_STORE];
    {
        let ctrl = CTRL.lock();
        dma_copy(&ctrl, &mut dev);
    }
    if !control_in(slot, setup(0x80, 6, 0x0200, 0, 9), 9) {
        let mut ctrl = CTRL.lock();
        publish(&mut ctrl, slot, port, addr, 1, &dev, &[], &[]);
        return true;
    }
    let declared = {
        let ctrl = CTRL.lock();
        u16::from_le_bytes([dma_byte(&ctrl, 2), dma_byte(&ctrl, 3)])
    };
    if (declared as usize) > 4096 || declared < 9 {
        let mut ctrl = CTRL.lock();
        publish(&mut ctrl, slot, port, addr, 1, &dev, &[], &[]);
        return true;
    }
    let want = {
        let ctrl = CTRL.lock();
        (declared as usize).min(ctrl.dma.len())
    };
    if !control_in(slot, setup(0x80, 6, 0x0200, 0, want as u16), want) {
        let mut ctrl = CTRL.lock();
        publish(&mut ctrl, slot, port, addr, 1, &dev, &[], &[]);
        return true;
    }
    let cfg_n = want.min(CFG_STORE);
    let mut cfg = [0u8; CFG_STORE];
    {
        let ctrl = CTRL.lock();
        dma_copy(&ctrl, &mut cfg[..cfg_n]);
    }
    let mut text = [0u8; STR_STORE];
    let mut flags = 0u8;
    let str_len = match product_string(slot, dev[15], &mut text) {
        Some(n) => n,
        None => {
            flags |= 1;
            0
        }
    };
    let mut ctrl = CTRL.lock();
    publish(
        &mut ctrl,
        slot,
        port,
        addr,
        flags,
        &dev,
        &cfg[..cfg_n],
        &text[..str_len],
    );
    true
}

fn product_string(slot: u8, index: u8, text: &mut [u8]) -> Option<usize> {
    if index == 0 {
        return Some(0);
    }
    if !control_in(slot, setup(0x80, 6, 0x0300, 0, 4), 4) {
        return None;
    }
    let lang = {
        let ctrl = CTRL.lock();
        if dma_byte(&ctrl, 0) >= 4 && dma_byte(&ctrl, 1) == 3 {
            u16::from_le_bytes([dma_byte(&ctrl, 2), dma_byte(&ctrl, 3)])
        } else {
            return None;
        }
    };
    if !control_in(slot, setup(0x80, 6, 0x0300 | u16::from(index), lang, 2), 2) {
        return None;
    }
    let len = {
        let ctrl = CTRL.lock();
        let len = dma_byte(&ctrl, 0) as usize;
        if !(2..=255).contains(&len) || dma_byte(&ctrl, 1) != 3 || !len.is_multiple_of(2) {
            return None;
        }
        len.min(text.len())
    };
    if !control_in(
        slot,
        setup(0x80, 6, 0x0300 | u16::from(index), lang, len as u16),
        len,
    ) {
        return None;
    }
    let ctrl = CTRL.lock();
    dma_copy(&ctrl, &mut text[..len]);
    Some(len)
}

#[allow(clippy::too_many_arguments)]
fn publish(
    ctrl: &mut Ctrl,
    slot: u8,
    port: u8,
    addr: u8,
    flags: u8,
    dev: &[u8],
    cfg: &[u8],
    text: &[u8],
) {
    let i = ctrl.published as usize;
    if i >= SLOTS {
        return;
    }
    let rec = &mut ctrl.rec[i];
    *rec = DevRec::empty();
    rec.used = true;
    rec.port = port;
    rec.addr = addr;
    rec.slot = slot;
    rec.flags = flags;
    let dn = dev.len().min(DEV_STORE);
    rec.dev[..dn].copy_from_slice(&dev[..dn]);
    let cn = cfg.len().min(CFG_STORE);
    rec.cfg_len = cn as u16;
    rec.cfg[..cn].copy_from_slice(&cfg[..cn]);
    let sn = text.len().min(STR_STORE);
    rec.str_len = sn as u8;
    rec.text[..sn].copy_from_slice(&text[..sn]);
    ctrl.published += 1;
    ctrl.tried |= 1 << (port - 1);
    crate::serial_println!("[xhci] device port={} addr={} flags={}", port, addr, flags);
}

fn prep_slot(
    ctrl: &mut Ctrl,
    slot: u8,
    port: u8,
    speed: u8,
    mps: u16,
    fresh: bool,
) -> Result<(), SysError> {
    let idx = slot as usize - 1;
    ctrl.in_ctx[idx].bytes.fill(0);
    if fresh {
        ctrl.out_ctx[idx].bytes.fill(0);
        ctrl.ep[idx] = Ring::empty();
        seal(&mut ctrl.ep[idx]);
    }
    let shift = ctrl.ctx as usize;
    // Add flags: slot (bit 0) and EP0 (bit 1) when addressing. Evaluate
    // touches EP0 only, and keeps the transfer ring where it is.
    let add = if fresh { 3 } else { 2 };
    write_ctx(&mut ctrl.in_ctx[idx].bytes, 1, add);
    let ep = &ctrl.ep[idx];
    let dq = if fresh {
        phys_of(&ep.trb)? | 1
    } else {
        phys_of(&ep.trb[ep.enq as usize])? | u64::from(ep.cycle)
    };
    write_ctx(
        &mut ctrl.in_ctx[idx].bytes[shift..],
        0,
        (u32::from(speed) << 20) | (1 << 27),
    );
    write_ctx(
        &mut ctrl.in_ctx[idx].bytes[shift..],
        1,
        u32::from(port) << 16,
    );
    let ep_off = shift * 2;
    let d1 = (3 << 1) | (4 << 3) | (u32::from(mps) << 16);
    write_ctx(&mut ctrl.in_ctx[idx].bytes[ep_off..], 1, d1);
    write_ctx(&mut ctrl.in_ctx[idx].bytes[ep_off..], 2, dq as u32);
    write_ctx(&mut ctrl.in_ctx[idx].bytes[ep_off..], 3, (dq >> 32) as u32);
    write_ctx(&mut ctrl.in_ctx[idx].bytes[ep_off..], 4, 8);
    ctrl.dcbaa.ptr[slot as usize] = phys_of(&ctrl.out_ctx[idx])?;
    Ok(())
}

fn slot_addr(ctrl: &Ctrl, slot: u8) -> u8 {
    // Output device context starts at the slot context. Byte 12 is the
    // USB device address. The input context's leading control block is
    // not present here.
    let bytes = &ctrl.out_ctx[slot as usize - 1].bytes;
    unsafe { core::ptr::read_volatile(bytes.as_ptr().add(12)) }
}

fn control_in(slot: u8, setup_data: u64, len: usize) -> bool {
    {
        let mut ctrl = CTRL.lock();
        if slot == 0 || slot as usize > SLOTS || len == 0 || len > ctrl.dma.len() {
            return false;
        }
        let idx = slot as usize - 1;
        let Ok(data_phys) = phys_of(&ctrl.dma[..len]) else {
            return false;
        };
        // One data TRB: the DMA page does not cross a physical gap. CH stays
        // clear so the status stage is not part of the data TD.
        push(
            &mut ctrl.ep[idx],
            setup_data,
            8,
            (TRB_SETUP << 10) | (1 << 6) | (3 << 16),
        );
        push(
            &mut ctrl.ep[idx],
            data_phys,
            len as u32,
            (TRB_DATA << 10) | (1 << 16),
        );
        push(&mut ctrl.ep[idx], 0, 0, (TRB_STATUS << 10) | (1 << 5));
        ctrl.xfer_done = false;
        ctrl.xfer_code = 0;
        ring_db(&ctrl, slot, 1);
    }
    if !wait_flag(true) {
        return false;
    }
    let ctrl = CTRL.lock();
    ctrl.xfer_code == 1 || ctrl.xfer_code == 13
}

fn command(param: u64, status: u32, ctrl_bits: u32) -> bool {
    {
        let mut ctrl = CTRL.lock();
        ctrl.cmd_done = false;
        ctrl.cmd_code = 0;
        ctrl.cmd_slot = 0;
        push(&mut ctrl.cmd, param, status, ctrl_bits);
        ring_db(&ctrl, 0, 0);
    }
    wait_flag(false)
}

/// Drops the controller lock before `hlt`, so the handler can take
/// `THREADS` without inverting the lock order.
fn wait_flag(xfer: bool) -> bool {
    let start = crate::arch::timer_ticks();
    loop {
        {
            let mut ctrl = CTRL.lock();
            drain(&mut ctrl);
            let done = if xfer { ctrl.xfer_done } else { ctrl.cmd_done };
            if done {
                return true;
            }
        }
        if crate::arch::timer_ticks().saturating_sub(start) > 500 {
            return false;
        }
        crate::sched::halt_for_device();
        interrupts::disable();
    }
}

fn drain(ctrl: &mut Ctrl) {
    for _ in 0..EVENTS {
        let i = ctrl.event.deq as usize;
        let raw = &ctrl.event.trb[i].raw;
        // Cycle is the device's last store. Read it after the payload.
        let p0 = unsafe { core::ptr::read_volatile(&raw[0]) };
        let p1 = unsafe { core::ptr::read_volatile(&raw[1]) };
        let st = unsafe { core::ptr::read_volatile(&raw[2]) };
        core::sync::atomic::fence(Ordering::Acquire);
        let cw = unsafe { core::ptr::read_volatile(&raw[3]) };
        if (cw & 1) != u32::from(ctrl.event.cycle) {
            break;
        }
        let kind = (cw >> 10) & 0x3f;
        let code = (st >> 24) as u8;
        let slot = (cw >> 24) as u8;
        if kind == EVT_CMD {
            ctrl.cmd_done = true;
            ctrl.cmd_code = code;
            ctrl.cmd_slot = slot;
        } else if kind == EVT_XFER {
            ctrl.xfer_done = true;
            ctrl.xfer_code = code;
        }
        let _ = (p0, p1);
        let next = ctrl.event.deq.wrapping_add(1);
        if next as usize == EVENTS {
            ctrl.event.deq = 0;
            ctrl.event.cycle ^= 1;
        } else {
            ctrl.event.deq = next;
        }
        let Ok(phys) = phys_of(&ctrl.event.trb[ctrl.event.deq as usize]) else {
            break;
        };
        write64(ctrl.rt, 0x38, phys | (1 << 3));
    }
}

fn seal(ring: &mut Ring) {
    let Ok(base) = phys_of(&ring.trb) else {
        return;
    };
    let link = (TRB_LINK << 10) | (1 << 1) | u32::from(ring.cycle);
    write_trb(&mut ring.trb[TRBS - 1], base, 0, link);
}

fn push(ring: &mut Ring, param: u64, status: u32, ctrl_bits: u32) {
    if ring.enq as usize + 1 >= TRBS {
        // The link TRB keeps the cycle the device is currently walking.
        // The producer toggles after it, so the next TRB is a new pass.
        seal(ring);
        ring.cycle ^= 1;
        ring.enq = 0;
    }
    write_trb(
        &mut ring.trb[ring.enq as usize],
        param,
        status,
        ctrl_bits | u32::from(ring.cycle),
    );
    ring.enq = ring.enq.wrapping_add(1);
}

fn write_trb(trb: &mut Trb, param: u64, status: u32, ctrl: u32) {
    unsafe {
        core::ptr::write_volatile(&mut trb.raw[0], param as u32);
        core::ptr::write_volatile(&mut trb.raw[1], (param >> 32) as u32);
        core::ptr::write_volatile(&mut trb.raw[2], status);
        core::sync::atomic::fence(Ordering::Release);
        core::ptr::write_volatile(&mut trb.raw[3], ctrl);
    }
}

fn ring_db(ctrl: &Ctrl, slot: u8, target: u8) {
    core::sync::atomic::fence(Ordering::SeqCst);
    write32(ctrl.db, u64::from(slot) * 4, u32::from(target));
}

fn portsc(ctrl: &Ctrl, port: u8) -> u32 {
    read32(ctrl.op, 0x400 + u64::from(port - 1) * 0x10)
}

fn write_port(ctrl: &Ctrl, port: u8, value: u32) {
    write32(ctrl.op, 0x400 + u64::from(port - 1) * 0x10, value);
}

fn setup(typ: u8, req: u8, value: u16, index: u16, len: u16) -> u64 {
    let mut b = [0u8; 8];
    b[0] = typ;
    b[1] = req;
    b[2..4].copy_from_slice(&value.to_le_bytes());
    b[4..6].copy_from_slice(&index.to_le_bytes());
    b[6..8].copy_from_slice(&len.to_le_bytes());
    u64::from_le_bytes(b)
}

fn write_ctx(bytes: &mut [u8], dword: usize, value: u32) {
    let at = dword * 4;
    if at + 4 <= bytes.len() {
        bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
}

fn dma_byte(ctrl: &Ctrl, index: usize) -> u8 {
    unsafe { core::ptr::read_volatile(ctrl.dma.as_ptr().add(index)) }
}

fn dma_copy(ctrl: &Ctrl, dst: &mut [u8]) {
    for (i, byte) in dst.iter_mut().enumerate() {
        *byte = dma_byte(ctrl, i);
    }
}

fn record_len(rec: &DevRec) -> usize {
    3 + DEV_STORE + 2 + rec.cfg_len as usize + 1 + rec.str_len as usize
}

fn encode(rec: &DevRec, dst: &mut [u8]) -> usize {
    let cfg = rec.cfg_len as usize;
    let text = rec.str_len as usize;
    let need = record_len(rec);
    dst[0] = rec.port;
    dst[1] = rec.addr;
    dst[2] = rec.flags;
    dst[3..3 + DEV_STORE].copy_from_slice(&rec.dev);
    let at = 3 + DEV_STORE;
    dst[at..at + 2].copy_from_slice(&rec.cfg_len.to_le_bytes());
    dst[at + 2..at + 2 + cfg].copy_from_slice(&rec.cfg[..cfg]);
    let s = at + 2 + cfg;
    dst[s] = rec.str_len;
    dst[s + 1..s + 1 + text].copy_from_slice(&rec.text[..text]);
    need
}

#[derive(Clone, Copy)]
struct Run {
    phys: u64,
    len: u32,
}

fn collect_runs(buf: &[u8]) -> Result<([Run; 8], u8), SysError> {
    let mut runs = [Run { phys: 0, len: 0 }; 8];
    let mut n = 0u8;
    let mut off = 0usize;
    while off < buf.len() {
        let va = (buf.as_ptr() as u64)
            .checked_add(off as u64)
            .ok_or(SysError::BadValue)?;
        let phys = mm::translate(VirtAddr::new(va))
            .ok_or(SysError::Unsupported)?
            .as_u64();
        let page_left = 0x1000 - (va as usize & 0xFFF);
        let chunk = page_left.min(buf.len() - off);
        if n > 0 {
            let prev = &mut runs[n as usize - 1];
            if phys == prev.phys + u64::from(prev.len) {
                prev.len += chunk as u32;
                off += chunk;
                continue;
            }
        }
        if n as usize == runs.len() {
            return Err(SysError::BadValue);
        }
        runs[n as usize] = Run {
            phys,
            len: chunk as u32,
        };
        n += 1;
        off += chunk;
    }
    if n == 0 {
        return Err(SysError::BadValue);
    }
    Ok((runs, n))
}

fn enable_msix(dev: pci::Device) -> bool {
    let Some((cap, table, _vectors)) = msix_table(dev) else {
        return false;
    };
    let ctrl = pci::read_u16(dev.bus, dev.slot, dev.func, cap + 2);
    pci::write_u16(
        dev.bus,
        dev.slot,
        dev.func,
        cap + 2,
        (ctrl & 0x07FF) | (1 << 15) | (1 << 14),
    );
    let dest = crate::arch::apic::lapic_id();
    let addr = 0xFEE0_0000u64 | (u64::from(dest) << 12);
    let slot = table.as_u64();
    unsafe {
        (slot as *mut u32).write_volatile(addr as u32);
        ((slot + 4) as *mut u32).write_volatile((addr >> 32) as u32);
        ((slot + 8) as *mut u32).write_volatile(u32::from(VECTOR));
        ((slot + 12) as *mut u32).write_volatile(0);
    }
    pci::write_u16(
        dev.bus,
        dev.slot,
        dev.func,
        cap + 2,
        (ctrl & 0x07FF) | (1 << 15),
    );
    let mut cmd = pci::read_u16(dev.bus, dev.slot, dev.func, 0x04);
    cmd |= 1 << 10;
    pci::write_u16(dev.bus, dev.slot, dev.func, 0x04, cmd);
    true
}

fn msix_table(dev: pci::Device) -> Option<(u8, VirtAddr, u16)> {
    let mut off = (pci::read_u32(dev.bus, dev.slot, dev.func, 0x34) & 0xFF) as u8;
    let mut guard = 0;
    while off >= 0x40 && guard < 48 {
        let header = pci::read_u32(dev.bus, dev.slot, dev.func, off);
        let id = header as u8;
        let next = (header >> 8) as u8;
        if id == 0x11 {
            let ctrl = (header >> 16) as u16;
            let vectors = (ctrl & 0x07FF) + 1;
            let table = pci::read_u32(dev.bus, dev.slot, dev.func, off + 4);
            let bir = (table & 0x7) as u8;
            let offset = table & !0x7;
            let base = pci::mem_bar(dev, bir)?;
            let phys = base.checked_add(u64::from(offset))?;
            let mapped = mm::map_mmio(phys, usize::from(vectors).saturating_mul(16).max(16));
            return Some((off, mapped, vectors));
        }
        if next == 0 || next == off {
            break;
        }
        off = next;
        guard += 1;
    }
    None
}

fn wire_intx(dev: pci::Device) -> bool {
    let line = pci::interrupt_line(dev);
    if line == 0 || !crate::arch::ioapic::wire_pci_level(u32::from(line), VECTOR, "xhci") {
        return false;
    }
    INTX.store(true, Ordering::Release);
    true
}

/// Arms the interrupt pipe for a HID boot keyboard on `port`.
///
/// The stored configuration must be class 3, subclass 1, protocol 1.
/// A mouse or any other device is [`SysError::BadValue`]. The previous
/// metadata slot is not involved: this only programs the controller.
pub fn arm_keyboard(port: u8) -> Result<(), SysError> {
    if !READY.load(Ordering::Acquire) {
        return Err(SysError::Unsupported);
    }
    let slot = {
        let ctrl = CTRL.lock();
        let Some(rec) = ctrl.rec.iter().find(|r| r.used && r.port == port) else {
            return Err(SysError::NotFound);
        };
        if !boot_keyboard_cfg(&rec.cfg[..rec.cfg_len as usize]) {
            return Err(SysError::BadValue);
        }
        rec.slot
    };
    if slot == 0 || slot as usize > SLOTS {
        return Err(SysError::BadValue);
    }
    {
        let ctrl = CTRL.lock();
        if ctrl.hid_on && ctrl.hid_slot == slot {
            return Ok(());
        }
    }
    if !control_status(slot, setup(0x00, 9, 1, 0, 0)) {
        crate::serial_println!("[xhci] hid set config failed slot {}", slot);
        return Err(SysError::Unsupported);
    }
    if !control_status(slot, setup(0x21, 0x0b, 0, 0, 0)) {
        crate::serial_println!("[xhci] hid set protocol failed slot {}", slot);
    }
    if !control_status(slot, setup(0x21, 0x0a, 0, 0, 0)) {
        crate::serial_println!("[xhci] hid set idle failed slot {}", slot);
    }
    let input = {
        let mut ctrl = CTRL.lock();
        if configure_hid(&mut ctrl, slot).is_err() {
            return Err(SysError::Unsupported);
        }
        phys_of(&ctrl.in_ctx[slot as usize - 1])?
    };
    if !command(input, 0, (TRB_CONFIG << 10) | (u32::from(slot) << 24)) {
        crate::serial_println!("[xhci] hid configure timed out slot {}", slot);
        return Err(SysError::Unsupported);
    }
    {
        let ctrl = CTRL.lock();
        if ctrl.cmd_code != 1 {
            crate::serial_println!(
                "[xhci] hid configure failed slot {} code {}",
                slot,
                ctrl.cmd_code
            );
            return Err(SysError::Unsupported);
        }
    }
    {
        let mut ctrl = CTRL.lock();
        ctrl.hid_slot = slot;
        ctrl.hid_prev = [0; 6];
        ctrl.hid_on = true;
        if post_hid(&mut ctrl, slot).is_err() {
            ctrl.hid_on = false;
            return Err(SysError::Unsupported);
        }
    }
    crate::serial_println!("[xhci] hid armed slot {}", slot);
    Ok(())
}

fn boot_keyboard_cfg(cfg: &[u8]) -> bool {
    if cfg.len() < 9 || cfg[1] != 2 {
        return false;
    }
    let mut i = cfg[0] as usize;
    while i + 8 < cfg.len() {
        let len = cfg[i] as usize;
        if len < 2 || i + len > cfg.len() {
            return false;
        }
        if cfg[i + 1] == 4 && cfg[i + 5] == 3 && cfg[i + 6] == 1 && cfg[i + 7] == 1 {
            return true;
        }
        i += len;
    }
    false
}

fn configure_hid(ctrl: &mut Ctrl, slot: u8) -> Result<(), SysError> {
    let idx = slot as usize - 1;
    let shift = ctrl.ctx as usize;
    ctrl.hid[idx] = Ring::empty();
    seal(&mut ctrl.hid[idx]);
    // QEMU accepts a configure only when add-flag bit 0 (the slot) is
    // set and bit 1 (EP0) is clear. Bit 3 is EP1 IN: the controller
    // reads that endpoint at input offset 128 (context size 32).
    // Context Entries is that same endpoint index.
    write_ctx(&mut ctrl.in_ctx[idx].bytes, 0, 0);
    write_ctx(&mut ctrl.in_ctx[idx].bytes, 1, 1 | (1 << 3));
    let slot_at = shift;
    if slot_at + 4 <= ctrl.in_ctx[idx].bytes.len() {
        let word = u32::from_le_bytes(
            ctrl.in_ctx[idx].bytes[slot_at..slot_at + 4]
                .try_into()
                .unwrap_or([0; 4]),
        );
        let word = (word & !(0x1f << 27)) | (3 << 27);
        write_ctx(&mut ctrl.in_ctx[idx].bytes[slot_at..], 0, word);
    }
    let ep_off = shift * 4;
    let dq = phys_of(&ctrl.hid[idx].trb)? | u64::from(ctrl.hid[idx].cycle);
    // High-speed interrupt interval 6 (2^(6-1) * 125 us). The boot
    // report is 8 bytes, so the max ESIT payload is 8.
    write_ctx(&mut ctrl.in_ctx[idx].bytes[ep_off..], 0, 6 << 16);
    write_ctx(
        &mut ctrl.in_ctx[idx].bytes[ep_off..],
        1,
        (3 << 1) | (7 << 3) | (8 << 16),
    );
    write_ctx(&mut ctrl.in_ctx[idx].bytes[ep_off..], 2, dq as u32);
    write_ctx(&mut ctrl.in_ctx[idx].bytes[ep_off..], 3, (dq >> 32) as u32);
    write_ctx(&mut ctrl.in_ctx[idx].bytes[ep_off..], 4, 8 | (8 << 16));
    Ok(())
}

fn post_hid(ctrl: &mut Ctrl, slot: u8) -> Result<(), SysError> {
    let idx = slot as usize - 1;
    let data = phys_of(&ctrl.report)?;
    for b in &mut ctrl.report {
        unsafe { core::ptr::write_volatile(b, 0) };
    }
    ctrl.xfer_done = false;
    ctrl.xfer_code = 0;
    push(&mut ctrl.hid[idx], data, 8, (TRB_NORMAL << 10) | (1 << 5));
    ring_db(ctrl, slot, 3);
    Ok(())
}

fn control_status(slot: u8, setup_data: u64) -> bool {
    {
        let mut ctrl = CTRL.lock();
        if slot == 0 || slot as usize > SLOTS {
            return false;
        }
        let idx = slot as usize - 1;
        ctrl.xfer_done = false;
        ctrl.xfer_code = 0;
        push(
            &mut ctrl.ep[idx],
            setup_data,
            8,
            (TRB_SETUP << 10) | (1 << 6),
        );
        push(
            &mut ctrl.ep[idx],
            0,
            0,
            (TRB_STATUS << 10) | (1 << 5) | (1 << 16),
        );
        ring_db(&ctrl, slot, 1);
    }
    if !wait_flag(true) {
        return false;
    }
    let ctrl = CTRL.lock();
    ctrl.xfer_code == 1 || ctrl.xfer_code == 13
}

/// Drains one HID report into the keyboard queue and posts the next read.
fn poll_keyboard() {
    let ready = {
        let mut ctrl = CTRL.lock();
        drain(&mut ctrl);
        if !ctrl.hid_on || !ctrl.xfer_done {
            return;
        }
        let code = ctrl.xfer_code;
        let mut raw = [0u8; 8];
        if code == 1 || code == 13 {
            for (i, byte) in raw.iter_mut().enumerate() {
                *byte = unsafe { core::ptr::read_volatile(ctrl.report.as_ptr().add(i)) };
            }
        }
        let prev = ctrl.hid_prev;
        if code == 1 || code == 13 {
            ctrl.hid_prev.copy_from_slice(&raw[2..8]);
        }
        let slot = ctrl.hid_slot;
        ctrl.xfer_done = false;
        let _ = post_hid(&mut ctrl, slot);
        if code == 1 || code == 13 {
            Some((raw, prev))
        } else {
            None
        }
    };
    let Some((raw, prev)) = ready else {
        return;
    };
    let shift = raw[0] & 0x22 != 0;
    let ctrl = raw[0] & 0x11 != 0;
    for usage in &raw[2..8] {
        if *usage != 0 && !prev.contains(usage) {
            if let Some(ch) = galexy_usb::key_char(*usage, shift, ctrl) {
                crate::drivers::keyboard::push_char(ch);
            }
        }
    }
}

fn phys_of<T: ?Sized>(r: &T) -> Result<u64, SysError> {
    let v = VirtAddr::new(core::ptr::from_ref(r) as *const u8 as u64);
    mm::translate(v)
        .map(|p| p.as_u64())
        .ok_or(SysError::Unsupported)
}

fn read8(base: u64, off: u64) -> u8 {
    unsafe { ((base + off) as *const u8).read_volatile() }
}

fn read32(base: u64, off: u64) -> u32 {
    unsafe { ((base + off) as *const u32).read_volatile() }
}

fn write32(base: u64, off: u64, value: u32) {
    unsafe { ((base + off) as *mut u32).write_volatile(value) }
}

fn write64(base: u64, off: u64, value: u64) {
    write32(base, off, value as u32);
    write32(base, off + 4, (value >> 32) as u32);
}
