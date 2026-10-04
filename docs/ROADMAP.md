# ROADMAP — galexy.os

Direction-level plan. Concrete check-off items live in `TODO.md`.

## Phase 0 — Boot & TTY ✅

Goal: bootable image, working text display, working keyboard, echo shell.

Done: boots (BIOS+UEFI), pixel-framebuffer text with colors + scrolling,
keyboard line editing with Backspace, echo shell; regression-tested.

## Phase 1 — Foundations for running things ✅

1. **Test harness** ✅ — QEMU integration tests (one kernel bin per test)
   + host unit tests of `galexy-core`; exit codes via `isa-debug-exit`.
2. **Physical memory** ✅ — bitmap frame allocator over the `BootInfo`
   memory map; physical memory mapped at a fixed offset.
3. **Paging** ✅ — `map_page`/`unmap_page`/`translate`, page-fault handler
   reports CR2 (framebuffer remap deliberately deferred).
4. **Heap** ✅ — `alloc` + `linked_list_allocator`, fixed 400 KiB area.

## Phase 2 — Concurrency & scheduling ✅

1. **Cooperative scheduler first** ✅ — task queue, yield points, simple
   round robin between counting tasks; heap-backed run queue.
2. **Preemptive scheduler** ✅ — naked-asm timer handler swaps full CPU
   contexts (per-thread stacks + FXSAVE); main loop is rotation slot 0.
3. **Lock discipline audit** ✅ — rule established and applied: locks held
   by preemptable code must be IRQ-gated, with the gate living in the
   module's public API (policy in `DESIGN.md`).

## Phase 3 — User space (RECORDED PLAN, not yet started)

### Step A — privilege rings + syscalls (first)

1. GDT user code/data segments (DPL 3; SYSRET's `user SS = user CS + 8`
   must land on our user data segment)
2. `TSS.RSP0` goes live: ring 3→0 transitions push IRQ frames on the
   current task's kernel stack; the switch-in updates RSP0 per task
3. SYSCALL/SYSRET: `EFER.SCE` (MSR write in arch init), `STAR`
   (kernel CS / user CS), `LSTAR` → naked entry, `FMASK`; the naked entry
   builds a uniform IRETQ frame (`rcx`→RIP, `r11`&→RFLAGS, user RSP/SS) so
   the switch machinery stays single-shaped
4. Dispatch (`rax`): print(char), exit, yield (yield = real round-robin
   switch; exit = mark done + switch away); mechanism in `arch/`, table in
   `sched/syscalls.rs` (boundary rule 7)
5. `spawn_user_task`: frame-allocate + map code (present+user) and stack
   (writable+user+NX) pages at a fresh user virtual region; fabricate the
   initial user-mode frame via the same `init_stack` path
6. First program: hand-assembled flat blob (~6 instructions, zero
   toolchain deps), embedded as a `const`
7. `bin/test-user.rs`: user task prints via syscall, exits via syscall,
   gets timer-preempted while spinning; kernel continues; exit 33

### Step B — real isolation (after Step A verifies)

1. Per-task CR3: fresh L4, kernel higher-half entries copied via the
   recursive mapping (P4 index 511); user region per task
2. CR3 in `Context` + swap in the timer switch (Redox pattern: swap only
   when different)
3. Per-task user stacks/program pages; preemptively-scheduled isolated
   user task end to end

### Debt to pay along the way (see TODO)

- APIC so UEFI boots get timer/keyboard (UEFI smoke test already guards
  boot-only behavior)
- Thread guard pages (too-deep thread silently corrupts the heap today)
- Thread reaper (returning threads park; stacks leak)
- Heap growth beyond the fixed 400 KiB area

## Phase 3 — Beyond

- User space: see the recorded plan above (Step A rings + SYSCALL/SYSRET,
  Step B isolation)
- Filesystem: read-only first (RAM disk or simple partition).
- Networking: no-timing-rush; driver work only after scheduling is solid.

## Standing principles

- Each phase must leave the system **bootable and non-regressed**. No
  half-broken intermediate states at the end of any session.
- Prefer the blog_os-proven path over cleverness until a step is *boring*.
- Anything that could corrupt the kernel's own memory is postponed one phase
  beyond the phase that needs it.
