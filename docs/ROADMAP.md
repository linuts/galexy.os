# ROADMAP — galexy.os

Direction-level plan. Concrete check-off items live in `TODO.md`.

## Phase 0 — Boot & TTY (current)

Goal: bootable image, working text display, working keyboard, echo shell.

Done criteria: can type into QEMU, see chars echoed, Enter repeats the line,
Backspace edits, machine never triple-faults.

## Phase 1 — Foundations for running things

1. **Test harness** — `#[test_case]` + QEMU exit-on-pass/fail. Do this before
   memory, because later work depends on regression confidence.
2. **Physical memory** — frame allocator over the `BootInfo` memory map,
   tracked with a bitmap or stack; enable `map_physical_memory` in the
   `BootConfig` first.
3. **Paging** — map the framebuffer explicitly (we currently rely on the
   bootloader's mapping); page-fault handler real enough to debug with.
4. **Heap** — `alloc` + global allocator, unlocked by (2)+(3).

## Phase 2 — Concurrency & scheduling

1. **Cooperative scheduler first** — task queue, yield points, simple round
   robin between two dummy tasks.
2. **Preemptive scheduler** — timer handler swaps in scheduling; TSS stacks
   already exist from M3; per-task kernel stacks + context switch.
3. **Lock discipline audit** — every `spin::Mutex` checked for
   dead-lock-with-interrupts hazards; document the policy in `DESIGN.md`.

## Phase 3 — Beyond

- User space: privilege rings, syscall interface.
- Filesystem: read-only first (RAM disk or simple partition).
- Networking: no-timing-rush; driver work only after scheduling is solid.

## Standing principles

- Each phase must leave the system **bootable and non-regressed**. No
  half-broken intermediate states at the end of any session.
- Prefer the blog_os-proven path over cleverness until a step is *boring*.
- Anything that could corrupt the kernel's own memory is postponed one phase
  beyond the phase that needs it.
