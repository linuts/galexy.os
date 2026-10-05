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

## Known limitations / follow-ups

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
            (e.g. per-CPU paging via a shared struct + disables) would be
            needed on pre-FSGSBASE CPUs — out of scope deliberately
      - [ ] Everything still single-consumer by DESIGN stays that way:
            cooperative tasks + shell + framebuffer + keyboard all live
            on the BSP (one display, one input queue — documented in
            DESIGN "Concurrency model")
- [ ] SYSCALL leaves DS/ES/FS/GS as kernel bootstrap selectors when the
      task resumes in ring 3 — user code must not do segment-based
      addressing; proper user segment reload is future segment work
- [ ] `write` printable-ASCII rule is a stand-in for a real console
      charset policy (newlines work; tab/CR/ESC are future screen work)
- [ ] `run` leaks the task name (`Box::leak`, a few bytes per spawn) —
      fine at this scale; a slot free-list is the fix if tasks churn
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
