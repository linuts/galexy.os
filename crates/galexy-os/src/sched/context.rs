//! Context switching for timer-preemptive kernel threads (x86_64).
//!
//! Design (adapted from the EuraliOS/nikofil pattern): the timer vector is a
//! NAKED function — the CPU has already pushed the IRQ frame; the naked code
//! pushes all general-purpose registers, calls the Rust scheduler with the
//! frame pointer, receives the next task's saved-context pointer (or 0 to
//! stay), swaps RSP, pops the registers, and `iretq`s straight into the next
//! task. Each task's full state therefore lives on its own stack.
//!
//! FPU/SSE: kernel code may auto-vectorize (e.g. memcpy), so XMM state is
//! preserved with FXSAVE/FXRSTOR around the switch, into per-task save areas.

use x86_64::VirtAddr;

/// Full CPU context of a preempted task, as laid out on its stack by the
/// naked timer wrapper (rising addresses: pushed GPRs first, then the IRQ
/// frame pushed by the CPU).
#[repr(C)]
pub struct Context {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

/// FXSAVE area size in bytes (512, 16-byte aligned).
pub const FX_AREA_SIZE: usize = 512;

/// Saves the CPU's XMM/x87 state into `area` (must be 16-byte aligned).
///
/// # Safety
///
/// `area` must be a valid, 16-byte-aligned, 512-byte buffer.
pub unsafe fn fx_save(area: *mut u8) {
    debug_assert!((area as usize).is_multiple_of(16));
    core::arch::asm!("fxsave [{}]", in(reg) area);
}

/// Restores the CPU's XMM/x87 state from `area`.
///
/// # Safety
///
/// `area` must be a valid, 16-byte-aligned, 512-byte buffer holding a prior
/// `fx_save` result.
pub unsafe fn fx_restore(area: *const u8) {
    debug_assert!((area as usize).is_multiple_of(16));
    core::arch::asm!("fxrstor [{}]", in(reg) area);
}

/// The timer's naked IDT entry: see module docs.
///
/// # Safety
///
/// naked function; must only be installed as the timer vector's handler.
/// Runs on the interrupted task's stack with IF=0 (interrupt gate).
#[unsafe(naked)]
pub unsafe extern "C" fn timer_handler_naked() {
    core::arch::naked_asm!(
        // GPRs, pushed so that rising addresses match Context's field order:
        // r15 ... rax (rax highest).
        "push rax", "push rbx", "push rcx", "push rdx",
        "push rsi", "push rdi", "push rbp",
        "push r8", "push r9", "push r10", "push r11",
        "push r12", "push r13", "push r14", "push r15",
        // Rust scheduler: rdi = context frame pointer; returns the next
        // task's context pointer in rax (0 = don't switch).
        "mov rdi, rsp",
        "call {sched}",
        "cmp rax, 0",
        "je 2f",
        "mov rsp, rax",
        "2:",
        "pop r15", "pop r14", "pop r13", "pop r12",
        "pop r11", "pop r10", "pop r9", "pop r8",
        "pop rbp", "pop rdi", "pop rsi", "pop rdx",
        "pop rcx", "pop rbx", "pop rax",
        "iretq",
        sched = sym timer_sched,
    );
}

/// The Rust half of the timer switch: called on the OUTGOING task's stack
/// with the frame pointer; returns the incoming task's context pointer (or
/// 0 to resume the outgoing task untouched).
extern "C" fn timer_sched(frame: *mut Context) -> u64 {
    use super::on_timer_tick;

    // SAFETY: the frame pointer comes from the naked wrapper's `mov rdi, rsp`
    // — it is the top of the outgoing task's (mapped, exclusively owned) stack.
    unsafe { on_timer_tick(frame) }
}

/// Fabricates a fresh task's initial context on `stack_top` and returns the
/// context pointer the switch machinery restores into.
///
/// # Safety
///
/// `stack_top` must top a fresh, exclusively owned, 16-byte-aligned stack of
/// at least `INITIAL_CONTEXT_SIZE` bytes.
pub unsafe fn init_stack(stack_top: u64, entry: extern "C" fn(), cs: u64, ss: u64) -> u64 {
    let mut sp = stack_top as *mut u64;
    let mut push = |value: u64| {
        // SAFETY: caller guarantees enough stack space above `stack_top`.
        unsafe {
            sp = sp.sub(1);
            sp.write(value);
        }
    };

    // Layout rises as: r15 ... rax, RIP, CS, RFLAGS, RSP, SS — mirrored by
    // the naked handler's 15 pops + iretq. Pushes go top-down, so the frame
    // is fabricated in REVERSE: SS first, r15 last.

    // IRQ frame (SS, RSP, RFLAGS, CS, RIP — push order reversed).
    push(ss); // SS
    push(stack_top - 512); // RSP: scratch space below the frame
    push(0x202); // RFLAGS: reserved bit 1 + interrupts enabled
    push(cs); // CS
    push(trampoline as *const () as u64); // RIP

    // GPRs (rax ... r15 — push order reversed); rdi carries the entry fn
    // (trampoline arg), everything else zero.
    push(0); // rax
    push(0); // rbx
    push(0); // rcx
    push(0); // rdx
    push(0); // rsi
    push(entry as usize as u64); // rdi = first arg for the trampoline
    push(0); // rbp
    push(0); // r8
    push(0); // r9
    push(0); // r10
    push(0); // r11
    push(0); // r12
    push(0); // r13
    push(0); // r14
    push(0); // r15

    sp as u64
}

/// Stack size a fresh task needs for its initial context (incl. scratch).
pub const INITIAL_CONTEXT_SIZE: u64 = 1024;

/// First-run landing pad: runs the task's entry, then parks if it returns
/// (kernel threads are not expected to return; no reaper yet).
extern "C" fn trampoline(entry: extern "C" fn()) {
    crate::serial_println!("[sched] trampoline entered");
    entry();
    crate::serial_println!("[sched] thread returned; parking");
    loop {
        x86_64::instructions::hlt();
    }
}

/// Helpers for reading kernel segment selectors (set by `arch::gdt`).
pub fn kernel_cs_ss() -> (u64, u64) {
    use x86_64::instructions::segmentation::{Segment, CS, SS};
    (CS::get_reg().0 as u64, SS::get_reg().0 as u64)
}

/// The saved-context address of a task's stack for VirtAddr conversions.
pub fn ctx_virt(addr: u64) -> VirtAddr {
    VirtAddr::new(addr)
}
