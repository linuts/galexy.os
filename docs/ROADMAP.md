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

1. Per-task CR3 ✅ — `FreshL4` per task at spawn (the clone source is
   the kernel root cached at init, Milestone 28); user
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

- Filesystem capabilities ✅ (Milestone 20: `open(name) → Cap(file)` +
  `read(cap)` + `close(cap)` on the ramdisk. Per-task table, READ grant
  checked as kernel-rights ∩ handle snapshot, `bin/test-open` round-trips
  `banner.txt` including a missing name, a forged cap, EOF, and close).
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
- Cross-CPU coordination ✅ (Milestone 19: precise-INVLPG shootdown IPIs
  on vector 0xF8 — lock-free handler, 8×16 VA mailbox, monotonic seq —
  heap growth broadcasts each new chunk through `shootdown_others` with a
  GROWING-conflict wait, idle-pass work stealing (stable context published
  after the victim's `mov rsp`, entry on the next tick, ~100-tick
  cooldown); `bin/test-ipi`, `bin/test-smpstress`,
  and `test-heapgrow`'s broadcast marker. Suite: 30 QEMU boots, all
  `-smp 2`).
- Shell `run <program>` command ✅ (Milestone 16: ramdisk service +
  `shell::exec("run hello")` — dispatch → loader, full lifecycle; typed-
  keystroke E2E over QMP proves the real input path)
- Userland print hygiene: console = screen + serial mirror ✅ (Milestone
  16).
- Console sequences ✅ (Milestone 24: tab, CR, and a small CSI subset
  on the framebuffer — SGR colors, cursor position, erase. The status
  bar owns the last row, so a full screen scrolls above it. Serial
  still receives the raw bytes. `bin/test-screen` checks glyphs and
  the serial mirror).
- Query capabilities ✅ (Milestone 22: `stats` / `tasks` / `threads`
  are reserved caps. `read` returns a fresh text snapshot, rendered
  without allocating on the IF=0 syscall path. The ring-3 shell types
  all three; `bin` coverage is the QMP typing test).
- Kernel-root page tables ✅ (Milestone 28: `FreshL4` copies the
  kernel root cached at init, not the table in CR3. A child cannot
  inherit another task's user mappings. The ELF load stays on the main
  loop, because the loader allocates and a syscall runs with interrupts
  off. `bin/test-cloneroot` installs a user mapping and checks the
  next fresh table does not have it).
- Scratch files ✅ (Milestone 27: `create`=9. Eight fixed slots, a
  64-byte name and a 256-byte buffer each, no heap on the syscall
  path. The cap is READ|WRITE; `write` appends. A tar name is
  `Unsupported`, a full table is `NoResource`. `bin/test-scratch`
  writes, reads back, and still sees the archive `banner.txt`).
- Core utilities ✅ (Milestone 29: the ring-3 shell has `echo`, `cat`,
  `touch`, `mkdir`, and `cd`. A path ending in `/` is a directory in
  the scratch table. `create` with `RDX == 1` empties an existing
  scratch file, so `echo >` can replace it. Archive names stay at `/`,
  and `run` is still a program name. The typing test cats `banner.txt`,
  writes a scratch file, and walks `mkdir` / `cd` / `ls`).
- Remove scratch names ✅ (Milestone 30: `remove`=10 frees a scratch
  file or an empty directory so the slot can be created again. A
  directory with a child stays, and a tar name cannot be removed. An
  open cap on a removed file is `BadCap`. `bin/test-rm` fills the
  table again after the frees. The shell's `rm` does the same).
- Launch by name ✅ (Milestone 31: typing `hello` starts that ELF.
  The launched task is granted the console only. The shell, loaded at
  boot, also holds the keyboard, the loader, the query caps, and power.
  A fabricated reserved index without that grant is `AccessDenied`.
  A non-ELF ramdisk name is `Unsupported`. The typing test types
  `hello`).
- Shell supervisor ✅ (Milestone 32: `spawn` returns once the ELF is
  loaded, and the child keeps running. `r8`/`r9` carry one argument.
  `r10` bit 0 adds the query grant; keyboard, the loader, and power
  stay with the shell. `echo`, `cat`, `touch`, `mkdir`, `rm`, and `ls`
  are ramdisk programs. If the shell faults, the main loop loads it
  again and other tasks keep running. The BIOS typing test starts
  `linger`, types `crash`, and still sees `beat`).
- One console per F-key ✅ (Milestone 33: F1–F12 each have a cell grid
  and a shell. The keyboard interrupt records the switch; the main
  loop paints it. Keys and COM1 follow the visible console. A program
  keeps writing the console it was started on. `bin/test-screen`
  restores a hidden grid, and a typing test runs `echo hi` on F2).
- galfs tokens ✅ (Milestone 34: each actor has one root. `/Desktop` is
  yours; `/dan@Desktop` is dan's. A token, not the path, grants rights.
  The shell and programs inherit the parent's tokens. Boot creates
  actor `alex`. `bin/test-galfs` proves AccessDenied without a token
  and a listing once LIST is installed).
- grant syscall ✅ (Milestone 35: `grant`=11 installs a token on a live
  user task. The caller must already hold the rights. The shell's
  `grant lr <path> <task>` uses it. `bin/test-galfs` has dan grant
  `/Desktop` to a reader that then opens `dan@Desktop/secret`).
- Revoke, pipes, seek, mv/cp ✅ (Milestone 36: `revoke`=12 drops token
  rights; boot creates actor `dan` with Desktop and F2's shell is dan;
  `pipe`=13 + `give`=14 move pipe ends between tasks; `seek`=15 sets the
  read cursor; `cp`/`mv` are ramdisk utils. Suite: 43 QEMU boots).
- User management ✅ (Milestone 37: `user`=16 with whoami/users/add/del/su.
  Alex can add and delete empty actors; `su` switches the shell's root
  while keeping tokens. `bin/test-users`. Suite: 44 QEMU boots).
- Thread-slot reuse ✅ (Milestone 26: a freed slot is overwritten in
  place once no CPU is current on it and the switch-out tail has left
  that stack. The name is a fixed buffer, so spawn does not leak.
  `bin/test-reuse` exits 80 threads and still lists the one left
  running).
- Ramdisk listing ✅ (Milestone 25: files cap `0x8006`. `read` returns
  the archive's regular names, one per line. The shell's `ls` types
  them; the same QMP test checks `banner.txt` and `hello`).
- Shutdown and reboot ✅ (Milestone 23: power cap `0x8005`, syscall
  `power`=8. ACPI S5 from the FADT/`_S5_` package, reset via the FADT
  register or the keyboard controller. `bin/test-shutdown` and
  `bin/test-reboot` prove QEMU actually exits. Under `cargo run`,
  `-no-reboot` means a reboot request quits QEMU).
- Userland shell ✅ (Milestone 21: the interactive shell is a ring-3
  program. Keyboard cap + `read` for keystrokes, loader cap + `spawn`
  which parks the caller until the child exits, the ELF load drained on
  the kernel page table. BSP-resident, not stealable. Typing `hello`
  still drives it).

## Standing principles

- Each phase must leave the system **bootable and non-regressed**. No
  half-broken intermediate states at the end of any session.
- Prefer the blog_os-proven path over cleverness until a step is *boring*.
- Anything that could corrupt the kernel's own memory is postponed one phase
  beyond the phase that needs it.
