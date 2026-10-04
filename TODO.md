# TODO

Tracking document for concrete work items. Big-picture direction lives in
`docs/ROADMAP.md`. Check items off as they land and are verified.

## Milestone 1 — Boot skeleton ✅

- [x] Workspace + pinned nightly toolchain
- [x] Freestanding kernel target (`x86_64-unknown-none`, bootloader v0.11 flow
      — no custom target JSON needed)
- [x] `#![no_std]`/`#![no_main]` kernel with panic handler (serial-reported)
- [x] Boot to "Hello from galexy.os!" — verified in QEMU (BIOS + UEFI)

## Milestone 2 — Display module ✅ (as `drivers/screen`, framebuffer-based)

- [x] Pixel framebuffer writer + Noto Sans Mono glyphs
- [x] Newline handling + scroll when hitting bottom (verified: 40+ lines)
- [x] `spin::Mutex`-guarded global screen, `_print` hook
- [x] `print!/println!` macros via `core::fmt::Write`
- [x] Serial output alongside (via `uart_16550`)

## Milestone 3 — Interrupts ✅ (BIOS path)

- [x] GDT + TSS, IST stack for double fault, all segment regs reloaded
- [x] IDT: breakpoint, page fault (parks + serial report), double fault
- [x] PIC remapping (`pic8259`), spurious IRQ handling via pic8259
- [x] Timer handler (PIT @ ~1 kHz) with tick counter + 1s serial heartbeat
- [x] Keyboard IRQ handler (raw scancode → `DecodedKey` → `kcore::Ring`)

## Milestone 4 — Echo shell ✅

- [x] Line buffer + Backspace
- [x] Enter echoes the typed line with `echo:` prefix, new prompt after
- [x] Verified interactively in QEMU (monitor `sendkey` + screendump)

## Milestone 5 — Test harness ✅

- [x] `galexy-core` crate lift: `Ring<T, N>` with host unit tests
      (`cargo test -p galexy-core`)
- [x] Kernel lib+bin split: shared init, panic handler, `exit_qemu` via
      `isa-debug-exit` port 0xF4 (Success=0x10 → QEMU exit 33)
- [x] Test kernel binaries (`src/bin/test-basic.rs`, `test-should-panic.rs`)
- [x] Runner-side boot tests: build.rs builds an image per kernel binary;
      `cargo test -p runner` boots each headless, asserts exit codes + serial
      markers, including a liveness check of the interactive kernel
- [ ] More test kernels as subsystems land (interrupt latency, memory map)

## Milestone 6 — Physical memory (frame allocator) ✅

- [x] `galexy-core::Bitmap` (fixed bitset, `fill`/`set`/`test`/first-clear)
      with host unit tests (10/10 across Bitmap + Ring)
- [x] Shared `BootloaderConfig`: kernel stack 256 KiB, physical memory mapped
      at fixed `0x0000_4000_0000_0000`, recursive page table at canonical
      P4-index-511 address (note: must be sign-extended + 512-GiB aligned!)
- [x] `arch/mm.rs`: first-fit frame allocator over `BootInfo` memory map,
      `.bss`-resident bitmap pair (used/usable), double-free + non-usable
      dealloc panics
- [x] `bin/test-memory.rs`: alloc → write/read roundtrip via phys offset →
      dealloc → first-fit reuse → expected double-free panic; verified
      (31239 free frames, exit 33)
- [ ] Extend bitmap coverage beyond 512 MiB when RAM grows (documented
      serial warning already)

## Milestone 7 — Paging ✅

- [x] `arch/mm/paging.rs`: `OffsetPageTable` over the bootloader's active
      page tables (L4 via CR3 + physical-memory offset; recursive entry
      available at P4 index 511)
- [x] `map_page` / `unmap_page` (TLB flushes) + `translate`; page-table
      frames come from our frame allocator via a trait adapter
- [x] Page-fault handler reports CR2 (faulting address) precisely
- [x] Runtime handler replacement (`arch::set_page_fault_handler`) — used by
      tests, will be used by demand paging
- [x] `bin/test-paging.rs`: map → write/read via virtual page AND physical
      offset (agreement asserted) → translate check → unmap → unmapped
      access faults (handler swapped to success-exit); verified (exit 33)

## Milestone 8 — Heap ✅

- [x] `arch/mm/heap.rs`: `linked_list_allocator` `LockedHeap` as
      `#[global_allocator]`, 400 KiB at fresh P4 entry 43
      (`0x5555_5555_0000`), pages mapped via our mapper + frame allocator
- [x] `extern crate alloc` in the kernel lib; echo line buffer → `String`
- [x] `bin/test-heap.rs`: Box/Vec/String roundtrips + drop-and-reuse;
      verified (exit 33)
- [x] Boot banner (`banner.rs`): feature showcase reading REAL subsystem
      state (framebuffer layout, frames free, paging translate of the heap
      start, heap size); echo shell prompt follows

## Milestone 9 — Cooperative scheduler ✅

- [x] `sched/mod.rs`: round-robin `VecDeque` run queue; `spawn(name, step)`,
      `run_once()` (pop → step → re-queue if Yield), `run()` sweep,
      `active_tasks()`/`spawned_total()` stats
- [x] Task model: `fn(&mut TaskCtx) -> TaskStatus` state machines with
      per-task scratch slots (heap-backed queue — first alloc consumer
      beyond echo)
- [x] Demo tickers (`sched/demo.rs`) interleaving on screen; banner gets
      `[ok] scheduler: N tasks`
- [x] `bin/test-sched.rs`: deterministic trace `ABABAA` (interleaving
      proven), completion, re-spawn after drain; verified (exit 33)

## Milestone 10 — Preemptive kernel threads ✅

- [x] `sched/context.rs`: naked-asm timer handler — full CPU context
      (r15..rax + rip/cs/rflags/rsp/ss) saved on each task's own stack,
      RSP swapped mid-handler, `iretq` straight into the next task;
      FXSAVE/FXRSTOR around switches (auto-vectorization safety);
      initial frames fabricated on fresh stacks (trampoline entry)
- [x] `sched`: round-robin over main + threads (`spawn_thread`), unified
      rotation, per-thread 32 KiB `Box` stacks
- [x] Lock audit applied (only preemptor = timer IRQ → locks must be
      IRQ-gated): `screen::_print`, keyboard queue, heap `GlobalAlloc`
      adapter, thread table; policy documented in `docs/DESIGN.md`
- [x] `ltr` fix: TSS now loaded after `lgdt` (was stale — IST dispatch
      read a stale descriptor!)
- [x] Demo threads interleave on screen purely by preemption; banner:
      `[ok] scheduler: N tasks, N threads`
- [x] `bin/test-preempt.rs`: two never-yielding counter threads, both
      progress + round-robin fairness asserted; verified (exit 33)

## Milestone 11 — Quiet OS demo ✅

- [x] Per-thread CPU tick accounting (charged in the timer switch; main
      loop = slot 0); `thread_stats()`/`main_ticks()`
- [x] Status bar: fixed bottom line, redrawn in place every second
      (uptime ticks, per-thread ticks, frames free) with cursor
      save/restore — IRQ-gated as a whole (lock-audit rule)
- [x] `echo.rs` -> `shell.rs`: `help`, `stats`, `tasks`, `threads`,
      `clear`, `about`; unknown lines still echo; silent demo threads
      (noise removed from boot)
- [x] Live demo verified across 3 screendumps: bar numbers change over
      time, command output legible, heartbeats continue (wedge fixed:
      `thread_stats` was taking the sched table lock ungated!)

## Known limitations / follow-ups

- [ ] UEFI: timer + keyboard dead under UEFI — legacy PIC doesn't exist;
      needs APIC under `arch/` (boot-only behavior now guarded by the UEFI
      smoke test)
- [ ] Thread guard pages: a too-deep thread silently corrupts the heap
- [ ] Thread reaper: returning threads park; stacks leak
- [ ] Status bar can overwrite the typing line when the screen is full
      (cursor is restored, but the in-progress line's glyphs are clipped)
- [ ] Heap is a fixed 400 KiB area — grow-on-demand when needed
- [ ] Framebuffer is used as the bootloader mapped it (deliberate — BootInfo
      exposes no physical framebuffer address; revisit with isolation work)
- [ ] Page mapping concurrency: mapper ops are main-loop-only right now;
      IRQ handlers never touch MAPPER (verify again with ring 3)
- [ ] Screen: text-mode cursor (blinking), tab handling, ANSI-ish output
- [ ] Keyboard queue overflow silently drops keys — fine for now, revisit
- [ ] Cooperative-scheduler nits: `run()` sweep fairness mid-sweep;
      TaskCtx's 8 fixed u64 slots (boxed state enum when tasks get richer)
