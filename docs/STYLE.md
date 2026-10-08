# STYLE — galexy.os conventions

Rules for how code in this repo is written. Reviewers (and future me) should
enforce these. Auth/token, galfs, and process/init rules below are binding
once the matching milestones land (43+ / 47 / 53–55); until then treat
them as the target.

## Rust idioms

- **`#![no_std]` always** in kernel crates. `alloc` only when a module
  explicitly opts in with its own gating (e.g. `#![feature(alloc)]` in one
  place, reviewed).
- **`#![deny(missing_docs)]` on public kernel modules.** Every public item
  gets a doc comment explaining *hardware behavior*, not just signature.
- **No comments unless asked for in review** — doc comments (`///`) are fine
  and expected; inline `//` only for genuinely non-obvious reasoning.
- Error types: implement `Debug` + a short `Display`; never `.unwrap()` in
  code that can fail at runtime — return `Result` up to `main`, panic only in
  genuinely unrecoverable states.
- Unsafe: every `unsafe` block gets a `// SAFETY:` comment stating the exact
  invariant being relied upon. No bare `unsafe`.
- **No new `static mut`.** Prefer `spin::Mutex`, atomics, or per-CPU GS.
  Stack buffers beat static scratch for syscall staging (passwords, paths).

## Module organization

- One module per directory (`arch/`, `drivers/`, ...), `mod.rs` re-exports
  the public surface only.
- **Layer boundaries are law** (see `docs/DESIGN.md`): `main.rs` is wiring
  only; only `arch/` touches ports/registers; drivers never call drivers;
  `galexy-core` has no layer dependencies.
- Each module exposes the **narrowest** API it can. Cross-module calls go
  through the owning module's public functions, never through its internals.
- Statics: `spin::Mutex` / `LazyLock` initialization; no global mutable
  `static mut` anywhere, ever.
- **`galexy-core` dependency rule:** `galexy-core` stays alloc-free,
  platform-independent, and has zero layer dependencies; everything else may
  use it. Crypto helpers (KDF, AEAD wrappers, CSPRNG fold) belong in
  `galexy-core` or a dedicated `galexy-crypto` crate — never in `arch/`.
- **Test kernels:** one integration test = one binary under `src/bin/` +
  one `#[test]` in `crates/runner/tests/`. Test kernels must `exit_qemu`
  with an explicit code; unexpected panics fail automatically.
- `main.rs` is a wiring file: it calls `init()` functions and then loops. All
  logic lives in modules.

## Syscall and interrupt path

- **IF=0 syscall path does not allocate** and does not take locks that a
  timer/IRQ also needs without the documented IRQ gate.
- User buffers are validated with a page walk (`USER_ACCESSIBLE` /
  writable) before copy; never trust a raw user pointer.
- Cap checks are **kernel grant ∩ handle snapshot** — never trust bits
  the user put in a Cap word alone.
- Lock order is documented in DESIGN (today: `THREADS` then galfs
  `TABLE`). New locks must extend that list in the same PR; no lock-order
  inversions, no “just this once”.

## Capabilities and tokens

- **Authentication ≠ authorization.** Passwords change `fs_root` / session;
  galfs tokens are the only way to touch objects. Admin `su` installs
  `ALL` on a named root; there is no blanket token bypass.
- Tokens name an **object id + rights**, not a path string. Path parse is
  lookup only; rights come from the card.
- `grant` may only install rights the caller already holds on that object
  (or an ancestor). `revoke` clears exact-object rights the caller names.
- Live-task grants die with the task unless a durable-share feature is
  explicitly designed (Milestone 45). Do not silently persist cards.
- Ramdisk `SPAWN_WAIT` utilities inherit the caller's cards — treat those
  ELFs as privileged. New utils need a one-line trust note in the PR.

## Scheduling

Runtime rules for Phase 7 (Milestones **56–58** ✅). Frozen policy:
`docs/SCHEDULING.md`. Process Caps stay in `PROCESS.md` — this section is
when tasks run, sleep, and block.

- **RR + pin + idle steal.** Do not grow CFS / nice / realtime classes
  without an explicit milestone. Pin-at-spawn and idle-pass steal are the
  placement story.
- **Deadline timer, not a 1 kHz metronome.** Busy CPUs re-arm a preempt
  quantum; idle stretches to the next real deadline. Sleep queues shorten
  that arm — they do not invent a second clock.
- **Monotonic time is `timer_ticks`.** Auth cool-downs and `sleep` share
  it. Do not add a wall-clock dependency to the scheduler path.
- **Park, don't spin.** New waits (sleep, empty pipe/keyboard) park the
  slot and wake from IRQ or peer close. Busy-poll “is name running?”
  loops are transitional; Cap-wait / event wake replace them.
- **No affinity ABI in v1.** Ownership is kernel policy; steal stays
  idle-only unless a later phase adds Caps.
- **Frozen numbers and ABI.** `Sleep`, `SLEEP_MS_MAX`, and
  `SysError::Interrupted` are stable (M58). Changing quantum / steal
  cooldown / `MAX_THREADS` / `SPAWN_FRAME_RESERVE` needs a milestone or
  DESIGN waiver — cite `docs/SCHEDULING.md` → Frozen policy.

## Compiler (gxc)

Rules for Phase 8 (Milestones **59–61**). Full plan: `docs/COMPILER.md`.
Host `rustc` remains how real programs (`shell`, utils) are built.

- **Subset, not rustc.** Document accepted syntax; reject the rest. Never
  claim Rust/cargo compatibility for `gxc` output.
- **Reuse the ABI.** Emit ELFs the existing loader accepts
  (`USER_IMAGE_BASE`, W^X, `_start` / Write / Exit). Do not invent a
  second userspace contract.
- **Hello is the gate.** New language features need a milestone checkbox;
  do not grow the subset to compile `shell` in the same breath as hello.
- **Host first.** `gxc` lands as a Linux host tool; on-OS compile is
  Milestone 62 only after 59–61 are boring.
- **License-aware reuse.** Prefer MIT/Apache (Cranelift, rustc-lite
  ideas, `object` / `iced-x86`). Vendored slices get attribution;
  evaluate before copying large trees.
- **Keep rustc `hello` green.** gxc demos use a distinct ramdisk name.


## Process model and init

Clean-slate rules for Milestones **47** and **53–55**. Full plan:
`docs/PROCESS.md`. Galexy is a capability OS — do **not** make global
integers the process API just because Unix did in 1970. Do not grow a
POSIX layer beside these rules.

- **Spawn, not fork.** New tasks are created by `spawn` (load ELF + args
  + attenuated caps/tokens). No `fork`/`clone` that duplicates an address
  space. Attenuation happens at spawn time, not after.
- **Process Cap is the identity.** `spawn` returns a **Cap** to the child
  (rights: wait / kill / transfer / inspect — exact bits in abi). Wait,
  kill, and supervise take that Cap. Holding the Cap *is* authority —
  same story as files and galfs tokens. No `kill(pid)` / `wait(pid)` that
  anyone can aim at a guessed number.
- **Debug ids are not handles.** A monotonic debug id (KOID-style) may
  appear in `tasks` listings and serial logs. It is **never** an
  authorization key and must not gain an `open_process(id)` ambient
  syscall. Scheduler slots may recycle; slots are not identities either.
- **Names are labels.** Task names are for humans and listings. Do not
  keep a permanent wait-by-name ABI once Cap-wait ships (migrate
  `SPAWN_WAIT` in the same milestone window).
- **Hierarchy is Cap transfer.** The kernel tracks a parent task. On
  parent exit, wait/control Caps for children move to **init** (or init
  receives equivalent rights). Exit status stays until a Cap-holder
  reaps it (zombie); unbounded zombies are a bug.
- **Init is userspace.** Init is the first ring-3 program and the
  orphan root — distinguished by role, not by magic “PID 1” in the ABI.
  Policy (seats, restart, shutdown order) lives in init; the kernel
  supplies mechanism: create init, transfer orphans, refuse kill of
  init’s process without a kernel path, panic if init exits.
- **Seats over ambient root.** Login TTYs and services are children of
  init (Caps init retains or delegates), each with their own session —
  not permanent kernel-injected admin shells.
- **Signals stay small.** Directed stop/fault via a process Cap (and
  later a **job/group Cap**). No full POSIX signal set unless a
  milestone checkbox says so.
- **Jobs / sessions (Milestone 55)** are capability-addressed groupings
  for TTY foreground and Ctrl-C — design the Cap rights before wiring
  the key, not a `setpgid` flag dump.
- **ABI changes** for process Caps / wait / kill ship in `galexy-abi` +
  DESIGN + `galexy-rt` + shell/init in the **same PR**, marked
  experimental until Milestone 51/55 freezes them.

## Secrets and passwords

- **Never log, serial-mirror, or `write_console` cleartext passwords.**
  Secret line mode suppresses echo (Milestone 43).
- Password material lives in **stack buffers**, zeroed with an explicit
  wipe helper after use (`zeroize`-style; volatile or equivalent). Do not
  leave hashes/passwords in reaped scratch pages without wipe.
- Compare password hashes in **constant time** (`hash_eq` or the KDF
  crate's verify). No early-out memcmp on secrets.
- Inline `login user pass` is a test convenience until Milestone 43 e2e
  lands; production UX is interactive prompts.
- Default `admin`/`admin` is for format bring-up only; Milestone 43 forces
  change before general use.

## On-disk formats (GALF and friends)

- Every on-disk layout change **bumps a version** and either migrates or
  refuses old images with a clear serial error. Silent reinterpretation
  of bytes is a bug.
- Dual-slot (or later journal) commits: write inactive → checksum/AEAD →
  flush → publish generation. Readers pick the newest valid slot only.
- Encryption (Milestone 44): plaintext file bytes and password hashes must
  not appear in a raw image dump. Tests may assert that.
- Host tools that parse GALF (`fsck`, inspect) live under `crates/` or
  `scripts/` and share structs with the kernel via `galexy-core` when
  possible — no duplicated magic numbers.

## Production vs test builds

- Test seams (`crash`, verbose sched logs, inline passwords) are
  **feature-gated** or cfg'd out of the default release image
  (Milestones 45 / 58).
- `cargo test -p runner --test boot` must stay green on every milestone
  merge. New QEMU boots get a matching `bin/test-*` or typing e2e.
- Prefer deterministic tests; when CSPRNG is required, inject a test
  seed behind `#[cfg(test)]` or a runner flag — never weaken production
  hashing in the same binary path without a loud cfg.

## Formatting

- `cargo fmt` is the law; `cargo clippy -- -D warnings` must pass.
- Max line length: whatever rustfmt uses (100 by default).
- Names: modules `snake_case`, types `UpperCamelCase`, kernel-agnostic names
  preferred (`ScreenWriter`, not `VgaTextBufferWriterFactory`).
- Syscall and right names stay aligned with `galexy-abi` (`TOKEN_*`,
  `USER_*`, `SPAWN_*`).

## Testing

- Pure logic (e.g. scancode → char, line-buffer edit, path parse, KDF)
  gets host `#[test]` in `galexy-core` / `galexy-abi` when possible.
- Kernel-visible behavior gets a QEMU boot test (`bin/test-*`) or a
  typing e2e under `crates/runner/tests`.
- Negative cases (AccessDenied, pre-login cannot spawn, bare spawn cannot
  write) are first-class — not only the happy path (Milestone 51).
- Anything touching hardware ports gets verified in QEMU; note the
  verification step in the PR/commit message.
- Do not check in `galfs.img` or other stateful disk images.

## Docs hygiene

- When behavior changes, update in this order: code → doc comment →
  `TODO.md` checkbox → `README.md` feature list → relevant deep doc
  (`DESIGN.md`, `AUTH.md`, `GALFS.md`, `PROCESS.md`, later `THREAT.md`).
- `ROADMAP.md` only for direction shifts or new phases.
- `TODO.md` checkboxes are only checked after the item is *verified
  working* (e.g. seen in QEMU), never when "written".
- ABI changes: `galexy-abi` + DESIGN syscall section + shell/`galexy-rt`
  wrappers in the **same PR**. Mark stable vs experimental in the ABI
  table (Milestone 51).

## Git

- Commit message style: `milestone: short imperative summary` (e.g.
  `mm: frame allocator over boot memory map`, `auth: argon2id actor
  passwords`).
- Never commit build artifacts; `target/` and `*.img` are gitignored.
- One logical change per commit when practical; docs-only commits are
  fine for milestone planning.