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

## Phase 2.5 — Ring-3 readiness ✅

Debt scrubbed BEFORE userland so Step A starts on clean ground (details in
`TODO.md` Milestone 12, commit-checked):

1. **Heap grow-on-demand** ✅ — 64 KiB chunks past the initial 400 KiB via
   `LockedHeap::extend`; OOM alloc → grow + retry.
2. **Paging hardening** ✅ — mapper ops IRQ-gated in the API (safe from any
   context incl. IRQs); `FreshL4` (cheap cloned L4, self-recursive entry)
   + `with_table` mapping through non-active trees — the Step B mechanics
   exist and are proven end to end before CR3 swapping began.
3. **Thread lifecycle** ✅ — reaper (stacks + fx areas return to the heap),
   tombstone slots (stable slot index = future TID), rotation skips dead
   slots, stack canary surfaces deep overflow on reap.
4. **Ring-3 plumbing** ✅ — GDT user segments (SYSRET-consecutive layout),
   TSS.RSP0 setter, `Context::cpl()`; proven by `bin/test-rings.rs`.
5. **`galexy-abi` crate** ✅ — syscall numbers + capability model + error
   codes frozen and host-tested BEFORE any ring-3 code exists.

## Phase 3 — User space

### Step A — privilege rings + syscalls ✅

ABI decisions frozen (Milestone 12, `crates/galexy-abi/`); the mechanism +
first real program landed in Milestone 13:

1. GDT user code/data segments ✅ (consecutive, `user SS = user CS + 8`)
2. `TSS.RSP0` live: per-task kernel stacks; switch-in updates RSP0 ✅
3. SYSCALL/SYSRET: `STAR` (write_raw user_cs / kernel_cs), `LSTAR` → naked
   entry, `FMASK` 0, `EFER.SCE` last ✅ — the naked entry switches to the
   task's kernel stack first (RSP is the user stack at entry), pushes the
   uniform frame (same shape as the timer frame), Rust dispatch returns
   0 = resume / pointer = switch ✅
4. Dispatch: `sched/syscalls.rs` binds `galexy-abi` numbers to behavior —
   exit (tombstone + handoff), yield (real rotation switch), write
   (cap authority + page-walk-validated user buffer + screen), cap_info
   (echo) ✅; unknown → Unsupported ✅
5. `spawn_user_task` ✅ — code/stack/scratch pages mapped at a fresh user
   P4 entry (scanned top-down); the initial ring-3 frame rides on the
   user stack ✅
6. First program ✅ — hand-assembled blob printing "Hello from ring 3!"
   through `write(console_cap, ...)`, yielding, exiting
7. `bin/test-user.rs` ✅ — the full lifecycle, preempted and reaped;
   plus `test-userpreempt.rs` (ring 3 vs timer) and `test-syscall.rs`
   (msr+frame end to end); all verified (exit 33)

### Step B — real isolation (next)

1. Per-task CR3: `FreshL4` per task (proven in Milestone 12 — self-recursive
   entry, kernel higher-half shared); user region per task mapped via
   `with_table`; page-table tree walk so the reaper can free whole spaces
   (currently the spawn's table frames stay allocated)
2. CR3 in `Context` + swap in the timer switch (Redox pattern: swap only
   when different); user DS/ES/FS/GS hygiene at ring-3 entry
3. Per-task user stacks/program pages; preemptively-scheduled isolated
   user task end to end; guard pages become unmapped low pages of each
   stack (the canary check graduates to real fault-on-overflow)

### Debt to pay along the way (see TODO)

- APIC so UEFI boots get timer/keyboard (UEFI smoke test already guards
  boot-only behavior)
- Heap growth ✅ done (grow-on-demand since Milestone 12)
- Thread reaper ✅ done (tombstones + canary since Milestone 12)

## Phase 4 — Beyond

- User space: see the recorded plan above (Step A rings + SYSCALL/SYSRET,
  Step B isolation)
- Filesystem: read-only first (RAM disk or simple partition). Resources get
  capabilities (ABI already shaped for it — no fds, ever).
- Networking: no-timing-rush; driver work only after scheduling is solid.

## Standing principles

- Each phase must leave the system **bootable and non-regressed**. No
  half-broken intermediate states at the end of any session.
- Prefer the blog_os-proven path over cleverness until a step is *boring*.
- Anything that could corrupt the kernel's own memory is postponed one phase
  beyond the phase that needs it.
