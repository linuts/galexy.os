# Changelog

One line per merged milestone PR, newest first, starting at #62
(the first PR of the Phase 5 review push). Earlier history is the git
log and `TODO.md` Milestones 1–42.

Format: `#PR — Milestone — what landed`. PR numbers link to GitHub.

## Unreleased

- [#89](https://github.com/linuts/galexy.os/pull/89) — scheduler and shell
  split, nano help, console escape cut. The
  preemptive scheduler moves out of `sched/mod.rs` into `thread`
  (table, reap, pin), `spawn` (queued spawn, shells, wait/kill,
  sessions), `task` (file, pipe, and channel syscalls), and `iowait`
  (timer, parked I/O, console budget). The shell moves out of
  `main.rs` into `state`, `edit`, `builtins`, and `jobs`. `nano`
  Ctrl-G opens a help page of keys that are not already on the
  shortcut bar; the next key closes it and is not inserted. A console
  write commits only a prefix that ends outside ESC/CSI, so the
  512-byte budget cannot tear a cursor sequence on the shared COM1
  line and leave the host terminal looking frozen. The shell yields
  when a keyboard read returns nothing. Suite: 114 boots.
- [#88](https://github.com/linuts/galexy.os/pull/88) — stability and
  security review — fixes
  [#86](https://github.com/linuts/galexy.os/issues/86) (`passwd` hung a
  sealed-disk boot): every kernel spin lock is `sync::Mutex`, whose
  relax step services TLB-shootdown acks so an IF=0 lock waiter cannot
  stall a heap growth on the other CPU; the shootdown initiator
  services other CPUs while it waits; the heap's second grower polls
  instead of re-enabling IF; galfs `sync_to_disk`, `wipe_volume_key`,
  and `seal_and_lock` run IF=0 from the BSP main loop; PBKDF2 runs
  before the `TABLE` lock; `thread_stats`, pipe wake, and orphan
  transfer no longer allocate under `THREADS`. `svc
  start|stop|restart` are admin-only through init (`svc status` stays
  open). The panic handler force-unlocks serial and `dmesg` before it
  prints. virtio-blk DMA follows the buffer's physical pages (one
  descriptor per contiguous run) instead of `translate(buf) + len`,
  which had let a sector straddling a 2 MiB boundary DMA into the
  page table the bootloader placed between two `.bss` frames; galfs
  `DISK_BUF` is page-aligned; requests batch 32 sectors. New
  `test-lockgrow` and `test-dmasplit` kernels and
  `passwd_on_sealed_disk_typing_e2e` (production image, sealed disk,
  passphrase → login → `passwd` → `sync` → reboot → login).
  `THREAT.md` gains a review log. Suite: 114 boots. The `v1.0` tag is
  still a separate gate
- [#87](https://github.com/linuts/galexy.os/pull/87) — M67 — init owns
  shutdown and the service table. `shutdown` / `reboot` ask init over
  one channel; init Cap-kills the other seats, calls `sync`, then
  `Power`. Logged-in seats do not hold Power while init is alive.
  Services are `restart | once | ignore` with backoff (0, then 250,
  500, 1000, cap 2000 ms). `svc status|start|stop|restart` never hands
  out a service Cap. Audit lines carry `id=<debug id>`. `Map`, `Clock`,
  `Channel`, `Send`, `Recv`, `wait`, and `kill` are stable. `spawn`
  stays experimental. No new syscall. `MAX_PROC_CAPS` stays 16. The
  `v1.0` tag is still a separate gate
- [#85](https://github.com/linuts/galexy.os/pull/85) — M66 — per-task
  `Map` heap (32 pages) with a `galexy-rt` bump allocator; `Clock`;
  capability channels (`Channel` / `Send` / `Recv`); NUL-separated argv
  in the 256-byte spawn blob; shell pipelines, `jobs` / `fg`, tab
  completion, `history`, and Shift+PgUp scrollback; `head`, `tail`,
  `wc`, `grep`, `uptime`, and `ls -l`. Syscall spawn keeps the child
  parked until the parent is linked, so `SPAWN_WAIT` cannot miss a
  fast exit on the other CPU. The thread table grows without holding
  its lock across a TLB shootdown. `Map`, `Clock`, `Channel`, `Send`, and
  `Recv` stay experimental until Milestone 67. `Sleep` is unchanged
- [#84](https://github.com/linuts/galexy.os/pull/84) — M65 — `-M q35`
  by default; PCIe ECAM when ACPI publishes `MCFG`; virtio-blk 1.x with
  MSI-X (legacy I/O BAR logs `legacy IO BAR`); virtio-input keyboard
  with a PS/2 fallback; HPET calibration cross-checked against CPUID
  0x15/0x16; TSC-deadline and x2APIC when CPUID reports them; 8259
  remap skipped when the FADT says the pair is absent. The PC speaker
  stays the only audio path
- [#83](https://github.com/linuts/galexy.os/pull/83) — M64 — release
  images (`opt-level = 3`, fat LTO, debug assertions on); KVM when
  `/dev/kvm` is writable; `bin/test-bench` and `docs/PERF.md`; virtio-blk
  completes on INTx (`IO_BLOCK`) instead of a 10 M-spin; `SPAWN_WITH_CAPS`
  (r10 bit 4; bit 3 is the keyboard grant) moves pipe ends before the
  child runs; `show_tty` repaints changed rows. PCID and a new heap
  allocator stay waived
- `nano` — ring-3 screen editor (`nano <path>`): arrows, Ctrl-O save,
  Ctrl-X exit. The shell passes `SPAWN_GRANT_KEYBOARD` (spawn `r10` bit 3)
  and Cap-waits so the editor can read the seat's keys
- [#81](https://github.com/linuts/galexy.os/pull/81) — M63 — SMEP, SMAP,
  and UMIP when the CPU reports them; user copies go through
  `arch::user_copy`; kernel KASLR stays inside P4 indexes 1..=24 (BIOS
  stage 4 switches to a 64 KiB stack before the ASLR RNG); ELF load
  failures return `BadValue`; GALF v12 stores each actor's KDF
  iteration count (100_000 on a release build under KVM, else 10_000;
  old images are refused); a 2 s rotation watchdog dumps once, and a
  wake or a new thread pokes an idle CPU with IPI `0xF7`
- [#79](https://github.com/linuts/galexy.os/pull/79) — `cargo run` is
  headless by default (`-nographic`); COM1 receive (IRQ4) feeds the
  keyboard queue so the terminal is the console. `--display` still opens
  the framebuffer window
- [#78](https://github.com/linuts/galexy.os/pull/78) — M51/M52 — soak and
  fairness evidence: `bin/test-soak` (ten pipe/file/spawn rounds with
  frame, pipe, thread, and galfs-block leak checks), `bin/test-fairness`
  (two-CPU steal fairness), `bin/test-pathological` (console budget
  under a flood of writes), password-paste and no-`crash`-seam shell
  e2e; `pipe::in_use()`; Milestone 52 checklist closed except the
  owner-deferred `review-rc1` tag; intermittent SMP hang tracked for
  Milestone 63
- [#77](https://github.com/linuts/galexy.os/pull/77) — M51 — reviewer
  docs pack: `docs/THREAT.md`, `docs/ABI.md`, `docs/PERF.md`,
  `LICENSE` (MIT), `SECURITY.md`, this changelog, a PR template,
  `scripts/review-smoke.sh`; DESIGN test-seam and coverage tables; AUTH
  and GALFS status refresh; README "For reviewers"
- [#76](https://github.com/linuts/galexy.os/pull/76) — M51 — hardening
  evidence: `loader::validate_elf` + `bin/test-badelf` (hostile ELF
  oracle), `bin/test-negative` (pre-login spawn denied, bare spawn cannot
  touch galfs), `galexy_core::parse_path` with an exhaustive host sweep,
  `galexy_galf::cards` property tests (grant∩ancestor closure, revoke
  exact-object, dual-slot generation monotonicity), ramdisk SHA-256 at
  build and re-measured at boot
- [#75](https://github.com/linuts/galexy.os/pull/75) — M51 — GitHub
  Actions `ci.yml`: `host` job (fmt, host suites, three clippy
  invocations) and `qemu` job (image build, `audit_strings`, full boot
  suite under TCG, serial logs on failure); README "CI" section maps
  each matrix axis to named boot tests
- [#73](https://github.com/linuts/galexy.os/pull/73) — M51 — toolchain
  hygiene: clippy green with `-D warnings` on kernel, host, userspace,
  and runner; one formatting-only rustfmt baseline; nightly pinned to a
  date in `rust-toolchain.toml`
- [#72](https://github.com/linuts/galexy.os/pull/72) — M69 — LINKER.md
  wording: `gxld::validate`, hand-emitted output, stale `gxc::elf`
  references dropped
- [#71](https://github.com/linuts/galexy.os/pull/71) — M69 — `gxld`:
  `no_std` static ELF64 linker library + GNU-ld-compatible CLI;
  `rustc -Clinker=gxld` links every userspace program; the
  `galexy-os-gxld` image passes the same e2e as the `rust-lld` one;
  `gxc` emits a relocatable object and links through `gxld`
- [#70](https://github.com/linuts/galexy.os/pull/70) — docs — `gxc`
  frozen as a fixture; `gxld` planned as Milestone 69; Phase 11
  renumbered
- [#69](https://github.com/linuts/galexy.os/pull/69) — docs — project
  review ("Where we stand" table), Phase 9 Milestones 63–67, Phase 11
  rustc-on-Galexy plan
- [#68](https://github.com/linuts/galexy.os/pull/68) — M50 — shell for
  real demos: `echo hi | cat` pipeline via `pipe` + `give`, one `*`
  glob per word, line editing (arrows, Ctrl-A/E/U), cwd kept across
  login / `su` / failed `cd`
- [#67](https://github.com/linuts/galexy.os/pull/67) — M49 — console,
  audit, and operator UX: blinking cursor, keyboard overflow drop
  counter, auth/grant audit lines without secrets, `dmesg` capability,
  `verbose-sched` steal trace
- [#66](https://github.com/linuts/galexy.os/pull/66) — M48 — memory,
  safety, and concurrency: W^X user maps, stack and secret wipe, fixed
  user maps (demand paging waived), heap policy, FSGSBASE required,
  lock-order table
- [#65](https://github.com/linuts/galexy.os/pull/65) — M45 — galfs crash
  injection (`test-crash`, consistent slot after a torn commit) and ATA
  I/O error propagation (`Unsupported`, never a panic)
- [#64](https://github.com/linuts/galexy.os/pull/64) — M44 — sealed GALF
  finished: interactive volume unlock (`USER_UNLOCK`), key wipe on last
  logout and on power off; wrong passphrase stays RAM-only
- [#63](https://github.com/linuts/galexy.os/pull/63) — M43 — auth
  hardening: PBKDF2-HMAC-SHA256 + CSPRNG salts, no-echo prompts, idle
  logout, kernel-enforced must-change for the default admin password,
  session generation, narrow admin bypass, spawn rights mask
- [#62](https://github.com/linuts/galexy.os/pull/62) — M43 — login
  lockout: five misses lock the actor and the TTY for a cool-down;
  `SysError::Locked` does not check the password
