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

impl Context {
    /// The privilege level this frame returns into (`0` or `3` — bits 0..1
    /// of the saved CS selector; ring 1/2 are unused by this OS).
    ///
    /// Frame shape is IDENTICAL for both rings: ring 0→0 and ring 3→0
    /// interrupts push the same 5-word IRQ frame (iret semantics). What
    /// differs per ring for ring 3→0 crossings is WHERE the frame lands
    /// (TSS.RSP0, not the user stack) and which selectors are valid —
    /// decided by the CPU, not this layout.
    pub fn cpl(&self) -> u8 {
        (self.cs & 0b11) as u8
    }
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
        // Publish the departed slot's context as stable NOW: RSP has left
        // that stack. gs:[40] is 1-based; CTX_STABLE is one byte per slot.
        // A plain store is a release store on x86.
        "mov rcx, qword ptr gs:[40]",
        "test rcx, rcx",
        "jz 2f",
        "dec rcx",
        "lea rdx, [rip + {stable}]",
        "mov byte ptr [rdx + rcx], 1",
        "mov qword ptr gs:[40], 0",
        "2:",
        "pop r15", "pop r14", "pop r13", "pop r12",
        "pop r11", "pop r10", "pop r9", "pop r8",
        "pop rbp", "pop rdi", "pop rsi", "pop rdx",
        "pop rcx", "pop rbx", "pop rax",
        "iretq",
        sched = sym timer_sched,
        stable = sym super::CTX_STABLE,
    );
}

/// The PAGE-FAULT vector's naked handler: same prologue as the timer, but
/// the page-fault vector carries an ERROR CODE pushed by the CPU right
/// above the IRQ frame — so the built frame is one word SKEWED vs
/// `Context` (nobody ever pops it: every page-fault outcome is death or
/// park). The Rust half reads fields by RAW OFFSETS.
///
/// - fault origin ring 3: kill the faulting task (tombstone + switch).
/// - fault origin ring 0: report + park (the old default).
///
/// # Safety
///
/// naked function; must only be installed as the page-fault vector's
/// handler.
#[unsafe(naked)]
pub unsafe extern "C" fn page_fault_handler_naked() {
    core::arch::naked_asm!(
        "push rax", "push rbx", "push rcx", "push rdx",
        "push rsi", "push rdi", "push rbp",
        "push r8", "push r9", "push r10", "push r11",
        "push r12", "push r13", "push r14", "push r15",
        "mov rdi, rsp",
        "call {sched}",
        "cmp rax, 0",
        "je 2f",
        "mov rsp, rax",
        // Same stable-context publish as the timer tail (see there).
        "mov rcx, qword ptr gs:[40]",
        "test rcx, rcx",
        "jz 2f",
        "dec rcx",
        "lea rdx, [rip + {stable}]",
        "mov byte ptr [rdx + rcx], 1",
        "mov qword ptr gs:[40], 0",
        "2:",
        "pop r15", "pop r14", "pop r13", "pop r12",
        "pop r11", "pop r10", "pop r9", "pop r8",
        "pop rbp", "pop rdi", "pop rsi", "pop rdx",
        "pop rcx", "pop rbx", "pop rax",
        "iretq",
        sched = sym page_fault_sched,
        stable = sym super::CTX_STABLE,
    );
}

/// The Rust half of the page-fault path (raw offsets; see
/// `page_fault_handler_naked` docs for the frame layout: 15 GPRs, then
/// error code, then the 5-word IRQ frame).
///
/// # Safety
///
/// Called only from the naked wrapper; `frame` is the outgoing task's
/// stack top (its context block), exclusively owned, IF=0.
unsafe extern "C" fn page_fault_sched(frame: *mut Context) -> u64 {
    let raw = frame as *const u64;
    // SAFETY: the frame is on the faulting task's mapped stack.
    let cs = unsafe { raw.add(17).read() };
    let err = unsafe { raw.add(15).read() };
    if cs & 0b11 == 3 {
        // Ring-3 fault: the task dies, the kernel lives. Tombstone +
        // rotate (guaranteed switch — the dead task is never main).
        let cr2 = x86_64::registers::control::Cr2::read();
        crate::serial_println!(
            "[pf] ring-3 task fault: err={:#x}, cr2={:#?} — killing the task",
            err,
            cr2
        );
        unsafe { super::syscall_handoff(frame, true, "page fault") }
    } else {
        // Ring-0 fault: kernel bug or a test-installed seam. Report
        // precisely, then park (the faulting instruction would refault).
        crate::serial_println!(
            "[pf] PAGE FAULT in ring 0, err={:#x}, cr2={:#?}",
            err,
            x86_64::registers::control::Cr2::read()
        );
        loop {
            x86_64::instructions::hlt();
        }
    }
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

/// Fabricates a fresh KERNEL thread's initial context on `stack_top` and
/// returns the context pointer the switch machinery restores into. The
/// trampoline is the landing RIP; the entry fn rides in `rdi`.
///
/// # Safety
///
/// `stack_top` must top a fresh, exclusively owned, 16-byte-aligned stack of
/// at least `INITIAL_CONTEXT_SIZE` bytes.
pub unsafe fn init_stack(stack_top: u64, entry: extern "C" fn(), cs: u64, ss: u64) -> u64 {
    // SAFETY: contract above.
    unsafe {
        init_frame_stack(
            stack_top,
            stack_top - 512, // RSP: scratch space below the frame
            trampoline as *const () as u64,
            entry as *const () as u64,
            0,
            cs,
            ss,
        )
    }
}

/// Fabricates a fresh USER task's initial context (RIP directly at the
/// program's entry address — no trampoline: ring-3 code has no kernel stack
/// to return through, and `thread_exit` does not exist for it until the
/// exit syscall lands).
///
/// `write_top` is WHERE the frame is fabricated (for loader spawns: the
/// phys-map image of the user stack's top page — the stack's own virtual
/// addresses are task-private); `user_rsp` is the RSP VALUE recorded in the
/// frame (must be a USER-space address: `user_rsp - 512` is left as scratch).
///
/// # Safety
///
/// Same contract as [`init_stack`]; additionally `cs`/`ss` must be the
/// RPL-3 user selectors, `rip`/`user_rsp` must be mapped user addresses.
pub unsafe fn init_user_frame(
    write_top: u64,
    user_rsp: u64,
    rip: u64,
    rdi: u64,
    rsi: u64,
    cs: u64,
    ss: u64,
) -> u64 {
    // SAFETY: contract above.
    unsafe { init_frame_stack(write_top, user_rsp, rip, rdi, rsi, cs, ss) }
}

/// Bytes [`init_frame_stack`] writes at the top of a fresh stack (20 pushes).
pub const FABRICATED_FRAME_BYTES: u64 = 20 * 8;

/// Shared frame fabricator (both rings; identical frame shape).
///
/// # Safety
///
/// `write_top` must top a fresh, exclusively owned, 16-byte-aligned stack of
/// at least `INITIAL_CONTEXT_SIZE` bytes; `rip`/`cs`/`ss` must form a valid
/// entry condition for the frame's privilege level.
unsafe fn init_frame_stack(
    write_top: u64,
    user_rsp: u64,
    rip: u64,
    rdi: u64,
    rsi: u64,
    cs: u64,
    ss: u64,
) -> u64 {
    let mut sp = write_top as *mut u64;
    let mut push = |value: u64| {
        // SAFETY: caller guarantees enough stack space above `write_top`.
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
    push(user_rsp); // RSP: caller-provided (user-space for ring 3)
    push(0x202); // RFLAGS: reserved bit 1 + interrupts enabled
    push(cs); // CS
    push(rip); // RIP

    // GPRs (rax ... r15 — push order reversed); rdi carries the entry fn
    // (trampoline arg) or 0 for user frames, everything else zero.
    push(0); // rax
    push(0); // rbx
    push(0); // rcx
    push(0); // rdx
    push(rsi); // rsi = second arg (user argument length, or 0)
    push(rdi); // rdi = first arg for the trampoline, or the argument pointer
    push(0); // rbp
    push(0); // r8
    push(0); // r9
    push(0); // r10
    push(0); // r11
    push(0); // r12
    push(0); // r13
    push(0); // r14
    push(0); // r15

    debug_assert_eq!(
        write_top - sp as u64,
        FABRICATED_FRAME_BYTES,
        "fabricated frame size drifted from FABRICATED_FRAME_BYTES"
    );
    sp as u64
}

/// Stack size a fresh task needs for its initial context (incl. scratch).
pub const INITIAL_CONTEXT_SIZE: u64 = 1024;

/// First-run landing pad: runs the task's entry, then marks the thread
/// exited (the scheduler's reaper frees the stack from the main loop; see
/// `sched::reap`). The naked timer handler is what "returns" here — this
/// function never returns to it.
extern "C" fn trampoline(entry: extern "C" fn()) {
    entry();
    super::thread_exit();
    // Zombie loop: MUST keep interrupts ENABLED. A disabled `hlt` here would
    // wedge the machine — the dead thread is running until the next timer
    // tick, and only that tick (which skips it via the rotation) lets the
    // main loop run again. With IF=1, the next tick preempts the zombie,
    // skips it forever, and the reaper later frees its stack.
    loop {
        x86_64::instructions::hlt();
    }
}

/// Helpers for reading kernel segment selectors (set by `arch::gdt`).
pub fn kernel_cs_ss() -> (u64, u64) {
    use x86_64::instructions::segmentation::{Segment, CS, SS};
    (CS::get_reg().0 as u64, SS::get_reg().0 as u64)
}

/// `(user_cs, user_ss)` selectors with RPL 3 (set by `arch::gdt`).
pub fn user_cs_ss() -> (u64, u64) {
    crate::arch::user_cs_ss()
}
