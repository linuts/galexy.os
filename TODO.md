# TODO

Tracking document for concrete work items. Big-picture direction lives in
`docs/ROADMAP.md`. Check items off as they land and are verified.

Shipped through Milestone 42 (password auth + login screen). **Next
focus:** Phase — Review readiness (Milestones **43–52**), then Phase 6 —
process Caps / init / seats (**53–55**). Plan: `docs/PROCESS.md`. Style:
`docs/STYLE.md`.

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

## Milestone 16 — shell `run <program>` ✅

The shell can launch real userland programs by name. Along the way, TWO
latent kernel bugs surfaced and were fixed; the suite grew to 23 QEMU boot
tests (+ a true typed-keystroke E2E) + 16 core host tests, green per commit.

- [x] **Ramdisk service** (`sched/ramdisk.rs`): `init(boot_info)` publishes
      the bootloader-mapped tar (`ramdisk_addr` = VIRTUAL, framebuffer-like
      contract) once; `find(name)` walks it via `TarCursor`. Test kernels
      stop re-walking the raw BootInfo themselves
- [x] **`run <name>`** (shell.rs): arg → `ramdisk::find` →
      `loader::spawn_program`; unknown program → `run: no such program`
      (not a silent echo); the parsed name is `Box::leak`ed for the task
      record (`&'static str`, same tombstone-slot philosophy). Dispatch
      body factored into `shell::exec(line)` — the typing flow and boot
      tests reach the SAME path. Help text lists `run <program>`
- [x] **Console = screen + serial** (`drivers/console.rs`): the console
      POLICY façade — userland's `write` and the shell's typed-key echo
      both print through it, so userland output is observable headless
      and the harness can sync on guest progress. This mirror is what
      exposed the two kernel bugs below
- [x] **FIX — M14 write-validation regression**: since per-task trees, the
      `write` syscall's buffer page-walk used the KERNEL-ROOTED mapper
      (`arch/mm::translate` — rooted at the boot L4 captured at init) and
      reported **BadBuffer for EVERY user buffer** (blob tests ignored the
      result, no test asserted printed text). Closed with
      `arch/mm::translate_active`: a read-only 4-level walk of the
      CR3-ACTIVE tree through the phys map (huge pages included) —
      `test-realprogram` now asserts hello's text on serial
- [x] **FIX — syscall-entry timer window**: `FMASK=0` left IF set at
      SYSCALL entry; a timer tick landing between the `syscall`
      instruction and the entry's `cli` interrupted at CPL=0 with RSP =
      the USER stack (no RSP0 auto-switch below ring 3), pushed its
      context onto the user stack, and the rotation's CR3 swap then
      unmapped it under the timer's own return path → PF → double fault →
      silent reset (TCG-timing flaky; found via `-d int`). `SFMASK` now
      clears IF+TF at entry (user RFLAGS rides in R11 unchanged)
- [x] **Screen lock gate** (`drivers/screen`): `with_lock`/`pos` are
      IRQ-gated in the module's public API (lock-audit rule) — the first
      ever coexistence of user-task screen writes (IF=0) with the main
      loop's shell output had exposed the gap
- [x] **PIC determinism** (`arch/pics`): explicit masks after remap
      (master 0b1111_1000 = IRQ0/1/2 + cascade, slave 0xFF — the inherited
      BIOS masks are SeaBIOS's polled-keyboard leftovers) + an
      output-buffer flush (a stale POST byte holds the buffer full and
      the first real keystroke never asserts IRQ1)
- [x] **`bin/test-runshell.rs`**: drives `shell::exec("run hello")`
      directly — ramdisk find (positive + negative), full lifecycle
      (exit-by-syscall + tree walk), hello's text on serial via the
      mirror, frames back to baseline; verified (exit 33)
- [x] **Typing E2E** (`runner/tests`): QMP `send-key` types
      `run hello<Enter>` into the LIVE main kernel (real PS/2 IRQs —
      nothing injected kernel-side). Determinism was earned the hard way:
      - fixed sleeps lose keys (init timing varies under TCG) → the
        harness waits for a serial ready-marker (`[boot] main loop ready`)
      - wall-time PACING still loses keys under host load (the guest
        drains at TCG speed; QEMU's 16-deep PS/2 queue overflows and
        silently drops tail keystrokes — deterministic 16-of-20 under
        load, found via `pckbd*` traces) → EACH key syncs on the guest's
        SERIAL echo of it (the console mirror in the other direction);
        Enter syncs on hello's program output
      - the exit handoff lags the program's output by a scheduling
        quantum under slow TCG → a final-marker wait (`exited
        (syscall)`) precedes the assertions
      - QMP replies read through the SAME buffered reader as the
        handshake (a second reader desynchronizes the command/reply
        stream — keys silently misdelivered)
- [x] **Test-harness hardening**: `test-realprogram`/`test-runshell`/
      `test-user` gain a drain phase after `threads_count()==0` (the exit
      handoff can land between `reap()` and the count check — RUNNING-only
      count — leaving the tree unfreed at assert time); liveness test
      window 20s → 45s (TCG-under-load)
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 17 — APIC: LAPIC + I/O APIC on every boot path ✅

ROADMAP Phase 4's APIC item, executed. **The timer is LAPIC-delivered
(MADT-discovered, PIT-calibrated), the keyboard routes through the I/O
APIC, and UEFI boots are FULL first-class citizens** (timer liveness + a
typed `run hello` E2E under OVMF). Suite: 26 QEMU boot tests + 16 core
host tests, green per commit.

- [x] **ACPI discovery** (`arch/acpi.rs`): RSDP (physical addr from
      `BootInfo.rsdp_addr`) → XSDT (v2, 8-byte entries) or RSDT (v1,
      4-byte) → first `APIC`-signed table = MADT; every table checksum-
      validated before trust; malformed/missing = loud panic. Parses and
      publishes: LAPIC MMIO base (header + type-5 override), the boot
      I/O APIC (base + GSI base, the record covering GSI 0), enabled
      CPU count + BSP APIC ID, ISA Interrupt Source Overrides (IRQ0→GSI2
      under QEMU; IRQ1 identity). Reads through the phys map — exists
      from boot, so `arch::init` order vs `mm::init` doesn't matter for
      discovery itself
      `arch::init` signature: now takes `&BootInfo` (reads rsdp +
      phys-offset itself); all 17 test kernels updated (mechanically),
      and their init order normalized to mm-before-arch where APIC needs
      the paging mapper (8 bins reordered)
- [x] **LAPIC** (`arch/apic.rs`): mode DETECTED from MSR 0x1B bit 10 —
      xAPIC (MMIO register page mapped at fixed kernel-half P4 entry 200,
      PRESENT|RW|NX|uncached) vs x2APIC (MSRs `0x800 + offset>>4`); every
      access funnels through one read/write pair so both paths share all
      logic. Bring-up: spurious vector 0xFF + an IDT gate for it (an
      unhandled stray spurious would triple-fault), TPR 0, flat DFR/LDR.
      Real hardware frequently ships x2APIC-enabled; QEMU defaults xAPIC
- [x] **LAPIC timer = THE timer**: calibrated ONCE against a PIT
      channel-2 one-shot (~10 ms window, ratio math only — TCG safe,
      interrupts off) → periodic on VECTOR 32 (unchanged: the naked
      handler, `timer_ticks()` accounting, 1s heartbeat, scheduler
      quantum all keep their meaning; only the delivery path swapped).
      EOI rewire: `arch::end_timer_interrupt` → `apic::eoi()` (the LAPIC
      EOI register is the one true EOI now)
- [x] **I/O APIC** (`arch/ioapic.rs`): register page at fixed kernel-half
      P4 entry 201 (LAPIC's sibling mapping); version sanity, ALL
      redirection entries masked first, then ONE wiring — the keyboard:
      ISA IRQ1 → GSI (MADT override or identity) → RTE pin, vector 33,
      edge/active-high/physical-dest = BSP LAPIC id. Keyboard EOI →
      `apic::eoi()` (edge lines need no IOAPIC-side EOI)
- [x] **PIC demoted, not deleted** (`arch/pics.rs`): remap + BOTH 8259s
      fully masked (masked lines never assert — no lost-EOI ghosts, no
      double delivery on BIOS); PS/2 controller enable + stale-buffer
      drain MOVED to `drivers/keyboard::init()` (i8042 work, not
      interrupt-controller work; runs on every boot path — OVMF may
      leave the port disabled). PIC-fallback for IOAPIC-less hardware =
      future work
- [x] **UEFI tests graduated** (`runner/tests/boot.rs`): the smoke test
      became `uefi_image_boots_and_timer_ticks` — asserts MADT + LAPIC +
      `[timer] 1s up` under OVMF (3 retries for OVMF disk flakiness);
      NEW `shell_run_hello_typing_e2e_uefi` — the full typed `run
      hello` under OVMF via QMP. Harness fix en route: QMP reply reads
      must SKIP async event lines (RTC_CHANGE under OVMF interleaves
      them and desynchronized the reply stream)
- [x] `bin/test-acpi.rs`: MADT parse assertions (bases, gsi 0 coverage,
      IRQ0→GSI2 override, ≥1 CPU) on the BIOS AND UEFI images
- [x] `bin/test-apic.rs`: mode detection (XApic under QEMU), LAPIC page
      mapping, LVT timer state (vector 32, periodic, unmasked), calibrated
      rate sane, ticks accrue LAPIC-side
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 18 — SMP: two CPUs, one kernel ✅

The multicore hop. **Both CPUs run the full kernel: per-CPU timer switches,
naked switch, ring-3 on either core, pinned-at-spawn rotation with owner-
reaping.** Suite: 28 QEMU boot tests, ALL at `-smp 2` (every existing test
exercises the multicore paths), + 16 core host tests. Green per commit.

- [x] **Per-CPU substrate** (`arch/cpu.rs`): identity via GS base (FSGSBASE
      feature asserted + CR4.FSGSBASE enabled at bring-up — the CPUID bit
      alone is NOT enough; WRGSBASE the slot address, roundtrip-verified).
      Fixed-offset contract for the naked asm: gs:[0] = self ptr,
      gs:[8] = SYSCALL kernel-stack target, gs:[16]/[24] = entry scratch.
      MAX_CPUS = 8; logical index + APIC id stored per slot
- [x] **Per-CPU GDT/TSS** (`arch/gdt.rs`): every CPU builds + loads its
      OWN tables (`bring_up(cpu_index)`); the selector layout is fixed and
      identically REPLICATED (STAR constants stay valid machine-wide, with
      build-time debug_asserts against the appended order). Per-CPU TSS
      gives each CPU its own RSP0 + double-fault IST stack
- [x] **SYSCALL entry via gs:[8]** (`arch/syscall.rs`): the global
      TASK_KSTACK static and the rip-relative scratch statics are GONE —
      the naked entry loads the per-CPU registry (gs:[8]) and stashes
      user RSP/rax in per-CPU scratch (gs:[16]/[24]); USER_CS/USER_SS stay
      global constants (CPU-independent)
      `idt`: the APs' IDTR was never loaded — a first tick hit the
      real-mode zero IDTR and triple-faulted (`GP fault (vector<<3)|2`
      dump was the tell). `idt::ap_load()` commits the shared table per
      AP; the IDT itself stays one shared instance (selector-value
      entries are valid in every replicated GDT)
- [x] **AP bring-up** (commit 2, `arch/cpu.rs` + `apic.rs`): a
      position-independent trampoline page at phys 0x8000 — 16→32→64-bit
      walks with push/`retf` transitions (0xCB byte pinned: the assembler
      resolved `retf` to a 0x66-prefixed 32-bit return!), assemble-time
      layout contracts asserted in Rust next to the copy (balign 256
      sections), micro-GDT + handoff slots (cr3/stack/fn/rank/magic) in
      the page tail. INIT → 10 ms → SIPI ×2 (PIT channel-2 millisecond
      delays), then the BSP spin-waits on an online-magic slot the AP
      writes after ITS per-CPU init completes
      AP page tables: one shared tree — a 1-GiB identity low map (the
      trampoline's continuation) + VERBATIM kernel-half L4 copy (needs
      EFER.NXE alongside LME or every NX PTE #PFs with error 0xA!). APs
      run `gdt::bring_up`, `idt::ap_load`, per-CPU GS, `syscall::init`
      (the MSRs are PER-CPU — the AP's STAR/LSTAR were unset until
      re-programmed!), LAPIC bring-up on its own MMIO (per-CPU by
      hardware orientation), blank-idle park
- [x] **Pinned-at-spawn scheduler** (commit 3, `sched/mod.rs`):
      `Thread.owner` (spawn hands the pin round-robin over MADT-enabled
      CPUs, returned to the caller race-free); the global rotation
      statics (CURRENT/LAST_SERVED/MAIN_CTX/MAIN_FX/MAIN_TICKS) became
      the per-CPU `CPU_SCHED` table; rotation scans + handoffs filter by
      owner (foreign slots are tombstone-like deadweights otherwise);
      `reap()` is OWNER-REAP — a CPU frees only its own dead (the
      cross-CPU zombie race is closed by ownership, not locks)
- [x] **Per-CPU timer machine**: the LAPIC timer is armed PER CPU with a
      SHARE-SPLIT ICR (ticks-per-ms × online count) — N CPUs × 1/N Hz
      each keeps the machine-wide tick rate at ~1 kHz, so TICKS/1000
      seconds, the heartbeat and the tests' tick budgets survive. The APs'
      LVTs stay masked until their `arm_timer`; `boot_aps()` also arms on
      the single-CPU path (the -smp-1 hidden-hang caught by the suite)
- [x] **AP idle loop**: `enable_and_hlt` + own `reap()` — each CPU's
      slot-0 main parks with ITS interrupts enabled, its own naked timer
      switch rotates its own threads back into its own main context
- [x] **SMP exposure suite-wide**: the runner boots EVERYTHING at
      `-smp 2 -cpu max` (FSGSBASE needs -cpu max; QEMU's default model
      lacks it) — the 26 pre-existing tests all exercise the multicore
      paths; the threadexit reaper assertion now sums per-owner reaps
- [x] **`bin/test-smp.rs`**: online==2, MADT AP count, pin-RR
      distribution asserted from the spawn return (t1..t4 = 0,1,0,1),
      preemption accumulation (the four spinning threads' CPU-time ticks
      across both per-CPU rotations ≥ 4), full owner-reap drain to zero
- [x] **`bin/test-smpuser.rs`**: TWO ring-3 blobs — one pinned per CPU —
      each completing write/yield/scratch-mark/exit; per-owner tree walks
      reclaim both task trees; data frames × 2 return to the allocator
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 19 — SMP act 2: cross-CPU coordination (the clean base) ✅

The multicore base filesystem capabilities build on. **Kernel-half remaps are
mechanized (precise-INVLPG shootdown IPIs), the scheduler self-balances
(idle-CPU work stealing), and a stress kernel proves the machine closes
under concurrent load.** Suite: 30 QEMU boot tests, all `-smp 2`.

- [x] **IPI infrastructure** (`arch/apic.rs` + `arch/idt.rs`): a dedicated
      IPI vector (0xF8) with a lock-free x86-interrupt handler; per-CPU
      ack atomics; `send_ipi` (M18) gets its first real consumer.
      `send_fixed_ipi` (fixed delivery) works from ANY CPU. The handler
      takes NO locks (the deadlock rule: an IF=0 lock holder must still be
      able to ack a broadcast).
      `bin/test-ipi.rs` round-trips 1-VA and full-16-VA broadcasts end to
      end
- [x] **Precise-INVLPG shootdown** (`arch/mm/shootdown.rs`): the ICR carries
      only 8 bits, so the VAs ride in a static mailbox pool (8 request
      slots of 16 VAs — one heap-growth chunk fits one slot; a monotonic
      machine-global sequence counter closes the ABA window; slots are
      only reused after every target consumed their `seq`). Targets'
      `seen` values are per-CPU per-slot atomics the initiator spins on.
      `mm::shootdown_others()` — broadcast + wait, LOCK-FREE initiator
      (the deadlock rule refined: targets' handlers take no locks; all
      lock holds stay short + IPI-free — the caller must hold NO Rust spin
      lock across the broadcast; any IRQ state is fine).
      `map_kernel_page_broadcast` (map + local flush + one-VA broadcast)
      is the single-page primitive. Heap `grow()` batches: gated
      `map_page` of the chunk, one lock-free `shootdown_others` for every
      new VA (a 16-page chunk fills one mailbox slot), then `extend` —
      the "kernel half is map-only" M18 assumption MECHANIZED (naked
      paths never allocate → never reach the broadcast). The GROWING-conflict path now WAITS for
      the in-flight growth (IF=1 spin + watchdog) instead of failing the
      alloc into a null-abort — a stress essential (both CPUs hammering
      the heap otherwise abort randomly).
      Task-tree maps stay local (per-CPU CR3s); only kernel-half runtime
      changes broadcast
- [x] **Work stealing** (`sched/mod.rs`): idle CPUs (no runnable OWNED
      thread — the alternate-with-main pattern is NOT idle) steal under
      the THREADS lock, only slots provably NOT current on their owner
      (`owner_cpu.current != slot` is necessary but not sufficient). Steal
      = owner flip of a slot whose saved context is STABLE. The victim's
      naked tail publishes that flag only AFTER `mov rsp` off the task
      kstack (gs:[40] = the departed slot; timer, syscall, and page-fault
      tails all do it). The lock drops before that tail, so a one-tick
      delay is not the proof — a host-starved victim vCPU can still be
      unwinding the frame when the stealer's next guest tick arrives
      (two CPUs popping one frame; `test-treechurn` under a parallel
      suite). Entry stays deferred to the stealer's next rotation scan,
      which rides the saved context — all per-CPU switch-in machinery
      (kstack slot gs:[8], TSS.RSP0, CR3) is CPU-agnostic and slot-held.
      A `stolen_at` cooldown (~100 ticks) stops idle-CPU ping-pong; one
      steal attempt per idle pass; stealing correctness rides the
      existing THREADS-lock serialization (no new locks)
- [x] **`bin/test-smpstress.rs`**: phase A steal proof (spinner on the
      BSP + flash on the AP → the AP idles → steals; owner flipped + ticks
      accrued there + cooldown stick asserted); phase B two concurrent
      1-MiB growers (real GROWING-conflict waits + shootdown broadcasts
      counted); phase C dual-CPU hammering (alloc/free + full-page touch)
      + ring-3 blob lifecycle + EXACT frame-accounting closure (heap size
      unchanged, free_frames back to baseline exactly)
- [x] **`test-heapgrow` gains the shootdown marker** (it boots `-smp 2`
      and grows the heap — the broadcast path runs for real; asserts
      `broadcast_count() >= 1`)
- [x] **Clippy repo-wide works again** (pre-existing `manual_div_ceil`
      lint breakage in galexy-core/tar.rs — one-liner)
- [x] Docs synced (TODO/DESIGN: shootdown rules + steal protocol +
      concurrency model, ROADMAP, README).

## Milestone 20 — Files as capabilities ✅

The capability model meets a real resource. **A task `open`s a ramdisk
file by exact name and gets a private READ cap; `read` copies the next
bytes; `close` drops the slot.** Suite: 31 QEMU boot tests, all `-smp 2`.

- [x] **ABI** (`galexy-abi`): `open`=4 (the reserved slot), `read`=5,
      `close`=6. Errors `NotFound`=6 and `NoResource`=7. File indexes
      start at `FILE_CAP_BASE` (3), after null / console / self. The
      effective right is kernel grant ∩ handle snapshot
- [x] **Per-task table** (`sched`): 8 slots carved into `Thread` at spawn.
      `open` does not allocate — the syscall runs IF=0, and a heap grow
      there would broadcast a shootdown. Slots die on reap. Two tasks'
      index 3 are different opens
- [x] **`open` / `read` / `close`** (`sched/syscalls.rs`): exact ramdisk
      name (`banner.txt`, `hello`); `read` short-reads at 1 KiB and
      returns 0 at EOF; user buffers must be `USER_ACCESSIBLE` (a
      destination must also be writable) so a kernel address is not a
      buffer
- [x] **`bin/test-open.rs`**: missing name → NotFound, `banner.txt`
      bytes match, a WRITE-only forgery of the same index is
      AccessDenied, EOF returns 0, close then read is BadCap
- [x] **`galexy-rt`**: `open` / `read` / `close` wrappers
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 21 — A ring-3 shell ✅

The interactive shell is a user program. **It reads the keyboard
capability, writes the console, and `spawn`s ramdisk programs through
the loader capability.** The kernel keeps the status bar and performs
the ELF load on its own page table. Suite: 31 QEMU boot tests, all
`-smp 2`.

- [x] **ABI**: keyboard index `0x8000`, loader index `0x8001` (high
      reserved band, above file caps). `spawn`=7. `read` on the keyboard
      returns 0 when no key is waiting. `spawn` requires EXEC and does
      not return until the program has exited
- [x] **Park, don't clone a user page table** (`sched`): the syscall
      copies the name into one fixed slot and marks the caller
      `WAITING`. The main loop drains that slot with `spawn_program`
      (kernel CR3). Exit, including a ring-3 fault, wakes the waiter
- [x] **BSP-resident** (`no_steal`): the shell is pinned to CPU 0 and
      idle stealing skips it. The keyboard IRQ and the framebuffer stay
      single-consumer
- [x] **`crates/userspace/shell`**: line editing (backspace, form-feed
      clear), `help` / `about` / `run <program>`. The boot path spawns
      it instead of `shell::poll`. `shell::exec` remains for the
      in-kernel `run hello` test
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 22 — Query capabilities ✅

The ring-3 shell can see the machine again. **`stats`, `tasks`, and
`threads` are reserved caps; `read` copies a fresh text snapshot.**
Suite: 32 QEMU boot tests, all `-smp 2`.

- [x] **ABI**: indexes `0x8002` / `0x8003` / `0x8004`, next to the
      keyboard and the loader. READ required. Each `read` is a new
      snapshot (no cursor, so `0` means the caller asked for no bytes)
- [x] **No allocation on the syscall** (`sched/syscalls.rs`): the text
      is rendered into the staging array. Thread names are walked under
      `THREADS` without building a `Vec`
- [x] **Shell commands**: `stats`, `tasks`, `threads` print that text.
      A typed QMP test runs all three and checks `frames free:`,
      `thread-a:`, and `cooperative tasks:`
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 23 — Shutdown and reboot ✅

The ring-3 shell can turn the machine off or reset it. **`shutdown`
and `reboot` are a reserved power cap; the syscall programs the
firmware's PM1 / reset registers.** Suite: 34 QEMU boot tests, all
`-smp 2`.

- [x] **ABI**: index `0x8005`, `CapRights::POWER`, `power`=8. Operands
      are shutdown (0) and reboot (1). A return means the platform
      ignored the request (`Unsupported`)
- [x] **FADT + `_S5_`** (`arch/acpi.rs`): PM1 control ports, SMI enable,
      reset register. The DSDT scan accepts only `Name(_S5_, Package)`.
      A missing FADT is logged; the MADT is still required
- [x] **`arch/power`**: S5 write, then the PIIX4 port `0x604` if still
      up. Reset prefers the FADT register, then i8042 `0xFE`. Both run
      with interrupts off and print before the port write
- [x] **Shell**: `shutdown` and `reboot`. If the call returns, the
      shell says the machine stayed up
- [x] **`bin/test-shutdown` / `bin/test-reboot`**: QEMU exits, no panic
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 24 — Console sequences ✅

The framebuffer speaks a small terminal subset. **Tab, CR, and CSI
render on the screen; serial still gets the raw bytes. Text scrolls
above the status row.** Suite: 34 QEMU boot tests, all `-smp 2`.

- [x] **Charset** (`write`): tab `0x09`, CR `0x0d`, ESC `0x1b`, beside
      the bytes the shell already used. Anything else is still
      `BadValue`
- [x] **Parser** (`drivers/screen.rs`): fixed-size, under the screen
      lock, no heap. SGR 30–37 and 90–97, cursor `H`/`f`/`A`–`D`,
      erase `J` and `K`. A sequence may span two writes
- [x] **Status row**: the last row is not part of the text scroll. The
      bar is drawn with `out_plain`, so a half-read CSI sequence cannot
      swallow it
- [x] **`bin/test-screen`**: tab column, CR, red SGR, cursor place,
      erase leaves the status marker, one text scroll, and the raw
      CSI bytes on COM1
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 25 — List the ramdisk ✅

The shell can see what it can run. **`ls` reads a reserved files
cap; the snapshot is the archive's regular names, one per line.**
Suite: 34 QEMU boot tests, all `-smp 2`.

- [x] **ABI**: index `0x8006`, READ, same high band as the other query
      caps. No new syscall. Each `read` is a new snapshot
- [x] **No allocation on the syscall**: `TarCursor` walks names into
      the stack buffer. An empty archive is still one newline, so a
      positive `len` does not come back as `0`
- [x] **Shell**: `ls`. The existing query typing test also types `ls`
      and checks `banner.txt` and `hello`
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 26 — Reuse thread slots ✅

The table still holds 64 records, and a freed one is handed out again.
**Names live in the slot, so `run` no longer leaks a string per spawn.**
Suite: 35 QEMU boot tests, all `-smp 2`.

- [x] **Name buffer**: 64 bytes on the thread, copied at spawn. The
      kernel shell and `drain_spawn` drop their `String` when the call
      returns
- [x] **Reuse in place**: a `Freed` slot is overwritten when no CPU's
      `current` is that index and `CTX_STABLE` is set. Stacks and the
      FXSAVE area are still freed by the owner first. Steal rules are
      unchanged, and live indexes do not move
- [x] **`bin/test-reuse`**: 80 short threads exit, one after another,
      and a thread named `keeper` is still listed
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 27 — Scratch files ✅

The tar stays immutable. `open` still grants READ on an archive entry.
**`create` adds a name in a fixed table and returns a cap that can be
written.** Suite: 36 QEMU boot tests, all `-smp 2`.

- [x] **`create` = 9**: eight files, 64-byte names, 256-byte buffers.
      No heap on the syscall path. The cap has READ and WRITE. `close`
      drops the task's handle; the bytes stay until reboot
- [x] **`write`** on that cap appends into the buffer (a read still
      starts at the beginning). A tar name, or a second `create` of
      the same scratch name, is `Unsupported`. A full scratch table
      or a full per-task file table is `NoResource`
- [x] **`bin/test-scratch`**: create, write, read back, `banner.txt`
      stays the archive copy, and the ninth scratch file is
      `NoResource`
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 28 — Clone the kernel page table ✅

A new program gets its own page table, copied from the kernel root
cached at boot. **A child cannot inherit another task's user
mappings.** Suite: 37 QEMU boot tests, all `-smp 2`.

- [x] **`FreshL4`** copies `kernel_cr3()`, the root cached at init.
      The active CR3 is not the source
- [x] **The load stays on the main loop.** The loader allocates, and
      a syscall runs with interrupts off. `spawn` still drains there
- [x] **`bin/test-cloneroot`**: a user page is mapped into one table,
      that table is installed, and the next fresh table does not
      contain the page
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 29 — Core utilities ✅

The ring-3 shell can print, read, and write scratch files, and walk
directories on the Milestone 27 table. `rm`, pipes, `mv`, `cp`, globs,
and a real disk stay out. Suite: 38 QEMU boot tests, all `-smp 2`.

- [x] **Shell only**: `echo` prints its arguments. `cat <name>` opens,
      reads, and writes the console. Text files work (`banner.txt`).
      A binary such as `hello` still fails the console charset check
- [x] **`touch`, `echo >`, `echo >>`**, then `cat` of that name. A tar
      name cannot be replaced. `create`'s third argument is `1` to
      empty an existing scratch file; any other value still refuses a
      second create
- [x] **Directories on that table**: `mkdir`, `cd`, `cd ..`, and `ls`
      of the current directory. The shell keeps the current path.
      Archive files stay at `/`. `run hello` stays a ramdisk program
      name, not a path
- [x] A boot test covers `cat banner.txt`, `mkdir` / `cd` / `ls`, and
      a create-write-read of a scratch file
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 30 — Remove scratch names ✅

`rm` frees a scratch file or an empty directory so the eight slots can
be used again. A directory that still has a child stays. A tar name
cannot be removed. Suite: 39 QEMU boot tests, all `-smp 2`.

- [x] **`remove` = 10**: the path names a scratch file or an empty
      directory. The slot is freed. An open cap on that file becomes
      `BadCap`. A ramdisk name, or a directory with a child, is
      `Unsupported`. A missing path is `NotFound`
- [x] **Shell**: `rm <name>` in the current directory. `rm box` while
      `box` still holds a file says the directory is not empty
- [x] **`bin/test-rm`**: remove, a stale cap, a tar name, a missing
      name, a non-empty directory, then eight new files and a ninth
      `NoResource`
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 31 — Launch by name ✅

Typing a ramdisk program name starts it. The new task is granted the
console only. The shell keeps the keyboard, the loader, the query caps,
and power. Suite: 39 QEMU boot tests, all `-smp 2`.

- [x] **No `run` verb**: a single token that names an ELF is spawned.
      `hello extra` is not a launch. `banner.txt` is `Unsupported`
- [x] **Grants**: recorded on the task at spawn. Console `write`,
      keyboard `read`, `spawn`, query `read`, and `power` require the
      matching grant. The boot shell receives the launcher set. A
      program started from the shell receives the console only
- [x] Typing `hello` on BIOS and UEFI still prints the program's line.
      `shell::exec("hello")` drives the same kernel-shell path
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 32 — Shell supervisor ✅

A utility runs as its own task. `spawn` returns once that task is
loaded, so the prompt comes back while it runs. A shell fault loads a
new shell; other tasks keep running. Suite: 39 QEMU boot tests, all
`-smp 2`.

- [x] **`spawn` returns at load**: the caller is parked only until the
      main loop has loaded the ELF. The child's exit wakes nobody.
      `r8`/`r9` are an argument of at most 256 bytes. `r10` bit 0 adds
      the query grant. The child always has the console. Keyboard, the
      loader, and power stay with the shell
- [x] **Utilities**: `echo`, `cat`, `touch`, `mkdir`, `rm`, and `ls` are
      ramdisk programs. The shell still owns the current directory and
      composes the path. `cd` and the power and status commands stay in
      the shell
- [x] A shell fault loads `shell` again. The BIOS typing test starts
      `linger`, types `crash`, sees the new prompt, and still sees `beat`
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 33 — One console per F-key ✅

F1–F12 each show a saved text grid and a shell. The keyboard interrupt
records the switch. The main loop paints it. Suite: 40 QEMU boot tests,
all `-smp 2`.

- [x] **Twelve grids.** A console write records characters on the task's
      TTY. Pixels change only while that TTY is visible. The status bar
      stays on the last row. COM1 mirrors the visible TTY
- [x] **Keys follow the screen.** F1–F12 are not typed characters. Each
      TTY has its own queue. A background shell's `read` does not take
      the foreground's keys
- [x] **One shell per key,** pinned to the BSP with the launcher grants.
      F1 stays named `shell`. A fault loads that shell again. A program
      started on a console keeps writing it
- [x] `bin/test-screen` hides a glyph and restores it, including a write
      that happened while it was hidden. The typing test runs `echo hi`
      on F2, then a key on F1
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 34 — galfs tokens ✅

Each person has one root. `/Desktop` is yours; `/dan@Desktop` is
dan's. A token names an object and its rights. Suite: 41 QEMU boot
tests, all `-smp 2`.

- [x] **Objects and actors**: 32 fixed objects replace the scratch
      table. Boot creates actor `alex`. Shells and test blobs hold a
      full token on alex's root. Spawn copies the parent's tokens
- [x] **Paths**: optional leading `/`; first component may be
      `owner@name`. Token checks on open, create, write, remove, and
      the files snapshot. No covering token is `AccessDenied`
- [x] The shell accepts `/Desktop` and `/dan@Desktop` for `cd` and the
      utilities. Existing mkdir/echo/rm/ls typing still works on alex
- [x] `bin/test-galfs` denies `/dan@Desktop` until a list+read token is
      installed, then lists and opens it
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 35 — grant syscall ✅

A task that holds rights on an object can install a token on another
live user task. Suite was 41 QEMU boot tests (`test-galfs` extended).

- [x] **Syscall `grant`=11**: path + rights (`TOKEN_*`) + target task
      name. Caller must hold every bit being granted. Same-object tokens
      merge rights. Missing path or task is `NotFound`
- [x] Shell builtin `grant <rights> <path> <task>` (`r`/`w`/`l`/`c`/`x`)
- [x] `bin/test-galfs`: dan grants `/Desktop` list+read to `reader`;
      reader opens `/dan@Desktop/secret`
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 36 — revoke, pipes, seek, mv/cp ✅

Complete the share path and a few utilities. Suite: 43 QEMU boots.

- [x] **`revoke`=12**: same layout as grant; clears rights on the
      target's exact-object token. Shell `revoke <rights> <path> <task>`
- [x] **Boot actor `dan`**: Desktop at init; F2's shell (`shell2`) gets
      dan credentials so grant works from the console
- [x] **`pipe`=13 / `give`=14**: anonymous pipe with read+write caps;
      `give` moves an open to another live task. `bin/test-pipe`
- [x] **`seek`=15**: SET/CUR/END on archive and galfs opens. `bin/test-seek`
- [x] **`cp` / `mv`** ramdisk utils (mv is copy then remove)
- [x] `bin/test-galfs` proves grant then revoke
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 37 — user management ✅

Actors are accounts. Suite: 44 QEMU boots.

- [x] **Syscall `user`=16**: ops whoami / users / add / del / su
- [x] Shell: `whoami`, `users`, `useradd`, `userdel`, `su`
- [x] Add/del require admin's root; su needs admin / born-admin return /
      ALL on the target (Milestone 40 drops kept tokens across su)
- [x] `bin/test-users` covers the happy path and access checks
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 38 — disk-backed galfs ✅

The table lives on the primary IDE slave when QEMU attaches one.
Suite: 45 QEMU boots.

- [x] **ATA PIO** (`drivers/ata.rs`): LBA28 read/write on primary slave
      (index=1). Absent slave → galfs stays RAM-only (existing suite)
- [x] **GALF image**: 24 sectors at LBA 0; load-or-format in `galfs::init`;
      sync after create/remove/append/useradd/userdel
- [x] `bin/test-galfs-disk` + runner `boot_with_galfs`: write on boot 1,
      verify on boot 2 with the same data image (boot drive still snapshotted)
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 39 — hardened galfs (crash-safe + capacity) ✅

Make the on-disk table production-shaped for a single-seat console.
Suite: 46 QEMU boots.

- [x] **GALF v2 dual slots**: generation + IEEE CRC-32; sync writes the
      inactive slot then `FLUSH CACHE`. Load picks the newest valid slot;
      structural validation rejects corrupt-but-checksum-ok images
- [x] **Capacity**: 16 actors, 64 objects, 512-byte files; alex and dan
      each get Desktop at format
- [x] **userdel**: refuses open caps on the actor's objects; drops tokens
      that named them
- [x] **Interactive persistence**: `cargo run` attaches `galfs.img` as
      IDE slave (boot drive still snapshotted)
- [x] `galfs_disk_recovers_from_corrupt_slot` host-corrupts the newest
      slot between boots
- [x] `galexy-core::crc32` host-tested
- [x] Docs synced (TODO/DESIGN/ROADMAP/README).

## Milestone 40 — shell and identity cleanup ✅

Default seat is `admin` only. Suite: 46 QEMU boots.

- [x] **Boot actor `admin`**: format creates admin + Desktop; alex/dan
      removed. GALF disk version 3. All shells start as admin
- [x] **`su` isolation**: replace tokens with ALL on the target; born-admin
      seats may return with `su admin`; shell resets cwd on su
- [x] **`SPAWN_WAIT`**: shell utilities park until the child exits (fixes
      `ls` needing an extra Enter). Bare launches (`hello`, `linger`) do not
- [x] **TTY cursor**: console writes save the focused TTY cursor so the
      input line stays visible after status-bar / focus churn
- [x] **Prompt**: `user@galexy>` / `user@galexy:/path> ` (whoami each draw)
- [x] Tests/docs: `test-users`, `test-galfs`, `test-rm`, `test-galfs-disk`

## Milestone 41 — spawn hardening ✅

Stop nested-shell loops and tighten spawn policy. Suite: 47 QEMU boots.

- [x] **Reserved names**: user `spawn` of `shell`…`shell12` is `Unsupported`
      (F-key consoles only). Shell prints `shell: reserved (use F1-F12)`
- [x] **Unique live names**: spawning a name that is already running/waiting
      is `NoResource` (SPAWN_WAIT wakes by name)
- [x] **Keyboard denied exits**: a shell without the keyboard grant exits
      instead of spinning "read: keyboard denied"
- [x] Typing e2e: `shell_nested_spawn_refused_e2e`

## Milestone 42 — password auth + least privilege ✅

Passwords authenticate; tokens authorize. See `docs/AUTH.md`. Suite grows.

- [x] **AUTH.md**: password login + access-card tokens (no disk encryption yet)
- [x] **GALF v4**: actor salt+hash; default admin password `admin`
- [x] **`login` / `passwd` / `useradd <name> <pass>`** syscalls + shell
- [x] **F2–F12 guest** seats (no power); F1 stays admin at boot
      *(superseded: every seat now boots logged out; see login-on-boot)*
- [x] **Spawn**: utilities inherit tokens; bare programs get empty tokens
- [x] **Console budget**: 512 bytes/tick short-write (bounds linger floods)
- [x] **`crash` omitted from `help`** (test seam kept)
- [x] Actor-root path `/eve@/` for granting login cards

Open follow-ups moved into Milestones 43–52 (review readiness).

## Phase — Review readiness (systems-engineer bar)

Milestones below prep galexy.os for an external systems review: auth that
survives a stolen disk image, a filesystem usable beyond demos, and kernel
edges that a reviewer will poke. **Ten milestones (43–52)** — each keeps
the full former checklist as subsections (nothing dropped). Order is
dependency-aware; each milestone must leave the suite green.
Conventions: `docs/STYLE.md`. Process **init / seats** land in Phase 6
(53–55) after `review-rc1`; Milestone 47 ships the **process-Cap**
foundation (not a Unix PID ABI).

---
## Milestone 43 — Auth hardening

Passwords prove identity; seats and spawns stop oversharing. Includes
crypto, no-echo prompts, session hygiene, least privilege, and monotonic
time for lockout/idle.

### Password crypto (KDF + random salts)

Replace demo hashing before any other auth work depends on the on-disk shape.

- [x] **CSPRNG**: `arch::rand` — RDRAND with tick-mixed xorshift fallback
      (documented). Used for salts; later AEAD nonces too
- [x] **Random salts**: `useradd` / `passwd` / format fill 8-byte salts
      from the CSPRNG; `salt_from_seed` test-only in `galexy-crypto`
- [x] **Real KDF**: PBKDF2-HMAC-SHA256 in `galexy-crypto` (10 000
      iters — debug-QEMU budget; raise later); GALF **v5** (same 8+16
      on-disk widths; v4 images refused).
      Argon2id deferred until a dedicated KDF stack (12 fat kstacks
      broke multi-seat boot)
- [x] **Constant-time verify** retained; host unit tests for wrong /
      truncated password and distinct salts for the same password
- [x] **Docs**: `AUTH.md` names the KDF + parameters; CRC mix retired
- [x] **Password policy (minimal)**: reject empty passwords; max 64
      bytes (syscall staging); ASCII graphic + space
- [x] **Wipe**: zero password staging buffers after login/useradd/passwd;
      wipe derived-key scratch after verify; host wipe unit test
- [x] Suite: `test-users` + galfs (+ disk) green on GALF v5
- [ ] **Follow-up**: Argon2id on a dedicated KDF stack / arena (keep
      32 KiB task kstacks)

### Interactive secrets (no-echo prompts)

Passwords must not appear in the shell line, COM1 mirror, or argv.

- [x] **Shell secret read**: `read_line(..., secret)` echoes `*`;
      Backspace works; cleartext never passed to `write_console`
- [x] **`login` / `passwd` / `useradd`**: interactive prompts when args
      omitted (`login eve` → `Password:`). `passwd` always prompts
      masked `Password:` + `Confirm:` (no inline secret). Inline
      `login eve secret` kept for scripts; typing e2e covers masks
- [x] **Serial policy**: secret mode only writes `*` (COM1 mirrors
      `write_console` on the visible TTY — no cleartext path)
- [x] **Abort**: Esc or Ctrl-C cancels a password prompt (`MapLettersToUnicode`)
- [x] **Length cap**: overlong input → `password too long` / `input too long`;
      buffer wiped; no silent truncate
- [x] Typing e2e: login screen + CLI `login admin` mask; serial has no
      cleartext after `Password: `
- [ ] **Kernel** (optional follow-up): one-shot scratch password syscall
      if keeping secrets out of argv is needed beyond interactive prompts

### Session hygiene

Make seats behave like accounts, not permanent admin shells.

- [x] **Login on every boot seat**: F1–F12 start logged out (no guest
      account); login screen `Galexy.OS v… (ttyN)` + masked password
- [x] **`logout`**: clear tokens, `fs_root = none`, pre-login grants,
      reset cwd; return to the login screen
- [x] **Pre-login grants**: console + keyboard only (no loader / query /
      power) until `login`; admin login restores power
- [x] **Force admin password change** (shell): login as `admin`/`admin`
      sets a seat flag; only `passwd` / `help` / `whoami` / `logout` until
      `passwd` succeeds. Typing e2e clears the default before other cmds.
- [ ] **Kernel must-change** (follow-up): persist flag on the actor /
      deny mutating syscalls so non-shell clients cannot skip the gate
- [ ] **Login lockout**: after N failures per actor (and/or per TTY),
      refuse further attempts for a cool-down; count visible via `stats`
      or serial audit line
- [ ] **Remove `crash` from production shells**: `cfg` / build feature so
      release images omit the seam; keep it only on test kernels that the
      supervisor e2e uses
- [ ] **Idle timeout (optional but planned)**: after N seconds with no
      keys on a logged-in seat, auto-`logout` (needs monotonic clock below)
- [ ] **Session id / generation**: bump a counter on login/logout so
      stale grants targeting a recycled task name cannot confuse audits
- [ ] Tests: lockout trips; must-change blocks `touch` until `passwd`

### Least-privilege seats & spawn

Tighten who can run code and what cards they carry.

- [x] **Pre-login without loader** (ships with login-on-boot)
- [ ] **Narrow admin operator bypass**: remove blanket `token_allows`
      success for admin root. Keep admin-only for `useradd` / `userdel` /
      `passwd <other>`; foreign trees require an explicit card (or a new
      `USER_IMPERSONATE` that installs ALL on a named root and is audited)
- [ ] **Spawn rights attenuation**: `SPAWN_WAIT` utilities inherit a
      *filterable* token set (default: parent's tokens; optional mask in
      `r10` or a follow-up ABI). Bare spawn stays empty-token
- [ ] **Ramdisk trust note**: document that every SPAWN_WAIT binary is
      trusted code with the caller's cards; Milestone 51 signs/measures
- [ ] **Login cards**: document semantics; add optional one-shot revoke
      on first `su` with a card (flag on the token or grant path)
- [ ] Update `test-galfs`: admin listing foreign trees without a card
      must fail again once bypass is removed
- [ ] Docs: `AUTH.md` operator model kept current with bypass changes

### Timekeeping for auth and audit

Lockout and idle logout need a trustworthy clock source.

- [ ] **Monotonic time**: expose ticks or a `clock` query (LAPIC-based);
      document resolution and wrap behavior
- [ ] **Wall clock (optional)**: CMOS/UEFI runtime clock or “no wall
      clock” waive — audit lines may use monotonic only
- [ ] **Lockout cool-down** wired to monotonic time (session items above)
- [ ] **Idle logout** wired to monotonic time (session items above)
- [ ] **Timeout helpers** in tests (QEMU accelerate / tick injection)
- [ ] Docs: time model for reviewers (what is and is not synchronized)

## Milestone 44 — Sealed GALF (disk encryption)

Protect the ATA image at rest (password hashes and file bytes).

- [x] **Threat model paragraph** in `AUTH.md`: stolen `galfs.img` /
      disk; cold boot out of scope initially
- [x] **Wrapping**: KEK = PBKDF2(volume passphrase); wrap a random
      32-byte volume key (ChaCha20-HMAC) into the GALF **v6** header
- [x] **Payload encryption**: ChaCha20-HMAC-SHA256 Encrypt-then-MAC over
      the actor/object blob; CRC of ciphertext kept as a cheap pre-check
- [x] **Header AAD**: magic + version + generation bind the wrap and
      payload tags (slot splice rejected)
- [x] **Boot unlock (bring-up)**: auto-unlock with `VOLUME_PASSPHRASE`
      (`galfs`); sync refused while locked
- [x] **Test**: `test-galfs-disk` round-trip; host asserts `persist-ok`
      is absent from raw `galfs.img` bytes
- [ ] **Interactive unlock**: prompt on F1 (or runner flag) before
      mounting; wrong passphrase → refuse mutate / RAM-only fallback
- [ ] **Key wipe**: volume key zeroed on logout/shutdown path where held
      in RAM; document residual cold-boot risk as accepted
- [ ] **Poly1305 follow-up**: swap HMAC tag for RFC 8439 Poly1305 without
      resizing the v6 header (same key/nonce/tag widths)
- [ ] Explicit non-goal until later: per-file keys, secure erase, TPM seal

## Milestone 45 — galfs for real usage

Capacity, operations, durability, and multi-user sharing.

### galfs capacity & on-disk layout

GALF **v11**: 32 actors / 128 objects / 256×512 block pool / 8 directs +
single indirect (32 KiB max file) + per-actor quotas + durable shares.

- [x] **Sizing plan** in `DESIGN.md` / `GALFS.md`: 32 actors, 128 objects,
      256 blocks × 512, 8 directs + single indirect/file, depth 8 — still
      no heap on IF=0
- [x] **GALF version bump**: v8 header carries actor/object/block counts;
      refuses older images; **v9** quotas; **v10** shares; **v11**
      single-indirect + 32 KiB max (`len` stays u16)
- [x] **Block store (direct + single indirect)**: shared pool + 8 direct
      pointers + one indirect block of u16 indexes; empty files cost one
      inode; sealed payload includes bitmap+blocks
- [x] **Larger files (to 32 KiB)**: multi-block append/read/truncate across
      direct and indirect; `bin/test-indirect`; double-indirect still open
- [x] **More opens**: keep **8** per-task opens (`MAX_OPEN_FILES`) —
      intentional for the IF=0 syscall path (no heap / shootdown on
      `open`); documented in `DESIGN.md` / `GALFS.md` (raise later with
      a non-blocking growth plan)
- [x] **Stress test (objects + blocks)**: `test-scratch` / `test-rm` /
      `test-blocks` fill to exhaustion and reuse after remove
- [x] **Free-block bitmap**: in-image bitmap; validate_table rejects
      leaks/duplicates (host fsck tool still open)
- [x] **Shared on-disk defs**: `galexy-galf` holds layout constants +
      decode/check; kernel asserts matching magic/version/sizes (explicit
      little-endian encode already); fuller `zerocopy` share still open
- [x] Suite growth: object + block + indirect capacity under QEMU;
      fragmentation / double-indirect still open

### galfs operations for real usage

Fill semantic gaps reviewers expect from a small FS.

- [x] **`rename` / `mv` in-FS**: `Syscall::Rename`; shell `mv` tries rename
      first (copy+remove fallback)
- [x] **`truncate` / set-size**: `Syscall::Truncate` on a file cap; shrink
      frees blocks; grow zero-fills; `create` replace flag unchanged
- [x] **`stat`-shaped query**: `Syscall::Stat` → [`STAT_LEN`] buffer (kind,
      size, owner, held rights); shell `stat` util
- [x] **Directory read**: keep **files snapshot** only (`FILES` cap /
      `ls`); `open` on a directory stays `Unsupported` (documented)
- [x] **Write cursor**: **append-only** `write` is permanent for review;
      `seek` moves the read cursor only (documented)
- [x] **Empty-dir rules / non-empty `userdel`**: refuse non-empty delete
      (unchanged); no recursive `rm -r` — document in `GALFS.md`
- [x] **Hard links / symlinks**: non-goal for review (documented in
      `GALFS.md`; no link syscall)
- [x] **Sparse files**: non-goal — truncate grow allocates zeroed blocks
- [x] **Name charset**: alphanumeric plus `.` `_` `-`; reject `.` / `..`
      (documented; existing `component_ok`)
- [x] Tests: `test-ops`; `DESIGN.md` / `GALFS.md` syscall notes updated

### galfs durability, sync, and recovery

Dual-slot CRC is a start; make failure modes explicit and operable.

- [x] **Sync policy**: every successful mutate that changes the table
      syncs the inactive slot + flush; `Syscall::Sync` is an explicit
      barrier (shell `sync`); RAM-only is a successful no-op
- [x] **Dirty / generation UI**: boot serial names loaded slot + gen;
      `(recovered from bad sibling)` when a newer slot failed; recovery
      counter in `galfs::recoveries()`
- [x] **Live `fsck` smoke**: `bin/test-fsck` runs `validate_table` after
      create / multi-block write / truncate / remove (host offline fsck
      tool still open)
- [x] **Host `fsck` tool**: `crates/galexy-galf` shared layout + unlock;
      `galfs-fsck` CLI reports orphans / bad parents / leaked blocks /
      both-corrupt (repair-into-new-slot still open); runner checks a
      guest-written image
- [x] **Corrupt-slot recovery**: runner breaks the newest slot's AEAD tag;
      verify boot recovers from the older sibling
- [ ] **Crash injection**: runner helper that kills QEMU mid-mutate;
      assert the next boot picks a consistent slot
- [x] **Refuse silent format**: both slots fail with GALF magic →
      galfs unavailable (`DISK_CORRUPT`); no RAM invent of admin;
      `bin/test-galfs-corrupt` + runner both-corrupt harness
- [ ] ATA errors: surface `Unsupported` / logged I/O error instead of
      panicking where possible
- [x] **Torn-write test**: host zeros newest slot from mid-sector (keeps
      `GALF` magic); guest recovers from older sibling
      (`boot_with_galfs_torn` + `galfs_disk_recovers_from_torn_write`)
- [x] **Idempotent mutate**: `bin/test-galfs-idempotent` + recover harness —
      duplicate create is `Unsupported`, remove+recreate+sync advances
      gen, `fsck_ok` holds

### galfs sharing, quotas, and cards

Multi-user usage beyond one admin and ad-hoc grants.

- [x] **Per-actor quotas**: max objects + max bytes on each actor (GALF
      v9); defaults for new users; admin unlimited; `NoResource` on
      create/append/truncate/cross-actor rename when exceeded;
      `USER_QUOTA` / `USER_SETQUOTA`; shell `quota` / `quota set`;
      `bin/test-quota`
- [x] **Token table UX**: shell `tokens` + `USER_TOKENS` lists the
      current task's cards (path + rights letters)
- [x] **Grant to actor vs task**: live `grant`/`revoke` stay task-scoped;
      durable home shares are GALF v10 (`Share`/`Unshare` syscalls, shell
      `share`/`unshare`, 32 share slots, re-applied in `install_session`);
      `bin/test-shares`
- [x] **Revoke on userdel**: clears durable shares naming the deleted
      actor or its removed objects (open-cap refuse already landed)
- [x] **Path canonicalization**: reject `.` / `..`; overlong components,
      max depth, embedded NUL, non-ASCII, empty/double separators;
      byte names only (`bin/test-paths`; documented in `GALFS.md`)
- [x] Sharing e2e (durable): `bin/test-share-disk` + `boot_with_galfs`
      records a share, reboots, `apply_shares` reinstalls the card; host
      fsck sees a used share slot (live grant/logout still covered by
      `test-galfs` / session clear)
- [x] **Confused-deputy tests**: LIST-only cannot `share` WRITE; cannot
      share a path without a covering card; missing path `NotFound`
      (`bin/test-cards`; same `resolve_and_check` bar as `grant`)
- [x] **Token / share slot exhaustion**: full table → `NoResource`;
      revoke/unshare frees a slot; maxima documented (`TOKEN_SLOTS`=8,
      `SHARE_SLOTS`=32); `bin/test-cards`

## Milestone 46 — Storage stack

Reviewers will ask how storage grows past QEMU's secondary IDE.

- [x] **Device abstraction**: `BlockDevice` trait (read/write sectors,
      flush, capacity) with ATA PIO `PrimarySlave` as the first impl;
      galfs uses only `disk()` → trait (no raw `ata::` I/O)
- [x] **Primary IDE / virtio-blk**: `drivers/virtio_blk` (legacy PCI) +
      `boot_with_galfs_virtio`; `cargo run` defaults to virtio-blk-pci
      (`GALEXY_GALFS_IDE=1` keeps the IDE slave); galfs prefers virtio
      when present
- [x] **Identify / capacity**: IDENTIFY words 60–61 / 100–103 →
      `capacity_sectors()`; galfs gates on ≥ dual-slot size; out-of-range
      LBA → `BadValue`; `disk_capacity_sectors` + `bin/test-galfs-disk`
- [x] **Flush discipline**: FLUSH CACHE on every slot commit; runner
      `boot_with_galfs_cache` + `galfs_disk_persists_writeback_cache` /
      `_none_cache` (also `writethrough` default)
- [x] **Optional**: simple partition offset — `set_disk_lba_base` /
      `DISK_PART_LBA` (2048); `bin/test-galfs-part` +
      `galfs_disk_persists_partition_offset`
- [x] **Write barriers**: whole sealed slot (data + metadata) then
      flush then publish gen — documented in `GALFS.md`
- [x] **Hot-unplug / missing disk**: boot without slave stays RAM-only;
      too-small disk logged + RAM-only; no panic
- [x] Docs: how `cargo run` attaches storage (`README` + `GALFS.md`)
- [x] Disk backend matrix in runner tests: IDE (writethrough / writeback /
      none) + virtio-blk-pci persistence e2e

## Milestone 47 — Process, ABI & capabilities

What a non-toy program and a forged Cap will hit. This milestone lays the
**clean-slate process foundation**: tasks are kernel objects addressed by
**process Caps** (wait / kill / transfer), not global PIDs. Userspace
**init** and seat supervision are Phase 6 (Milestones 53–55).
Plan: `docs/PROCESS.md`. Style: `docs/STYLE.md` → Process model and init.

### Process identity (Cap foundation)

Today: names + slots. Target: **spawn returns a Cap to the child** —
same discipline as files. Optional debug ids for listings only.

- [x] **Design note** in `DESIGN.md`: why not PIDs as ABI (guessable
      global namespace); spawn-not-fork; process Cap rights; debug id vs
      Cap; parent + Cap transfer on orphaning; zombie until Cap-wait
- [x] **Process Cap rights** in `galexy-abi`: `PROC_WAIT` /
      `PROC_KILL` / `PROC_TRANSFER` / `PROC_INSPECT` / `PROC_PARENT`
      (bits 6–9); attenuation same intersection rule as file Caps
      (`DESIGN.md` + abi docs)
- [x] **`spawn` returns a child Cap** to the caller (`galexy-rt` + shell);
      without the Cap you cannot wait or kill that task
- [x] **Self Cap**: `SELF_INDEX` + `PROC_INSPECT`; `read(self)` →
      `id=… name=… state=…` (`test-selfcap`); no `getpid`
- [x] **Debug id (optional)**: monotonic KOID-style number for `tasks` /
      serial only; **no** `open_process(debug_id)` syscall
- [x] **Parent pointer** on every task; until init exists, kernel-spawned
      roots use parent “kernel”; Milestone 53 makes init the orphan root
- [x] **Exit status**: Cap-wait delivers the child’s exit code; shell
      `echo $?` prints last Cap-wait status
- [x] **Wait by Cap**: `wait(cap)` + shell Cap-wait via `SPAWN_INHERIT`;
      `SPAWN_WAIT` remains a convenience (Cap installed, park until exit);
      **no wait-by-name or wait-by-pid ABI** (STYLE)
- [ ] **Wait/reap hygiene**: waiter exits first → child’s wait Cap
      transfers to the new parent (eventually init); no dangling wait
      edges; zombies until Cap-wait or Cap drop (orphan transfer open)
- [x] **Kill by Cap (signals-lite)**: stop a runaway only if you hold
      `PROC_KILL` on that Cap — not a name or number (`test-proccap`)
- [x] **Names as labels**: `tasks` / `threads` show debug id + name +
      state (or ticks); unique-live name may remain for UX but is not
      the wait key
- [ ] **Name length / charset** for labels aligned with spawn checks;
      documented in abi
- [ ] **Args & env**: spawn already passes one argument blob — define
      argv/env layout (or explicitly freeze “single arg blob” in ABI)
- [ ] **Ring-3 segment reload**: SYSCALL return restores user DS/ES (and
      documents FS/GS policy); today kernel bootstrap selectors remain
- [ ] **More query caps or `sysinfo`**: uptime, free frames, galfs
      usage, task list (debug ids only) — for review demos
- [ ] ABI doc section: stable vs experimental (process Cap wait/kill
      marked experimental until Phase 6 freezes)
- [x] Tests: spawn → Cap-wait exit code; forging a Cap word fails;
      kill + Cap-wait status 137 (`test-proccap`); stale Cap after
      reap → BadCap (gen bump)

### Capability & resource accounting

Make the object-capability story hold under exhaustion and forgery.

- [ ] **Cap forge battery**: reserved indices without grants →
      AccessDenied; stale caps after close/remove → BadCap; stripped
      rights bits cannot be re-added by the user — **include process
      Caps** in the same battery
- [ ] **Per-task budgets**: document max opens, pipes, tokens, arg bytes,
      process Caps held; hit each ceiling in tests
- [ ] **Frame/charge limits (soft)**: optional max frames per user task;
      spawn fails cleanly when the machine is low on memory
- [ ] **Give/pipe lifecycle**: all ends closed; no kernel pipe slab leak
      across N create/give/exit cycles; process Cap `give`/`TRANSFER`
      covered
- [ ] **Query caps**: snapshots do not allocate on IF=0 (already true —
      add a regression comment/test if a change regresses it)
- [ ] **Loader EXEC**: only the shell (or tasks with loader grant) can
      spawn; pre-login denial covered in Milestone 43 — cross-link tests here

## Milestone 48 — Memory, safety & concurrency

Paging policy, W^X, scrub, and a frozen lock story.

### Memory, paging, and SMP review items

Close or formally waive the known memory-model nits.

- [ ] **PCID / GLOBAL kernel pages**: design note; implement if churn
      measurements warrant, else waive with numbers from the suite
- [ ] **Demand paging policy**: page faults for user heap/stack growth
      vs today's fixed maps — either a small MVP or a written non-goal
- [ ] **Heap policy**: initial size, growth cap, OOM behavior visible to
      userland (kill vs error)
- [ ] **FSGSBASE fallback**: document `-cpu` requirements; panic message
      names the feature; optional soft path remains out of scope but
      stated
- [ ] **BSP-only devices**: reaffirm keyboard/framebuffer single-consumer
      in DESIGN; add a one-page “Concurrency model for reviewers”
- [ ] **Guard / canary audit**: confirm user stack guard + kstack canary
      still fire in dedicated tests
- [ ] **Frame accounting**: free frames at boot vs after N spawn/exit
      cycles stays stable (existing churn tests + a documented budget)
- [ ] **User map W^X**: code pages RX, stack/scratch RW never X; test
      that jumping to stack faults
- [ ] **ASLR-lite (optional)**: randomize user P4 pick among free
      entries, or waive with rationale in THREAT.md

### Memory safety hardening pass

Push the easy wins a systems engineer will check in the first hour.

- [ ] **Stack wipe on reap**: kstack / user scratch pages zeroed before
      reuse (or documented skip with threat note)
- [ ] **Password / key scrub** audit across login, passwd, unlock
      (cross-check Milestones 43/44)
- [ ] **NX / W^X audit** (cross-check paging items above): ELF loader rejects
      writable+executable segments or maps them safely
- [ ] **User pointer TOCTOU**: copy path/password into kernel buffers
      before parse/verify (document if already true; fix if not)
- [ ] **Integer / length checks**: every `len` from userland checked
      against ABI max before slice construction
- [ ] **Panic on debug assertions** in test builds for canary / table
      invariants; soft handling in release where appropriate

### Lock order, IRQ gates, and init/shutdown

Freeze concurrency rules so review does not invent races.

- [ ] **Lock-order table** in DESIGN (complete list: THREADS, galfs TABLE,
      screen, keyboard ring, ATA, …) with allowed nestings
- [ ] **IRQ-gate audit**: every public API that takes a preemptable lock
      is IRQ-gated; grep/doc checklist
- [ ] **Init order** documented: mm → arch/ACPI → sched → galfs → shells
- [ ] **Shutdown order**: flush galfs, drop volume key, power
- [ ] **Steal/reap invariants** restated with a small diagram or bullet
      proof; `test-smpstress` remains the hammer
- [ ] **No lock in shootdown handler** reaffirmed; new IPI handlers follow
      the same rule

## Milestone 49 — Console, audit & operator UX

Human path plus the evidence trail.

### Console, keyboard, and TTY polish

Human-facing paths reviewers will exercise for an hour.

- [ ] **Blinking text cursor** on the active TTY
- [ ] **Keyboard overflow**: count drops; optional serial warning;
      document bound
- [ ] **CSI / control policy**: list supported sequences; reject set
      stays intentional
- [ ] **Per-TTY scrollback** bound documented (cell grid size)
- [ ] **Password star-prompt** integration with Milestone 43 secret prompts
- [ ] **Ctrl-C / Ctrl-D** semantics documented (line cancel vs EOF)
- [ ] **UTF-8**: console remains byte/ASCII-centric for review; document
      non-goal for full Unicode editing
- [ ] Manual script: “reviewer demo” — create user, grant card, dual TTY

### Audit, logging, and diagnostics

If it is not logged, it did not happen in a review.

- [ ] **Auth audit lines** (serial): login ok/fail, logout, passwd,
      useradd/userdel, su, lockout — no password material
- [ ] **Token audit**: grant/revoke with actor, path, rights, target task
- [ ] **Panic / fault reports**: keep task-kill path; ensure serial
      breadcrumbs include task name + rip
- [ ] **`dmesg`-lite**: ring buffer of recent kernel lines readable from
      a query cap or `dmesg` util (even if it only mirrors serial)
- [ ] **Debug vs release**: feature flags for verbose sched steal/reap
      logs
- [ ] **Rate-limit audit spam**: repeated failed logins do not fill the
      dmesg ring to the exclusion of faults
- [ ] **No secrets in audits**: red-team the format strings; add a grep
      CI check for `password` in serial helpers if practical

## Milestone 50 — Shell for real demos

Shell is the demo UI; make it less surprising under load.

- [ ] **Pipeline wiring**: `echo hi | cat` via existing pipe/give syscalls
- [ ] **Background / foreground**: optional; at least document that all
      bare launches share the console
- [ ] **Glob** (userspace): `*` expansion against the files snapshot
- [ ] **Line editing**: history (even one previous line), Ctrl-C
      semantics (kill foreground child if any)
- [ ] **Prompt / cwd correctness** across login, logout, su, failed cd
- [ ] Typing e2e for pipeline + glob smoke

## Milestone 51 — Docs, tests, CI & soak

Paperwork, evidence, tooling, and scope freeze.

### Threat model & documentation pack

Paperwork a systems engineer expects before reading code.

- [ ] **`docs/THREAT.md`**: assets (disk, console, ramdisk ELFs, caps);
      adversaries (stolen disk, malicious user program, malicious second
      seat); trusts (physical F1, ramdisk publisher); non-goals
- [ ] **`docs/FS.md`**: galfs layout, versions, sync, recovery, quotas,
      path grammar — split out of the long DESIGN syscall essay
- [ ] **`AUTH.md` refresh**: end-state after Milestones 43–44
- [ ] **Reviewer README section**: how to build, run BIOS/UEFI, attach
      disk, default accounts, where logs go
- [ ] **ABI stability table**: syscall numbers + struct layouts marked
      stable / unstable
- [ ] **Known limitations** pruned: every remaining bullet either has a
      milestone id or an explicit waive

### Hardening tests & supply chain

Evidence, not assertions.

- [ ] **Negative suite**: path fuzz (host); grant/revoke confused-deputy
      cases; pre-login spawn denied; bare spawn cannot write galfs
- [ ] **Ramdisk measurement**: hash of the packed tar at build time;
      kernel checks optional allowlist before `SPAWN_WAIT` inherit
      (or document “trusted ramdisk” as a hard requirement)
- [ ] **Feature-gated test seams**: `crash`, verbose panics, etc.
- [ ] **CI matrix doc**: BIOS, UEFI (`OVMF_FD`), `-smp 2`, with/without
      galfs disk, cache modes
- [ ] **Coverage list**: which milestones each `bin/test-*` guards
- [ ] **Host fuzz**: `parse_path` / component_ok under cargo-fuzz or a
      small exhaustive generator in galexy-core tests
- [ ] **Property tests**: grant∩ancestor closure; revoke exact-object;
      dual-slot generation monotonicity

### Build, CI, reproducibility, and tooling

Make “green on my machine” into “green in CI and for the reviewer”.

- [ ] **Pinned toolchain** already — document `rustc -V` in README
      reviewer section
- [ ] **One-command review boot**: `cargo run` + disk + `OVMF_FD` notes;
      script `scripts/review-smoke.sh` runs a focused subset
- [ ] **CI matrix** (doc + workflow if GH Actions exists, else runner
      instructions): BIOS, UEFI, smp2, disk on/off
- [ ] **Clippy -D warnings** + fmt check in CI
- [ ] **Host tests** for abi/core on every PR
- [ ] **Repro notes**: ramdisk tar hash printed at build; image names
      stable
- [ ] **PR template**: test plan + STYLE secrets/GALF checklist
- [ ] **`galfs.img` gitignore** verified; clean instructions if a bad
      image breaks boots after a version bump

### Performance budgets & soak

Solid means it does not fall over when exercised.

- [ ] **Budgets doc**: target syscall latency class (order-of-magnitude),
      console budget (already 512 B/tick), max tasks, max galfs mutate/s
      on QEMU
- [ ] **Soak**: N-minute idle + periodic spawn/exit + galfs touch under
      QEMU; no leak in free frames / pipe slots / thread slots
- [ ] **Steal fairness**: under load both CPUs do useful work
      (`test-smpstress` metrics or serial counters)
- [ ] **Pathological input**: huge paste on password prompt; tight
      write loop on console (budget); deep path components
- [ ] **Explicit non-goal**: desktop-class throughput — state it

### Scope freeze & explicit non-goals

What we will tell a reviewer we are *not* doing — written down.

- [ ] **No network stack** for review-rc1
- [ ] **No GPU / multi-framebuffer**
- [ ] **No POSIX compatibility claim** — galexy ABI only (process Caps,
      not a Linux PID/`waitpid` promise)
- [ ] **No ambient process namespace** — no kill/wait/open by guessed
      global integer id
- [ ] **No MFA / networked IdP / PAM**
- [ ] **No demand-paged swap**
- [ ] **No multiprocessor device drivers** (keyboard/FB stay BSP)
- [ ] **No secure boot / measured boot** (ramdisk hash optional in hardening below)
- [ ] **No systemd/dbus** — Phase 6 init is a small supervised table
- [ ] Each non-goal listed in `THREAT.md` with one-line rationale
- [ ] ROADMAP Phase 5 updated to listed here under Milestone 45

## Milestone 52 — Review release candidate

The “ready for review” checklist — not a feature dump.

- [ ] All milestones 43–51 either ✅ or explicitly waived in THREAT/FS
      docs with rationale
- [ ] Full suite green BIOS+UEFI; disk persist + corrupt recover + auth
      e2e + galfs capacity smoke
- [ ] Default build: no `crash`, KDF live, encryption on if disk present
- [ ] Fresh format walkthrough in README (login on every seat; grant demo)
- [ ] Tag `review-rc1` (or note in ROADMAP) with a short changelog
- [ ] Freeze window: ABI changes require DESIGN + abi crate bump in the
      same PR
- [ ] **STYLE.md audit**: PR checklist that secrets/GALF/IF=0 rules were
      followed (short bullet list in the PR template or REVIEWER.md)
- [ ] Phase 6 process/init milestones listed in ROADMAP (not required to
      tag `review-rc1`, but design notes from M47 must not contradict them)

---

## Phase 6 — Process model, init & supervised seats

Goal: a **modern, clean-slate** capability process architecture —
process Caps, real hierarchy via Cap transfer, userspace init as orphan
root, seats/services supervised in userspace — without a Unix PID ABI or
POSIX claim. **Plan: `docs/PROCESS.md`.** Builds on Milestone 47. Style:
`docs/STYLE.md` → Process model and init. Direction: `docs/ROADMAP.md`
Phase 6.

## Milestone 53 — Init (orphan root)

Mechanism in the kernel; policy in userspace.

### Kernel mechanism

- [ ] **Init is the first ring-3 task**: kernel loads `init` (ramdisk)
      once; it is the root of the user process tree (role flag / reserved
      slot — not an ABI “PID 1”)
- [ ] **Orphan Cap transfer**: when a parent exits, wait/control Caps for
      live children move to init; zombies are Cap-waitable by init (or
      whoever still holds a wait Cap)
- [ ] **Init is immortal to user kill**: kill Cap on init is not issued
      to others (or always AccessDenied); if init exits/faults → kernel
      panic (or controlled reboot) with a clear serial reason — never
      silently `ensure_shell` around it
- [ ] **Retire kernel seat supervisor**: `ensure_shell()` / auto-respawn
      of `shell`…`shell12` becomes a transitional shim, then **removed**
      once init owns seats (Milestone 54). Document the cutover in DESIGN
- [ ] **Shutdown/reboot path**: power syscalls either require a right
      held by init (or a grant init gives the operator shell), or become
      “request to init” so flush/order happens in userspace first
- [ ] Tests: orphan Cap transfer; kill-init denied; init exit
      panics/reboots deterministically in a test kernel

### Userspace init program

- [ ] **`crates/userspace/init`**: minimal orphan root — Cap-wait/reap
      loop, start configured children, handle shutdown request
- [ ] **Config surface (v1)**: fixed table in init or a small `/etc/init`
      galfs file — which programs to spawn at boot (getty/seats, optional
      services). No dbus/systemd graph in v1
- [ ] **Restart policy (v1)**: on child exit, restart | once | ignore —
      per entry; crash loops back off with monotonic time (M43 clock);
      init keeps the child’s Cap to supervise
- [ ] **Caps/tokens for children**: init attenuates what each child gets
      (login seat ≠ disk service); never ambient “all rights because
      parent is init”
- [ ] **Logging**: init writes a short serial/console line on
      start/reap/restart/shutdown (debug ids OK in logs; Caps stay private)
- [ ] Docs: `DESIGN.md` boot → init → seats diagram; AUTH notes that
      login seats are init children

## Milestone 54 — Seats & service supervision

Move F-key consoles and long-runners under init.

### Login seats (getty → shell)

- [ ] **Per-TTY seat child**: init spawns one seat program per console
      (or one getty that execs/spawns the shell) with the TTY arg used
      today for the login banner; **init retains a supervise Cap**
- [ ] **Login screen stays in the seat**: logged-out UI remains the
      shell (or a thin getty); init does not embed password prompts
- [ ] **Seat crash → restart**: replacing today's kernel `ensure_shell`
      with init's restart policy; supervisor typing e2e updated
- [ ] **Session id**: bind Milestone 43 session generation to the seat
      Cap / debug id so audits name a stable process identity
- [ ] **F1–F12 switching** remains kernel console selection; only the
      *task lifecycle* moves to init
- [ ] Tests: Cap-kill seat → login screen returns; other seats unaffected

### Service supervision (lite)

Small and explicit — not a systemd clone.

- [ ] **Service table**: name, program, restart policy, required caps /
      token skeleton, optional dependency “after:” (ordering only in v1)
- [ ] **Operator surface**: `svc status|start|stop|restart <name>` (shell
      builtins or a tiny util) talking to init via a documented IPC
      (pipe/galfs control file/syscall — pick one in DESIGN, keep it
      capability-gated). Operators do **not** get raw process Caps to
      every service unless init deliberately grants them
- [ ] **No ambient root services**: each service runs as an actor or
      with a dedicated card set; document the trust boundary
- [ ] **Hang detection (optional)**: liveness pipe or deadline; mark
      waived if not in v1
- [ ] Docs: which boot services exist for `review-rc1` vs Phase 6 demos
- [ ] Explicit non-goals for v1: socket activation, cgroups, timers,
      device manager, user bus

## Milestone 55 — Sessions & job control (lite)

Enough structure for demos and Ctrl-C — still not POSIX.

### Sessions and job Caps

- [ ] **Session** = login seat (or service root); **job** = pipeline
      under that session — both addressed by **Caps**, not pgids as
      ambient integers
- [ ] **Job Cap rights**: spawn can create/join a job; shell holds the
      job Cap for the foreground pipeline
- [ ] **TTY foreground job**: keyboard-generated interrupt (Ctrl-C)
      delivers signals-lite to the foreground **job Cap** only
- [ ] **Shell jobs (v1)**: one foreground pipeline; background optional
      or waived with DESIGN note
- [ ] **Wait on job Cap**: shell waits for its pipeline without reaping
      unrelated cousins
- [ ] Tests: Ctrl-C kills foreground `linger`, not a sibling seat; wait
      collects pipeline exit status
- [ ] ABI freeze note: process/job Caps + wait + kill marked stable or
      explicitly experimental in the Milestone 51 table
- [ ] Explicit non-goals: full job-control tty ioctls, POSIX job specs,
      `SIGTSTP`/`SIGCONT` zoo, kill-by-pgid ambient namespace

---

## Known limitations / follow-ups

Open bullets below are tracked by milestone id where planned. Waived
items stay here with rationale.

- [x] ~~UEFI: timer + keyboard dead under UEFI~~ — CLOSED by Milestone 17
      (APIC family: LAPIC timer + I/O APIC keyboard route on every boot
      path; UEFI liveness + typed E2E asserted). The door to SMP is open:
      MADT CPU records are already parsed
- [x] ~~SMP~~ — CLOSED by Milestone 18 (two CPUs running the full kernel;
      pinned-at-spawn rotation, owner-reaping, per-CPU timers/E2E under
      the existing suite). The SMP-era follow-ups:
      - [x] ~~TLB shootdown IPIs~~ — CLOSED by Milestone 19 (vector 0xF8,
            lock-free handler, mailbox pool; heap growth broadcasts for
            real). Demand paging, if it remaps kernel-half pages, must
            use the same `shootdown_others` path
      - [x] ~~Load balancing~~ — CLOSED by Milestone 19 (idle-pass work
            stealing: owner flip under THREADS, entry deferred one tick,
            stolen_at cooldown). Pin-at-spawn is still the initial
            placement; only idle CPUs migrate
      - [ ] `-cpu max` asserts FSGSBASE on real hardware too; a fallback
            would be needed on pre-FSGSBASE CPUs — **waive for review**
            unless real hardware without FSGSBASE is a goal (Milestone 48)
      - [ ] Everything still single-consumer by DESIGN stays that way:
            cooperative tasks + shell + framebuffer + keyboard all live
            on the BSP (Milestone 48 concurrency note)
- [ ] SYSCALL leaves DS/ES/FS/GS as kernel bootstrap selectors when the
      task resumes in ring 3 — **Milestone 47**
- [ ] `write` still rejects controls outside the console subset
      (printable ASCII, space, newline, backspace, tab, form feed, CR,
      ESC). Blinking cursor + CSI policy — **Milestone 49**
- [x] ~~There is no glob or disk-backed store yet~~ — disk-backed store
      CLOSED by Milestone 38; glob — **Milestone 50**
- [x] ~~`run` leaks the task name, and freed slots panic the table at
      64~~ — CLOSED by Milestone 26 (name copied into the slot; a
      `Freed` record is reused once no CPU is current on it)
- [x] ~~Status bar can overwrite the typing line when the screen is full~~
      — CLOSED by Milestone 24 (text scrolls above the status row)
- [ ] Framebuffer is used as the bootloader mapped it (deliberate — BootInfo
      exposes no physical framebuffer address) — **waive** unless isolation
      work needs a physical FB remap
- [ ] Screen: text-mode cursor (blinking) — **Milestone 49**
- [ ] Keyboard queue overflow silently drops keys — **Milestone 49**
- [ ] Cooperative-scheduler nits: `run()` sweep fairness mid-sweep;
      TaskCtx's 8 fixed u64 slots — **waive for review** (document in
      Milestone 48 concurrency note) unless a bug shows up
- [ ] TLB efficiency: every CR3 swap is a full flush (no PCID/GLOBAL
      kernel pages) — **Milestone 48** (implement or waive with numbers)
- [ ] Auth hardening (crypto, prompts, sessions, least privilege) —
      **Milestone 43**
- [ ] Sealed GALF — **Milestone 44**
- [ ] galfs for real usage — **Milestone 45**
- [x] Storage stack — **Milestone 46**
- [ ] Process/ABI/caps (process-Cap foundation) — **Milestone 47**
- [ ] Memory/safety/concurrency — **Milestone 48**
- [ ] Console/audit — **Milestone 49**
- [ ] Shell demos — **Milestone 50**
- [ ] Docs/tests/CI/soak — **Milestone 51**
- [ ] Review RC — **Milestone 52**
- [ ] Init (orphan root) — **Milestone 53** (Phase 6)
- [ ] Seats & service supervision — **Milestone 54**
- [ ] Sessions & job Caps lite — **Milestone 55**
