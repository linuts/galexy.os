//! Per-CPU substrate: identity via GS base.
//!
//! Kernel-side per-CPU data lives at `gs:0` — the GS base register holds the
//! address of THIS CPU's [`PerCpu`] struct (written once at bring-up via
//! WRGSBASE; requires the FSGSBASE CPU feature, asserted loudly at boot).
//! Userland never touches GS (and cannot change the GS BASE), so the swapgs
//! discipline Linux needs does not apply here: kernel-side gs accesses are
//! usable at ANY CPL-0 moment (syscall entry, naked timer, IRQ) with no
//! segment juggling. This is the documented ABI rule "user code must not do
//! segment-based addressing" made load-bearing for the kernel too.
//!
//! Fixed-offset contract (naked asm reads/writes these; atomics are
//! `repr(transparent)` over their inner u64, so offsets are the raw ones):
//! - `gs:[0]`  = self pointer (`PerCpu` address; sanity/introspection)
//! - `gs:[8]`  = `kstack_top` — the SYSCALL entry's kernel-stack target
//!   (the value the naked entry loads into RSP before building the frame)
//! - `gs:[16]` = `saved_rsp` — SYSCALL-entry mid-flight scratch (user RSP)
//! - `gs:[24]` = `saved_rax` — SYSCALL-entry mid-flight scratch (syscall no.)
//!
//! Every per-CPU field is WRITTEN by exactly one CPU (its owner) — no locks
//! required by ownership; atomics appear anyway so the static array itself
//! is `Sync` and because later commits will extend the struct with fields
//! that ARE read cross-CPU.
//!
//! On a single-CPU boot, only slot 0 is touched ("pinned at spawn" scheduler
//! follows in a later commit, so nothing distinguishes CPU 0 from an AP yet).

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::serial_println;

/// CPUs the kernel can host. Bring-up is MADT-driven; tests run 2. The cap
/// is generous deliberately — per-CPU slots are tiny statics.
pub const MAX_CPUS: usize = 8;

/// One CPU's identity + scratch block.
///
/// `repr(C, align(64))` is load-bearing for the first four fields (the
/// naked-asm offsets above); static asserts pin them below.
#[repr(C, align(64))]
pub struct PerCpu {
    /// This struct's own address (gs:[0]; set at bring-up).
    self_ptr: AtomicU64,
    /// SYSCALL entry kernel-stack target (gs:[8]). `0` = main loop /
    /// kernel thread — a syscall there is a kernel bug, checked in Rust.
    kstack_top: AtomicU64,
    /// SYSCALL-entry scratch: user RSP stashed before the stack switch.
    saved_rsp: AtomicU64,
    /// SYSCALL-entry scratch: the syscall number (stashed before the Rust
    /// dispatch) so the uniform frame can carry the entry-time value.
    saved_rax: AtomicU64,
    /// Logical CPU index (`0..=MAX_CPUS-1`; set at bring-up).
    cpu_index: AtomicU32,
    /// The CPU's APIC ID (from the MADT; set at bring-up).
    apic_id: AtomicU32,
}

// Layout contract for the naked asm (verified at compile time):
const _: () = assert!(core::mem::offset_of!(PerCpu, self_ptr) == 0);
const _: () = assert!(core::mem::offset_of!(PerCpu, kstack_top) == 8);
const _: () = assert!(core::mem::offset_of!(PerCpu, saved_rsp) == 16);
const _: () = assert!(core::mem::offset_of!(PerCpu, saved_rax) == 24);

impl PerCpu {
    /// This struct's own address (gs:[0]; set at bring-up).
    pub fn self_ptr(&self) -> &AtomicU64 {
        &self.self_ptr
    }

    /// SYSCALL entry kernel-stack target (gs:[8]).
    pub fn kstack_top(&self) -> &AtomicU64 {
        &self.kstack_top
    }

    /// Logical index of the owning CPU.
    pub fn index(&self) -> u32 {
        self.cpu_index.load(Ordering::Relaxed)
    }

    /// The owning CPU's APIC ID (from the MADT; set at bring-up).
    pub fn apic_id(&self) -> u32 {
        self.apic_id.load(Ordering::Relaxed)
    }

    /// All-zero block (`.bss`-shaped); bring-up fills the identity fields.
    const fn zeroed() -> Self {
        Self {
            self_ptr: AtomicU64::new(0),
            kstack_top: AtomicU64::new(0),
            saved_rsp: AtomicU64::new(0),
            saved_rax: AtomicU64::new(0),
            cpu_index: AtomicU32::new(0),
            apic_id: AtomicU32::new(0),
        }
    }
}

/// The per-CPU slots; `SLOTS[i]` belongs to logical CPU `i` only.
static SLOTS: [PerCpu; MAX_CPUS] = [const { PerCpu::zeroed() }; MAX_CPUS];

/// Number of CPUs the kernel has brought online (BSP counts as 1).
static ONLINE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
/// GS-base access is live for the CALLING CPU only after ITS bring-up
/// wrote the base — but the boot sequence is strictly ordered (gdt::init →
/// init_bsp on the BSP; each AP brings itself up), so this global boolean
/// guards the whole mechanism from early Rust-side callers (a bug net).
static GS_READY: AtomicBool = AtomicBool::new(false);

/// Per-CPU raw base fetch (`RDGSBASE`).
#[inline]
fn gs_base() -> u64 {
    let base: u64;
    // SAFETY: RDGSBASE is gated by the FSGSBASE feature assert at bring-up;
    // `nomem` is exact: the instruction reads no memory.
    unsafe { asm!("rdgsbase {}", out(reg) base, options(nostack, nomem)) };
    base
}

/// The calling CPU's per-CPU block. Panics before bring-up (a bug).
#[inline]
pub fn current() -> &'static PerCpu {
    assert!(
        GS_READY.load(Ordering::Acquire),
        "cpu: per-CPU GS access before bring-up"
    );
    let base = gs_base();
    assert!(base != 0, "cpu: GS base is 0 despite GS_READY");
    // SAFETY: the base was WRGSBASE'd at bring-up to point at this CPU's
    // slot in `SLOTS`; the address is within the static array forever.
    unsafe { &*(base as *const PerCpu) }
}

/// Logical index of the calling CPU.
#[inline]
pub fn current_index() -> usize {
    current().cpu_index.load(Ordering::Relaxed) as usize
}

/// Writes the SYSCALL entry's kernel-stack target for THIS CPU (switch-in
/// hook; the naked entry reads gs:[8] on the same CPU, so ownership makes
/// this race-free without a lock).
pub fn set_kstack(top: u64) {
    current().kstack_top.store(top, Ordering::Relaxed);
}

/// Number of CPUs online (BSP counts once).
pub fn online() -> usize {
    ONLINE.load(Ordering::Relaxed)
}

/// Detects FSGSBASE via CPUID leaf 7, subleaf 0, EBX bit 0.
fn fsgsbase_supported() -> bool {
    let b: u64;
    // NOTE: for architectural leaf 7, cpuid's EAX return is the *subleaf
    // max* (often 0) — NOT a max-level indicator. Leaf 7 exists on every
    // long-mode-model CPU; only the EBX feature bits carry information.
    let mut _max_subleaf = 0u32;
    // SAFETY: cpuid probes features and clobbers eax/ecx/edx AND ebx.
    // ebx is callee-saved and CANNOT be an inline-asm constraint on
    // x86_64, so its value is parked in r8 via xchg (explicit register —
    // no allocator aliasing hazards) and the captured feature bits stay
    // in r8 while rbx is put back.
    unsafe {
        asm!(
            "xchg rbx, r8",
            "cpuid",
            "xchg rbx, r8",
            out("r8") b,
            inout("rax") 7u32 => _max_subleaf,
            inout("rcx") 0u32 => _,
            out("rdx") _,
        );
    }
    b & (1 << 0) != 0
}

/// Brings up per-CPU access for the BSP (logical CPU 0): feature assert,
/// fill the slot's identity fields, WRGSBASE, verify roundtrip.
///
/// MUST run AFTER `gdt::init` (the GS selector load resets the base to the
/// descriptor's — WRGSBASE must be the last GS-base writer).
pub fn init_bsp() {
    // CR4.FSGSBASE must be SET before any RDGSBASE/WRGSBASE is legal:
    // the CPUID bit only says the CPU CAN do it (firmware leaves the CR4
    // bit clear). Every crossing of gs:[*] (syscall entry, later all
    // switch paths) needs this live.
    let mut cr4 = x86_64::registers::control::Cr4::read();
    cr4.insert(x86_64::registers::control::Cr4Flags::FSGSBASE);
    // SAFETY: setting CR4.FSGSBASE on a CPU that reports the CPUID feature;
    // no live per-CPU accessors exist yet (this runs before any).
    unsafe { x86_64::registers::control::Cr4::write(cr4) };
    serial_println!("[cpu] CR4.FSGSBASE enabled");

    assert!(
        fsgsbase_supported(),
        "cpu: FSGSBASE unsupported — per-CPU mechanism requires it (CPUID 7.0.EBX bit 0)"
    );

    let slot = &SLOTS[0];
    let slot_addr = slot as *const PerCpu as u64;
    slot.self_ptr.store(slot_addr, Ordering::Relaxed);
    slot.cpu_index.store(0, Ordering::Relaxed);
    // SAFETY: WRGSBASE is gated by the feature assert above; `slot` is this
    // CPU's own slice entry (never aliased by another CPU).
    unsafe { asm!("wrgsbase {}", in(reg) slot_addr) };

    // Roundtrip: the GS base must now read back as the slot address, and
    // the memory the base points at (gs:[0]) must hold that same address
    // (self-referential sanity). NOTE: this path cannot call `current()`
    // (its GS_READY net isn't set yet) — read through the slot directly.
    let readback = gs_base();
    assert_eq!(readback, slot_addr, "cpu: WRGSBASE roundtrip failed");
    assert_eq!(
        slot.self_ptr.load(Ordering::Relaxed),
        readback,
        "cpu: gs:[0] self-pointer mismatch"
    );

    GS_READY.store(true, Ordering::Release);
    ONLINE.store(1, Ordering::Relaxed);
    serial_println!("[cpu] per-cpu GS live (slot 0 @ {:#x})", readback);
}
