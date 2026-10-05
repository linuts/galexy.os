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

### Step B — real isolation ✅

1. Per-task CR3 ✅ — `FreshL4` per task at spawn (kernel-tree guard:
   spawns must run on the kernel table so the clone stays clean); user
   region per task mapped via `with_table` into its own tree at a scanned
   top-free P4 entry; kernel-side staging through backing frames ✅
2. CR3 in `Context`'s task record + swap in BOTH switch paths (timer +
   syscall handoff), no-op when unchanged ✅; the kernel half of every
   tree is shared verbatim, so the switch is safe mid-gate ✅
3. Reaped trees: `free_user_tree` walks the task's own P4 entry subtree
   (tables + data) — frame accounting closes exactly across churn ✅;
   guard pages = the unmapped page below each user stack; ring-3 faults
   kill only the faulting task (naked PF handler, third handoff entry
   point) ✅

### Debt to pay along the way (see TODO)

- APIC ✅ done (Milestone 17 - LAPIC timer + I/O APIC keyboard on every
  boot path, UEFI first-class)
- Heap growth ✅ done (grow-on-demand since Milestone 12)
- Thread reaper ✅ done (tombstones + canary since Milestone 12)

### Step C — real programs ✅ (Milestone 15)

1. `galexy-rt` runtime (entry!, syscall wrappers, panic handler) ✅
2. ELF loader (xmas-elf, static ET_EXEC, per-segment flags, strict
   same-P4-entry policy) ✅
3. Ramdisk: runner packs user-program ELFs into a tar;
   `BootInfo.ramdisk_addr` read kernel-side; `TarCursor` in galexy-core ✅
4. `hello` — a real Rust user program, loaded + lifecycle-complete ✅

## Phase 4 — Beyond

- Filesystem capabilities: `open(name) → Cap(file)` + `read(cap, ...)` —
  the capability-day-one machinery exercised by real resources.
- APIC so UEFI boots get timer/keyboard; the door to SMP ✅ (Milestone 17:
  MADT discovery (arch/acpi), LAPIC enabled with a PIT-calibrated periodic
  timer on vector 32 (xAPIC/x2APIC dual access), I/O APIC routing the
  keyboard onto vector 33, legacy 8259s remapped + fully masked; UEFI
  boots assert full liveness — heartbeat AND a typed `run hello` E2E
  under OVMF).
- SMP: two CPUs, one kernel ✅ (Milestone 18: per-CPU GS-base substrate
  (FSGSBASE) + per-CPU GDT/TSS/syscall-MSRs, position-independent
  16→32→64-bit AP trampoline + INIT/SIPI bring-up, pinned-at-spawn
  scheduler (per-CPU rotation, owner-reaping), share-split per-CPU LAPIC
  timers (machine-wide ~1 kHz preserved), ring-3 on either CPU; the WHOLE
  suite runs at `-smp 2`, plus dedicated bin/test-smp/-smpuser).
- Shell `run <program>` command ✅ (Milestone 16: ramdisk service +
  `shell::exec("run hello")` — dispatch → loader, full lifecycle; typed-
  keystroke E2E over QMP proves the real input path)
- Userland print hygiene: console = screen + serial mirror ✅ (Milestone
  16); ANSI-ish console layer still future work.
- Userland shell (a shell as a REAL ring-3 program): needs a `read`-side
  syscall + keyboard capability — the next capability step after fs caps.

## Standing principles

- Each phase must leave the system **bootable and non-regressed**. No
  half-broken intermediate states at the end of any session.
- Prefer the blog_os-proven path over cleverness until a step is *boring*.
- Anything that could corrupt the kernel's own memory is postponed one phase
  beyond the phase that needs it.
