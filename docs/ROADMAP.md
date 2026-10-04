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

## Phase 3 — User space (RECORDED PLAN, not yet started)

### Step A — privilege rings + syscalls (first)

ABI decisions already FROZEN (Milestone 12, `crates/galexy-abi/`):
capabilities day one (`Cap` = 48-bit index + 16-bit rights; console=1,
self=2; rights bits permanent), numbered syscall table (exit=0, yield=1,
write=2, cap_info=3), error codes 1..5. `sched/syscalls.rs` consumes it —
Step A wires mechanism only.

1. GDT user code/data segments (DPL 3; SYSRET's `user SS = user CS + 8`
   must land on our user data segment) ✅ land-paved (segments already in
   the GDT, consecutive, proven by `bin/test-rings.rs`)
2. `TSS.RSP0` goes live: ring 3→0 transitions push IRQ frames on the
   current task's kernel stack; the switch-in updates RSP0 per task
   (setter already proven — only the per-task hook remains) ✅ land-paved
3. SYSCALL/SYSRET: `EFER.SCE` (MSR write in arch init), `STAR`
   (kernel CS / user CS), `LSTAR` → naked entry, `FMASK`; the naked entry
   builds a uniform IRETQ frame (`rcx`→RIP, `r11`&→RFLAGS, user RSP/SS) so
   the switch machinery stays single-shaped
4. Dispatch (`rax`): mechanism in `arch/`, table in `sched/syscalls.rs`
   (boundary rule 7) — the dispatch skeleton already exists; Step A makes
   `exit`, `yield`, and `write(console_cap, ...)` real
5. `spawn_user_task`: frame-allocate + map code (present+user) and stack
   (writable+user+NX) pages at a fresh user virtual region; fabricate the
   initial user-mode frame via the same `init_stack` path
6. First program: hand-assembled flat blob (~6 instructions, zero
   toolchain deps), embedded as a `const` — its print call goes through
   `write(console_cap, buf, len)` from day one, never a magic framebuffer
   syscall
7. `bin/test-user.rs`: user task prints via syscall, exits via syscall,
   gets timer-preempted while spinning; kernel continues; exit 33

### Step B — real isolation (after Step A verifies)

1. Per-task CR3: `FreshL4` per task (proven in Milestone 12 — self-recursive
   entry, kernel higher-half shared); user region per task mapped via
   `with_table`
2. CR3 in `Context` + swap in the timer switch (Redox pattern: swap only
   when different)
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
