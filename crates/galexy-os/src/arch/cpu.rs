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

use core::arch::{asm, global_asm};
use x86_64::PhysAddr;
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
/// Shared per-CPU bring-up (BSP init + every AP): CR4 gate, feature assert,
/// slot fill, WRGSBASE, roundtrip verify. MUST run AFTER `gdt::bring_up` on
/// that CPU (the GS selector load resets the base — WRGSBASE must be the
/// last GS-base writer).
pub fn init_percpu(cpu_index: usize, apic_id: u32) {
    // CR4.FSGSBASE must be SET before any RDGSBASE/WRGSBASE is legal: the
    // CPUID bit only says the CPU CAN do it (firmware leaves the CR4 bit
    // clear). Every crossing of gs:[*] (syscall entry, later all switch
    // paths) needs this live.
    let mut cr4 = x86_64::registers::control::Cr4::read();
    cr4.insert(x86_64::registers::control::Cr4Flags::FSGSBASE);
    // SAFETY: setting CR4.FSGSBASE on a CPU that reports the CPUID feature;
    // no live per-CPU consumers exist on this CPU yet (its own bring-up).
    unsafe { x86_64::registers::control::Cr4::write(cr4) };

    assert!(
        fsgsbase_supported(),
        "cpu: FSGSBASE unsupported — per-CPU mechanism requires it (CPUID 7.0.EBX bit 0)"
    );

    let slot = &SLOTS[cpu_index];
    let slot_addr = slot as *const PerCpu as u64;
    slot.self_ptr.store(slot_addr, Ordering::Relaxed);
    slot.cpu_index.store(cpu_index as u32, Ordering::Relaxed);
    slot.apic_id.store(apic_id, Ordering::Relaxed);
    // SAFETY: WRGSBASE is gated by the feature assert above; `slot` is this
    // CPU's own slice entry (never aliased by another CPU).
    unsafe { asm!("wrgsbase {}", in(reg) slot_addr) };

    // Roundtrip: the GS base must read back as the slot address, and the
    // memory at the base (gs:[0]) must hold that same address. NOTE: this
    // path cannot call `current()` yet (the GS_READY net isn't set until
    // the first CPU completes) — read through the slot directly.
    let readback = gs_base();
    assert_eq!(readback, slot_addr, "cpu: WRGSBASE roundtrip failed");
    assert_eq!(
        slot.self_ptr.load(Ordering::Relaxed),
        readback,
        "cpu: gs:[0] self-pointer mismatch"
    );

    GS_READY.store(true, Ordering::Release);
    let _ = ONLINE.fetch_add(1, Ordering::Relaxed);
    serial_println!("[cpu] per-cpu GS live (slot {} @ {:#x})", cpu_index, readback);
}

/// BSP bring-up entry (slot 0; called by `arch::init`).
pub fn init_bsp() {
    let bsp_id = crate::arch::acpi::madt().boot_cpu_apic_id();
    init_percpu(0, bsp_id as u32);
}

/* ---------------- AP bring-up (INIT/SIPI + trampoline) ---------------- */

/// Physical address of the 16→32→64-bit trampoline page (SIPI target).
/// MUST be 4 KiB aligned and under 1 MiB (vector = phys >> 12).
const TRAMPOLINE_PHYS: u64 = 0x8000;
/// UIFFF Bootstrap AP page tables: L4 with (a) a 1-GiB identity low map
/// (the trampoline's own continuation + handoff slots) and (b) VERBATIM
/// kernel L4 entries beyond that (kernel image + phys map + MMIO pages +
/// recursion all shared). Built once before the first AP; every AP loads
/// the SAME cr3.
static AP_CR3: AtomicU64 = AtomicU64::new(0);
/// Per-AP boot stacks (BSP keeps the bootloader's). Index 0 = logical CPU 1.
const AP_STACK_SIZE: usize = 64 * 1024;
static AP_STACKS: [PerCpuStack; MAX_CPUS - 1] = [const { PerCpuStack::new() }; MAX_CPUS - 1];

/// Stack type with alignment for the FXSAVE-friendly 16-byte ending.
#[repr(C, align(16))]
struct PerCpuStack([u8; AP_STACK_SIZE]);

impl PerCpuStack {
    const fn new() -> Self {
        Self([0; AP_STACK_SIZE])
    }
}

// SAFETY: written only by its owning CPU (hardware stack memory, the CPU
// is the sole writer; no Rust reader aliases a live stack).
unsafe impl Sync for PerCpuStack {}

/// The online magic an AP writes into its handoff slot once its per-CPU
/// init has completed (the BSP's startup loop waits for exactly this).
const AP_ONLINE_MAGIC: u64 = 0xC0FF_EE01;

// Fixed trampoline page layout (RUST writes the data slots through the
// phys map BEFORE copying the code blob in; offsets are absolute page
// offsets so both the asm and Rust agree without any linker games):
//   page+0x000        the 5-byte far jump (to blob offset 5)
//   page+0x005..      .code16 real-mode section (≤ 0x100 bytes)
//   page+0x100        .code32 bridge (aligned)
//   page+0x200        .code64 continuation (aligned)
//   page+0xE00        micro-GDTR: u16 limit + 3-byte base (m16&24 form)
//   page+0xE20        micro-GDT: null, code32 (base 0x8000, limit 0xFFF),
//                     code64 (flat, L=1)
//   page+0xF00        handoff: cr3 | stack top | fn | rank | magic slot
//
// Position-independence: the blob is assembled at the kernel's link VMA
// but RUNS at 0x8000 — so label VALUES are wrong by the delta between the
// two. The design therefore uses NO label completes as absolute targets:
// every control-flow cross-mode transition pushes + `retf`s values
// computed at RUNTIME from a `pop eip` (the CS bases chosen here make
// EIP == page offset where it matters), and intra-mode jumps are RELATIVE
// label encodings.
//
// The ONE deliberate deviation: the VERY FIRST far jump is a raw 0xEA
// byte encode (SIPI lands at 0:0x8000 = blob offset 0; the far jmp
// normalizes CS to 0x0800 so `cs:` offsets reach the data slots).
global_asm!(
    ".section .text.trampoline,\"ax\",@progbits",
    "TRAMP_START:",
    ".code16",
    // SIPI starts at CS:IP = 0:0x8000 → linear 0x8000 = blob+0. The far
    // jump's precise 5-byte encoding targets blob+5 (tramp16) with
    // CS changed to 0x0800 (base 0x8000): from now on, cs:off = page+off.
    ".byte 0xEA, 0x05, 0x00, 0x00, 0x08",
    "tramp16:",
    "cli",
    // Real-mode lgdt reads m16&24 (u16 limit + 24-bit base) — the data
    // slots the kernel wrote at +0xE00 (gdtr) and +0xE20 (GDT).
    "lgdt cs:[0xE00]",
    // CR4.PAE (long-mode prerequisite; INIT leaves CR4 = 0).
    "mov eax, cr4",
    "or eax, 0x20",
    "mov cr4, eax",
    // CR3: the bootstrap AP tables (phys < 4 GiB; the handoff carries the
    // u64 at +0xF00 — the low dword is all we need for cr3's load).
    "mov eax, DWORD PTR cs:[0xF00]",
    "mov cr3, eax",
    // EFER.LME (bit 8) + EFER.NXE (bit 11): the AP's tables carry the
    // SHARED kernel subtrees, whose data/BSS PT_LOAD pages are NX-marked —
    // with NXE clear, every NX PTE is a reserved-bit page fault (e=0xA!)
    // instead of an intended no-execute mapping.
    "mov ecx, 0xC0000080",
    "rdmsr",
    "or eax, 0x900",
    "wrmsr",
    // CR0.PG | CR0.PE in ONE write (PG requires PE): with PAE + LME this
    // is the transition to long-mode-active (16-bit compatibility).
    "mov eax, cr0",
    "or eax, 0x80000001",
    "mov cr0, eax",
    // 16 → 32: far return into the 32-bit code selector (0x08 = the
    // micro-GDT entry with base 0x8000, limit 0xFFF; D=1). 16-bit far
    // return pops IP16 then CS16: push the target's page offset LAST.
    // (0xCB is the 16-bit-operand encoding by default — the assembler's
    // mnemonic resolution emitted a 0x66-prefixed 32-bit return here, so
    // the byte is pinned explicitly.)
    "push 0x08",
    "push 0x100",
    ".byte 0xCB",
    ".balign 256",
    ".code32",
    "TRAMP32:", // blob/page offset 0x100 (balign guarantees it)
    // 32-bit compat with CS = the base-0x8000 selector: EIP == page
    // offset, linear = 0x8000 + EIP. Compute the .code64 target linear
    // (fits the identity-mapped low page) with label differences:
    "call nextE",
    "nextE:",
    "pop eax", // eax = page offset of (nextE + 5) — the point AFTER the call
    "sub eax, 5",
    // LAYOUT CONTRACT STEPS (asserted in Rust next to the copy): tramp32
    // = blob+0x100, cont64 = blob+0x200.
    "sub eax, 0x100",          // eax = (nextE - tramp32)
    "add eax, 0x200",          // eax = cont64's page offset
    "add eax, 0x8000",         // → cont64's LINEAR address
    // 32 → 64: far return into the 64-bit CS (0x10, L=1 flat). retf
    // (32-bit operand) pops EIP (zero-extended into RIP) then CS.
    "push 0x10",
    "push eax",
    "retf",
    ".balign 256",
    ".code64",
    "CONT64:", // blob/page offset 0x200
    // Long mode; the identity 1-GiB map covers the page. Handoff slots
    // (absolute low-memory addresses via the identity map):
    //   0x8F00 cr3 (unused here) · 0x8F08 stack top · 0x8F10 fn
    //   0x8F18 rank · 0x8F20 magic slot (ap_main writes it).
    "mov rsp, QWORD PTR [0x8F08]",
    "mov rdi, QWORD PTR [0x8F18]",
    "mov rax, QWORD PTR [0x8F10]",
    "call rax", // ap_main(rank) — never returns
    "hlt",
    "absurd:",
    "hlt",
    "jmp absurd",
    "TRAMP_END:",
);

// SAFETY: symbols attached to the global-asm blob above (never relocated —
// only their addresses are taken, for the copy loop + layout contracts).
unsafe extern "C" {
    static TRAMP32: u8;
    static CONT64: u8;
}

// SAFETY: symbols attached to the global-asm blob above (never reloc'd —
// only their addresses are taken, for the copy loop's byte range).
unsafe extern "C" {
    static TRAMP_START: u8;
    static TRAMP_END: u8;
}

/// Copies the trampoline blob from its link-image location into the SIPI
/// target page, writes the micro-GDT + handoff for `rank`, and returns the
/// page's physical base. Called once per AP (sequentially).
unsafe fn stage_trampoline(rank: usize, stack_top: u64) {
    const TRAMP_PHYS: u64 = 0x8000;
    let dst = super::mm::frame_virt(PhysAddr::new(TRAMP_PHYS)).as_mut_ptr::<u8>();
    // The blob occupies [TRAMP_START, TRAMP_END). Layout contracts — the
    // asm's raw pushes/jumps read fixed 0x100 steps (verified, not assumed):
    let base = core::ptr::addr_of!(TRAMP_START) as *const u8 as u64;
    assert_eq!(
        core::ptr::addr_of!(TRAMP32) as u64 - base,
        0x100,
        "trampoline: the 32-bit bridge must sit at blob offset 0x100"
    );
    assert_eq!(
        core::ptr::addr_of!(CONT64) as u64 - base,
        0x200,
        "trampoline: the 64-bit continuation must sit at blob offset 0x200"
    );
    let src = core::ptr::addr_of!(TRAMP_START) as *const u8;
    let len = core::ptr::addr_of!(TRAMP_END) as u64 - src as u64;
    // The blob must end below the data slots (the 16-bit section reaching
    // tramp32 at +0x100 was verified by the balign + asserts above).
    assert!(
        len <= 0xE00,
        "trampoline: the code blob overgrew its page ({len} bytes)"
    );
    // SAFETY: the SIPI target page is host RAM below 1 MiB (Usable), owned
    // at bring-up; the phys map gives kernel-side write access to it.
    unsafe {
        core::ptr::copy_nonoverlapping(src, dst, len as usize);
    }

    // Fixed-slot writer (contract coordinates with the asm via page offsets).
    let put = |slot: u64, value: u64| {
        let addr = dst.add(slot as usize) as *mut u64;
        // SAFETY: slot addresses are the fixed contract with the asm.
        unsafe { addr.write_volatile(value) };
    };
    // micro-GDT at 0xE20 (3 entries) + gdtr (m16&24) at 0xE00.
    let gdt_phys = TRAMP_PHYS + 0xE20;
    let put_entry = |i: usize, v: u64| {
        let addr = dst.add(0xE20 + i * 8) as *mut u64;
        // SAFETY: fixed slot; part of the trampoline page contract.
        unsafe { addr.write_volatile(v) };
    };
    put_entry(0, 0); // null
    // code32: base = page base, limit = 0xFFF, D=1, type 9A, G=0.
    put_entry(1, encode_flat_code(TRAMP_PHYS, 0x0FFF, false));
    // code64: L=1 flat code (base/limit ignored in long mode).
    put_entry(2, 0x00AF_9A00_0000_FFFF);
    // gdtr (m16&24 form): u16 limit + 24-bit base = spread over 0xE00..E05.
    let put_u16 = |slot: u64, v: u16| {
        let addr = dst.add(slot as usize) as *mut u16;
        // SAFETY: fixed slot.
        unsafe { addr.write_volatile(v) };
    };
    put_u16(0xE00, 0x17); // limit = 3 entries * 8 - 1
    put_u16(0xE02, (gdt_phys & 0xFFFF) as u16);
    put_u16(0xE04, ((gdt_phys >> 16) & 0xFF) as u16);

    // Handoff (+0xF00): cr3 · stack top · fn (ap_main) · rank · magic=0.
    put(0xF00, AP_CR3.load(Ordering::Acquire));
    put(0xF08, stack_top);
    put(0xF10, ap_main as *const () as u64);
    put(0xF18, rank as u64);
    put(0xF20, 0); // clear the online magic
}

/// Descriptor encoder for 32-bit code (base + limit + D perimeter).
const fn encode_flat_code(base: u64, limit: u64, l: bool) -> u64 {
    // descriptor bit layout: [15:0] limit 0..15 · [39:16] base 0..23 ·
    // [47:40] type/flags · [51:48] limit 16..19 · [55:52] G,D/B,L,AVL ·
    // [63:56] base 24..31
    let mut raw = 0u64;
    raw |= limit & 0xFFFF;                       // limit 0..15
    raw |= (base & 0xFF) << 16;                  // base 0..7
    raw |= ((base >> 8) & 0xFF) << 24;           // base 8..15
    raw |= ((base >> 16) & 0xFF) << 32;          // base 16..23
    raw |= 0x9A << 40;                           // PRESENT ring-0 code, read
    raw |= ((limit >> 16) & 0xF) << 48;          // limit 16..19
    let flags: u64 = if l { 0b1010 } else { 0b0100 }; // G, D|L, AVL
    raw |= flags << 52;
    raw |= ((base >> 24) & 0xFF) << 56;          // base 24..31
    raw
}

/// Boots every AP below the BSP, one at a time: reseat truc, handoff
/// staged, INIT → 10 ms → SIPI → (200 µs) → SIPI, then wait for the
/// online magic. BSP-only (sends IPIs across the bus; touches global);
/// runs with interrupts off per `arch::init` ordering.
pub fn boot_aps() {
    let madt = crate::arch::acpi::madt();
    let ids = madt.enabled_ids();
    if ids.len() <= 1 {
        serial_println!("[cpu] single-CPU MADT; no APs to boot");
        return;
    }
    build_ap_tables();
    for rank in 1..ids.len() {
        let apic_id = ids[rank];
        if rank >= MAX_CPUS {
            serial_println!("[cpu] warning: AP beyond MAX_CPUS skipped (apic id {})", apic_id);
            continue;
        }
        let stack = &AP_STACKS[rank - 1];
        let stack_top = stack.0.as_ptr() as u64 + AP_STACK_SIZE as u64;

        // SAFETY: whole-maintenance staging; BSP-only flow with IRQs off.
        unsafe { stage_trampoline(rank, stack_top) };

        // INIT → wait → SIPI ×2 → wait for online magic.
        crate::arch::apic::send_init(apic_id);
        crate::arch::timer::delay_ms(10);
        crate::arch::apic::send_sipi(apic_id, TRAMPOLINE_PHYS);
        crate::arch::timer::delay_ms(1);
        crate::arch::apic::send_sipi(apic_id, TRAMPOLINE_PHYS);

        let magic_ptr = super::mm::frame_virt(PhysAddr::new(TRAMPOLINE_PHYS + 0xF20)).as_mut_ptr::<u64>();
        // SAFETY: fixed handoff slot (contract with the asm).
        let deadline = 50_000_000usize;
        let mut i = 0;
        while unsafe { magic_ptr.read_volatile() } != AP_ONLINE_MAGIC {
            core::hint::spin_loop();
            i += 1;
            if i > deadline {
                serial_println!("[cpu] warning: ap rank {} (apic id {}) never came online", rank, apic_id);
                break;
            }
        }
        if i <= deadline {
            serial_println!(
                "[cpu] ap rank {} (apic id {}) online",
                rank,
                apic_id
            );
        }
    }
}

/// The AP's own per-CPU table readiness (L4: identity 1 GiB low + verbatim
/// kernel entries beyond it). Built alongside the FIRST AP's boot; shared.
fn build_ap_tables() {
    if AP_CR3.load(Ordering::Acquire) != 0 {
        return; // already built
    }
    // Frames: one L4 + one L3 (with the 1-GiB identity huge page).
    let l4 = crate::arch::mm::allocate_frame().expect("ap tables: L4 frame");
    let l3 = crate::arch::mm::allocate_frame().expect("ap tables: L3 frame");
    let write_table = |root: PhysAddr, index: usize, value: u64| {
        // SAFETY: allocator-owned (Usable) frames; exclusive at build.
        unsafe {
            let base = crate::arch::mm::frame_virt(root).as_mut_ptr::<u64>();
            base.add(index).write_volatile(value);
        }
    };
    // L3, entry 0: 1-GiB identity (huge page, PRESENT|WRITE|ACCESSED).
    write_table(l3.start_address(), 0, 0x0000_0000_0087);
    // L4, entry 0 points at the L3 (PRESENT|WRITE).
    write_table(l4.start_address(), 0, l3.start_address().as_u64() | 0x003);
    // L4, everything else: verbatim boot-table entries (kernel image, phys
    // map, MMIO slots, recursion) — the kernel half is shared memory.
    let kernel_root = crate::arch::mm::kernel_cr3().start_address();
    for i in 1..512 {
        // SAFETY: read source is the still-active boot L4 (never modified
        // here); write target is an allocator-owned frame.
        unsafe {
            let src = crate::arch::mm::frame_virt(kernel_root).as_ptr::<u64>();
            let dst = crate::arch::mm::frame_virt(l4.start_address()).as_mut_ptr::<u64>();
            dst.add(i).write_volatile(src.add(i).read_volatile());
        }
    }
    AP_CR3.store(l4.start_address().as_u64(), Ordering::Release);
    serial_println!(
        "[cpu] ap tables ready (cr3 {:#x}, identity 1 GiB + shared kernel half)",
        AP_CR3.load(Ordering::Acquire)
    );
}

/// AP-only: the whole per-CPU bring-up once the trampoline hands us off.
/// Called on the AP's OWN boot stack with interrupts still off; runs until
/// the machine parks it.
extern "C" fn ap_main(rank: u64) -> ! {
    let cpu_index = rank as usize;
    assert!(cpu_index < MAX_CPUS, "ap_main: rank beyond MAX_CPUS");

    // Per-CPU tables + GS identity. Segment reloads here also fix the
    // trampoline-leftover segment state (real-mode cached values).
    crate::arch::gdt::bring_up(cpu_index);
    init_percpu(cpu_index, apic_id_for(cpu_index));
    // LAPIC up WITHOUT the timer (its LVT stays masked; the per-CPU
    // scheduler arms its own tick later).
    crate::arch::apic::bring_up(crate::arch::acpi::madt().lapic_base());
    serial_println!(
        "[cpu] ap rank {} online (per-cpu @ {:#x})",
        cpu_index,
        current() as *const PerCpu as u64
    );
    // Handoff magic: the BSP's boot loop waits for this.
    let magic = super::mm::frame_virt(PhysAddr::new(TRAMPOLINE_PHYS + 0xF20)).as_mut_ptr::<u64>();
    // SAFETY: fixed handoff slot.
    unsafe { magic.write_volatile(AP_ONLINE_MAGIC) };

    // Park: IRQs are still off (no sharing concerns); commit 3 turns this
    // into the per-CPU scheduler's idle loop.
    loop {
        core::hint::spin_loop();
        x86_64::instructions::hlt();
    }
}

/// APIC id for a logical index (read from the MADT at boot).
fn apic_id_for(cpu_index: usize) -> u32 {
    crate::arch::acpi::madt().enabled_ids()[cpu_index] as u32
}
