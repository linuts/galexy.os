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

## Milestone 12 — Ring-3 readiness ✅

Foundations scrubbed BEFORE any userland lands; every item QEMU-tested.
The galexy-abi ABI decisions (capabilities day one) are locked below and in
`docs/DESIGN.md`.

- [x] Heap grows on demand: `arch/mm/heap.rs` maps 64 KiB chunks past the
      initial 400 KiB (P4 entry 43 spans 512 GiB) via `LockedHeap::extend`;
      OOM in `alloc` triggers one growth + retry. `bin/test-heapgrow.rs`
      pushes 5 MiB through it (~12.8x initial); verified (exit 33)
- [x] Paging hardening: map/unmap/translate IRQ-gated in the API
      (lock-audit rule — callable from any context now, including IRQs);
      `FreshL4` builds a near-verbatim copy of the active L4 with a
      SELF-POINTING recursive entry (a verbatim copy would address the OLD
      tree once loaded in CR3); `with_table` maps through NON-active trees.
      `bin/test-freshl4.rs`: clone + self-recursive + higher-half shared +
      fresh-tree-only mapping invisible to the active tree; verified
- [x] Thread lifecycle: reaper + tombstones + canary guard
      - slots are NEVER removed (`CURRENT`/`LAST_SERVED` indexes must stay
        stable mid-switch) — exited threads stay as `Freed` tombstone
        structs (stable slot = future TID); rotation scans forward past
        dead slots, terminates on main
      - `thread_exit()` tombstones from the thread itself; main-loop
        `sched::reap()` frees stack + FXSAVE area and checks the stack
        canary (deep overflow → loud panic instead of silent heap rot)
      - WEDGE FIX (found by the new test): the zombie's park loop MUST
        keep IF=1 — `hlt` with interrupts off sleeps forever (the dead
        thread is running until the next tick skips it; with IF=0 nothing
        ever preempts it and the whole machine sleeps)
      - `bin/test-threadexit.rs`: 3 threads return from their entry, all
        reaped, 3x32 KiB stacks + fx areas back on the heap free list;
        verified (exit 33)
- [x] Ring-3 plumbing (structure only, no userland yet): GDT gains DPL-3
      user code/data segments (consecutive: `user SS = user CS + 8`, the
      SYSRET quirk); `arch::set_tss_rsp0`/`tss_rsp0` (TSS now behind an
      UnsafeCell — CPU reads it via descriptor while Rust updates RSP0);
      `Context::cpl()` decodes the frame's privilege (identical frame
      shape for both rings). `bin/test-rings.rs`: selectors live + TSS.RSP0
      roundtrip + cpl decode; verified (exit 33)
- [x] `galexy-abi` crate: THE syscall ABI, frozen before any ring-3 code
      - capabilities day one: opaque u64 `Cap` (48-bit index + 16-bit
        rights), rights mask (READ/WRITE/SIGNAL/WAIT/EXEC — bits never
        renumbered), reserved indexes (console=1, self=2) permanent
      - syscall table `SYSCALLS` (index = number): exit=0, yield=1,
        write=2, cap_info=3 (template), slot 4 reserved; host tests assert
        unique/dense/ordered numbering, `exit` stays 0, cap layout
        roundtrips, error codes (BadCap=1, AccessDenied=2, BadBuffer=3,
        Unsupported=4, BadValue=5) roundtrip
      - kernel side: `sched/syscalls.rs` dispatch skeleton consumes the
        ABI (mechanism stays in `arch/` for Step A)
- [x] Docs: layout tree (galexy-abi + userspace contracts), boundary rule
      8 (ABI stability), DESIGN/ROADMAP/README synced

## Milestone 13 — First user task: rings + syscalls ✅

ROADMAP Step A, executed. **Ring 3 code runs, syscalls work, printing
works, exit reaps.** Every commit left the boot suite green.

- [x] **User tasks in the unified rotation** (`sched/`): `spawn_user_task`
      maps a code page (PRESENT\|USER), a 4-page user stack
      (RW\|NX\|USER) and an RW scratch page at a fresh user region
      (free P4 entry scanned top-down, 512 GiB per task; code at +0,
      stack at +1 GiB). Tasks: `is_user` + dedicated kernel-mode stack
      (heap `Vec`, canary-painted) — timer IRQs from ring 3 push onto it
      via TSS.RSP0 (set on every switch-in to a user task; cleared for
      main/kernel threads). Initial ring-3 frame fabricated by
      `context::init_user_frame` (RIP = code vaddr, user selectors,
      RFLAGS IF=1). kstack registry additionally published for the
      syscall entry (`arch::syscall::set_task_kstack`).
      `bin/test-userpreempt.rs`: blob spins in ring 3, timer keeps
      switching through it, both quanta sides accumulate; verified
- [x] **SYSCALL/SYSRET mechanism** (`arch/syscall.rs`): `STAR`
      (`write_raw(user_cs, kernel_cs)` — SYSRET forces RPL 3 on both CS
      and SS; our consecutive GDT layout satisfies `SS = CS + 8`),
      `LSTAR` → naked entry, `FMASK = 0` (full user RFLAGS carried), then
      `EFER.SCE` last. Naked entry: `cli` → switch to the task kernel
      stack (RSP is the user stack at entry) → push the uniform frame
      (SS, RSP, RFLAGS=r11, CS, RIP=rcx, then GPRs — same shape as the
      timer frame) → Rust dispatch (`rdi` = frame, `rsi` = number) → 0 =
      pop+iretq resume, pointer = switch. Frame bug found by the
      cpl-assert on first run: the CS push was missing (0x202's low bits
      decoded as CPL 2). Kernel-origin syscalls (main/kernel threads)
      fail loudly. `bin/test-syscall.rs`: cap_info echoes the console
      cap's bits into the task's scratch page, kernel polls it through
      the shared address space; verified
- [x] **Syscall behaviors** (`sched/syscalls.rs`):
      - `exit(code)` → tombstone + handoff, never resumes; reaper frees
        user stack pages, scratch, code page, kernel stack + fx. Known
        debt (pre-existing): page-table frames consumed by the spawn's
        mapping chain stay allocated (FreshL4 tree walk, Step B).
      - `yield()` → the real rotation from inside the syscall
        (`sched::syscall_handoff`): save context/fx, advance round-robin,
        switch — result stamped into the frame before the handoff.
      - `write(console_cap, addr, len)` → cap authority (index + WRITE
        right), length cap (1 KiB), page-walk validation of the user
        buffer via `translate`, ASCII-printable staging buffer, screen
        output; result `rax = len, rdx = ok`. Bad cap → BadCap,
        missing right → AccessDenied, unmapped page → BadBuffer,
        non-printable/oversized → BadValue.
      - Register/return contract: `RAX = value`, `RDX = 1 ok / 0 err`.
- [x] **First user program** (`bin/test-user.rs`): hand-assembled blob —
      write("Hello from ring 3!", 18) → yield → write again → scratch
      mark → exit. Kernel asserts: scratch marks done, task
      exited-by-syscall serial marker, reaped, 6 data frames returned to
      the allocator; verified (exit 33; 18 QEMU tests now)
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 14 — Step B: real isolation ✅

ROADMAP Step B, executed. **Every user task owns its address space; CR3
swaps with the rotation; trees are walked back; ring-3 crashes kill only
the crasher.** Suite grew to 19 QEMU boot tests, green per commit.

- [x] **CR3 plumbing (no behavior change)**: kernel table root cached at
      init; `install_cr3()` no-ops when the frame is already active (the
      Redox pattern); `Thread.cr3` (0 = kernel table); switch-in hooks in
      the timer path AND `syscall_handoff` install the incoming CR3 inside
      the IRQ gate — safe by construction: every task table shares the
      kernel half (the FreshL4/M12 contract), so everything the switch
      touches stays mapped across the swap
- [x] **Per-task trees**: `spawn_user_task` builds a FreshL4 at spawn
      (guarded: spawn must run on the kernel tree — a FreshL4 clones the
      ACTIVE table, which must never carry user mappings), then maps the
      task's code page / 4-page user stack / scratch page INTO ITS OWN
      TREE via `with_table` at a scanned top-free P4 entry (< 256). All
      kernel-side staging goes through backing frames (`frame_virt`) —
      the task tree never needs to be active to write it. The initial
      ring-3 frame is fabricated through the phys-map image of the top
      stack page (user-space vaddrs are task-private now).
      `TaskFrameAlloc` made public for out-of-module `map_to` calls.
      Tests poll a task's scratch page via `frame_virt(scratch_phys)`
      (`UserRegion` gained the phys addr). `test-userpreempt` +
      `test-syscall` now headline the CR3-swap crossing; verified
- [x] **Reaper tree walk**: `paging::free_user_tree(root, p4_index)`
      frees the task's whole P4-entry subtree — P3/P2/P1 frames AND data
      frames (the kernel's shared subtrees under other entries are never
      touched). The reaper dropped its unmap-per-page pass; one walk per
      dead task reports `freed task 'X' tree: N frame(s)`. The M12/M13
      table-frame leak debt is CLOSED.
      `bin/test-treechurn.rs`: spawn→exit→reap looped 5×; free-frames
      returns to baseline EXACTLY every cycle (12 frames/cycle: 6 data +
      6 tables); verified (exit 33)
- [x] **Crash isolation**: one GUARD page left unmapped directly below
      each user stack (a fence of absence); the page-fault vector got a
      NAKED handler (same prologue as the timer; the vector carries an
      error-code word, so fields are read by raw offsets and never
      resumed): ring-3 faults tombstone + rotate (the kernel lives),
      ring-0 faults still report + park. The `syscall_handoff` seam took
      a `reason` ("yield"/"syscall"/"page fault" in the serial trace).
      `bin/test-userfault.rs`: blob recurses into the guard page, task
      dies, main keeps rotating, tree fully reclaimed; verified (exit 33)
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 15 — Real userland: runtime + ELF loader + ramdisk ✅

User programs are REAL RUST CRATES now — no more hand-assembled blobs
(blobs remain as the loader's test-side sibling). Suite: 21 QEMU boot
tests + 16 core host tests, green per commit.

- [x] **Ramdisk**: runner's build.rs packs user-program ELFs (artifact
      bindeps, one package dir per program: `dir == pkg == bin`) plus a
      standing `banner.txt` marker into an uncompressed tar
      (`tar` crate in build-deps; header must be sized+checksummed —
      `append_data` on a bare `new_gnu()` header silently records size 0!).
      `set_ramdisk` for BIOS+UEFI images; the kernel reads
      `BootInfo.ramdisk_addr/len` (a VIRTUAL address the bootloader mapped
      — treat like the framebuffer, NOT physical). Kernel-side test
      (`bin/test-ramdisk.rs`): tar walk via the phys map... via the direct
      mapping; banner.txt roundtrips byte-for-byte
- [x] **Tar cursor** (`galexy-core::TarCursor`): read-only USTAR walk
      (512-byte headers, octal sizes, regular files only, extension
      entries + directories skipped, zero-block end). 16 host tests
- [x] **`galexy-rt`** (`crates/userspace/galexy-rt`): the ring-3 runtime —
      inline-asm syscall wrapper (register contract: rax in/out value,
      rdx in/out ok-flag, rcx/r11 clobbered by the instruction),
      `write_console` (console cap from the ABI), `yield_now`, `exit`,
      the `entry!` macro (no_mangle `_start` → main → exit), user panic
      handler (console report + exit 1). Dependency bottom:
      `galexy-rt → galexy-abi` — never the kernel
- [x] **`hello`** (`crates/userspace/hello`): the first real Rust user
      program — `entry!` + `write_console` + return 0. Built as a STATIC
      NON-PIE ELF (`--image-base=USER_IMAGE_BASE` + `--no-pie` — note:
      plain `-Ttext` anchors only text, leaving the ELF-header segment at
      lld's 2 MiB default, OUTSIDE the program's P4 entry!) linked at the
      ABI's fixed base
- [x] **`USER_IMAGE_BASE`** (`galexy-abi`): the fixed virtual load address
      for every user program (per-task trees make sharing the base safe);
      P4 entry 25 of the user half; append-only ABI addition
- [x] **Kernel ELF loader** (`sched/loader.rs`, `xmas-elf`): static
      ET_EXEC only; every PT_LOAD mapped into the task's own tree with
      per-segment flags (RX/RW + USER + PRESENT, BSS tails zeroed +
      page slack), STRICT same-P4-entry policy (segments outside the
      program's P4 entry would map through kernel-SHARED subtree tables —
      rejected loudly), entry validated inside the image region.
      Task model identical: kernel stack + canary via `register_user_task`
      (the sched-owned seam), CR3 own tree, tree-walk reaping
- [x] **Real-program test** (`bin/test-realprogram.rs`): hello's ELF from
      the tar → spawn_program → runs (print on screen through the
      syscall!) → exits 0 via the shim → tombstone + tree walk + reaped;
      verified (exit 33)
- [x] Bug found + fixed en route: the fabricated frame recorded RSP from
      the frame's WRITE position (a phys-map image!) — split
      `init_user_frame(write_top, user_rsp, ...)`; latent in the blob
      path too (Step A/B blobs just never pushed)
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Known limitations / follow-ups

- [ ] UEFI: timer + keyboard dead under UEFI — legacy PIC doesn't exist;
      needs APIC under `arch/` (boot-only behavior now guarded by the UEFI
      smoke test; userland is BIOS-path for now, syscalls work the same)
- [ ] SYSCALL leaves DS/ES/FS/GS as kernel bootstrap selectors when the
      task resumes in ring 3 — user code must not do segment-based
      addressing; proper user segment reload is future segment work
- [ ] `write` printable-ASCII rule is a stand-in for a real console
      charset policy (newlines unsupported yet — the blob prints one line)
- [ ] Tombstone slots live forever (a few bytes per dead thread) — fine
      until tasks churn; a free-list of slots is the fix if ever needed
- [ ] Status bar can overwrite the typing line when the screen is full
      (cursor is restored, but the in-progress line's glyphs are clipped)
- [ ] Framebuffer is used as the bootloader mapped it (deliberate — BootInfo
      exposes no physical framebuffer address; revisit with isolation work)
- [ ] Screen: text-mode cursor (blinking), tab handling, ANSI-ish output
- [ ] Keyboard queue overflow silently drops keys — fine for now, revisit
- [ ] Cooperative-scheduler nits: `run()` sweep fairness mid-sweep;
      TaskCtx's 8 fixed u64 slots (boxed state enum when tasks get richer)
- [ ] TLB efficiency: every CR3 swap is a full flush (no PCID/GLOBAL
      kernel pages) — fine at this scale, revisit if task churn grows
