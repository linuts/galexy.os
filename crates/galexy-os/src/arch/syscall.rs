//! SYSCALL/SYSRET mechanism (x86_64): MSR configuration + the naked entry
//! that builds a uniform context frame.
//!
//! Register state at SYSCALL entry (fixed CPU semantics):
//! - `rcx` = user RIP, `r11` = user RFLAGS (explicitly NOT saved by the
//!   instruction), `rsp` = user RSP (unchanged), other GPRs = user values,
//!   DS/ES/FS/GS still the kernel bootstrap selectors on entry.
//! - On return to ring 3, DS/ES are reloaded with the user data selector
//!   (RPL 3). FS stays the kernel bootstrap selector (unused by user
//!   programs). GS keeps the kernel per-CPU base across rings — userland
//!   must not load GS.
//!
//! Entry protocol: switch to the CURRENT task's kernel stack (per-CPU
//! `gs:[8]`, written on every switch-in to a user task), push the uniform
//! frame (same shape as the timer's interrupt frame), then run the Rust
//! dispatch (policy in `sched::syscalls`, per boundary rule 7). The
//! dispatch returns either 0 (resume the outgoing frame) or a context
//! pointer (switch: yield/exit handoff).
//!
//! Mid-flight scratch (user RSP / syscall number) lives in PER-CPU memory
//! (`gs:[16]`/`gs:[24]`, see `arch/cpu.rs`) — with 2 CPUs a shared static
//! would clobber across cores.
//!
//! GS contract: the kernel keeps its GS base (per-CPU struct) across ring
//! transitions — userland never modifies it, and SYSRET leaves it alone.
//! `gs:[8]` therefore names THIS CPU's registry everywhere.

use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::registers::model_specific;
use x86_64::registers::rflags::RFlags;

use crate::serial_println;
/// The user CS selector (RPL 3, iretq's return target) pushed into every
/// uniform frame's CS slot.
static USER_CS: AtomicU64 = AtomicU64::new(0);
/// The user SS selector pushed into the uniform frame (SYSRET computes the
/// CS half in hardware; SS is our constant).
static USER_SS: AtomicU64 = AtomicU64::new(0);

/// Publishes the CURRENT task's kernel stack top for this CPU (switch-in
/// hook). `0` clears (main loop / kernel thread).
pub fn set_task_kstack(top: u64) {
    crate::arch::cpu::set_kstack(top);
}

/// MSR configuration (order matters): `STAR` (kernel/user CS bases), then
/// `LSTAR` (entry address), then `EFER.SCE` last — MSRs must be consistent
/// before the instruction is enabled. `FMASK` left at 0: the uniform frame
/// carries the full user RFLAGS (interrupt state included).
pub fn init() {
    let (kernel_cs, user_cs) = crate::arch::syscall_selectors();
    // STAR[47:32] = kernel CS (SS auto = CS + 8 = kernel data ✓);
    // STAR[63:48] = user CS (SYSRET forces RPL 3 on both CS and SS, and our
    // GDT guarantees user CS + 8 == user data ✓).
    // SAFETY: MSR writes configure the syscall mechanism; the GDT layout is
    // proven by test-rings before any user task exists.
    unsafe { model_specific::Star::write_raw(user_cs, kernel_cs) };
    model_specific::LStar::write(x86_64::VirtAddr::new(
        syscall_entry_naked as *const () as usize as u64,
    ));
    // FMASK: clear IF (and TF) the instant SYSCALL lands. FMASK=0 left the
    // entry's first instructions with IF=1 — a timer tick landing in that
    // one-instruction window interrupts at CPL=0 with RSP = the USER stack
    // (no automatic RSP0 switch below ring 3), and the tick's context gets
    // pushed onto the user stack; the rotation's CR3 swap then unmaps it
    // under the timer's own return path → page fault → double fault →
    // reset. With IF cleared at entry, the window is closed; the user's
    // full RFLAGS still arrives in R11 and rides into the frame (SYSRET
    // consumes it unmodified).
    model_specific::SFMask::write(RFlags::INTERRUPT_FLAG | RFlags::TRAP_FLAG);
    // SAFETY: enabling SCE; STAR/LSTAR are consistent above.
    unsafe {
        model_specific::Efer::update(|f| *f |= model_specific::EferFlags::SYSTEM_CALL_EXTENSIONS);
    }
    let (user_cs_raw, user_ss_raw) = crate::arch::user_cs_ss();
    USER_SS.store(user_ss_raw, Ordering::Relaxed);
    USER_CS.store(user_cs_raw, Ordering::Relaxed);
    serial_println!(
        "[syscall] SYSCALL/SYSRET live (star cs {:#x}/{:#x}, lstar {:#x})",
        kernel_cs,
        user_cs,
        syscall_entry_naked as *const () as usize as u64
    );
}

/// The naked SYSCALL entry: see the module docs for the register state.
///
/// # Safety
///
/// naked function; must only ever be installed as `IA32_LSTAR`.
#[unsafe(naked)]
pub unsafe extern "C" fn syscall_entry_naked() {
    core::arch::naked_asm!(
        // Kernel-origin syscalls are checked in Rust (fail loudly there);
        // first order: interrupts off + switch to the task kernel stack.
        "cli",
        // Stash user rsp + rax into PER-CPU scratch (gs:[16]/[24]), then
        // load the task's kernel stack top (gs:[8]). GS is the kernel's
        // per-CPU base across rings (userland never modifies it) — this
        // column is exactly what makes the entry CPU-agnostic.
        "mov QWORD PTR gs:[16], rsp",
        "mov QWORD PTR gs:[24], rax",
        "mov rsp, QWORD PTR gs:[8]",
        // Uniform frame, pushed downward; rising layout matches
        // context::Context exactly: SS, RSP, RFLAGS, CS, RIP, r15..rax.
        "push QWORD PTR [rip + {user_ss}]", // SS
        "push QWORD PTR gs:[16]", // RSP = user rsp
        "push r11", // RFLAGS = user rflags
        "push QWORD PTR [rip + {user_cs}]", // CS = user (RPL 3 — iretq returns to ring 3)
        "push rcx", // RIP = user rip
        "push QWORD PTR gs:[24]", // rax (the syscall number)
        "push rbx",
        "push rcx", // r11/rcx are clobbered by SYSCALL per the ABI anyway
        "push rdx",
        "push rsi",
        "push rdi",
        "push rbp",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        // Rust dispatch: rdi = frame, rsi = syscall number (rax untouched).
        "mov rdi, rsp",
        "mov rsi, rax",
        "call {rust}",
        // rax = next ctx (0 = resume outgoing frame).
        "cmp rax, 0",
        "je 2f",
        "mov rsp, rax",
        // Publish the departed slot's context as stable now that RSP has
        // left its stack (gs:[40], 1-based). Same sequence as the timer tail.
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
        // Ring-3 data segments: SYSCALL left DS/ES as kernel selectors.
        // Reload before iretq so user code sees RPL-3 DS/ES. Preserve rax
        // (syscall result). FS unused; GS stays the per-CPU base.
        "push rax",
        "mov ax, {user_ds}",
        "mov ds, ax",
        "mov es, ax",
        "pop rax",
        "iretq",
        user_ss = sym USER_SS,
        user_cs = sym USER_CS,
        user_ds = const crate::arch::gdt::USER_DS_RPL3,
        rust = sym syscall_rust,
        stable = sym crate::sched::CTX_STABLE,
    );
}

/// The Rust half: routing + handoff decisions (policy in
/// `sched::syscalls::service`).
///
/// # Safety
///
/// Called only from the naked entry above; `frame` is the just-built frame
/// on the current task's kernel stack.
unsafe extern "C" fn syscall_rust(frame: *mut crate::sched::context::Context, sysno: u64) -> u64 {
    let frame = unsafe { &mut *frame };

    // A syscall is only legal from a ring-3 task context. Kernel-origin
    // SYSCALL = kernel bug (or an ABI cheat): fail loudly.
    let slot = crate::sched::current_slot();
    if slot == 0 {
        serial_println!(
            "[syscall] BUG: syscall from kernel context (rax={:#x})",
            sysno
        );
        crate::exit_qemu(crate::QemuExitCode::Failed);
    }
    if !crate::sched::slot_is_user(slot) {
        serial_println!(
            "[syscall] BUG: syscall from a kernel thread (rax={:#x})",
            sysno
        );
        crate::exit_qemu(crate::QemuExitCode::Failed);
    }

    match crate::sched::syscalls::service(frame, sysno) {
        crate::sched::syscalls::Outcome::Resume => 0,
        crate::sched::syscalls::Outcome::Handoff => {
            // SAFETY: the frame is the current task's uniform context on
            // its kernel stack (built by the naked entry just now).
            unsafe { crate::sched::syscall_handoff(frame, false, "yield") }
        }
        crate::sched::syscalls::Outcome::Exit => {
            // SAFETY: as above; exit tombstones the task — the handoff
            // guarantees a nonzero target (never returns to the dead task).
            unsafe { crate::sched::syscall_handoff(frame, true, "syscall") }
        }
    }
}
