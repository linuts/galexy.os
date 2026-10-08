# THREAT — what galexy.os defends, against whom, and what it does not

This is the page a reviewer reads before the code. It names the assets,
the adversaries, the trust assumptions, the controls that exist today
(each with the test that proves it), the gaps that are tracked rather
than waived, and the non-goals with a one-line reason each.
`SECURITY.md` points here for scope. Deep mechanism lives in
`DESIGN.md` (memory, locks, loader), `AUTH.md` (passwords, sessions,
spawn policy), `GALFS.md` (filesystem, sealed slots), and `PROCESS.md`
(process Caps).

Status: **review-rc1 bar** (Milestones 43–52) plus Milestone 63
(CPU features, per-actor KDF cost, user-VA copies under SMAP). The
`v1.0` bar still adds Milestones 64–67. Waivers for KPTI, IBRS, MDS,
and CET are in [CPU features](#cpu-features) below.

## Assets

| Asset | Where it lives | Why it matters |
| --- | --- | --- |
| **File bytes and metadata** in galfs | sealed GALF slots on the virtio-blk / IDE disk; RAM table when no disk | the user's data; per-actor trees with quotas |
| **Password hashes and salts** | actor records inside the sealed slot | offline guessing of a stolen image |
| **Volume key** | wrapped in the slot header; unwrapped copy in kernel RAM while unlocked | unwraps every file byte on the disk |
| **Live sessions** | per-TTY seat state: actor, session generation, idle clock | a logged-in seat is an authority |
| **Capabilities and tokens** | per-task Cap tables (console, keyboard, loader, power, queries, files, pipes, process Caps) and galfs token tables | the only way to touch anything; forging one is total compromise of that resource |
| **Kernel integrity** | ring 0 code and data, page tables, scheduler state | everything above assumes it |
| **Console and serial** | framebuffer text, COM1 mirror, `dmesg` ring | must never carry a secret |
| **Ramdisk ELFs** | tar baked into the boot image (`init`, `shell`, utilities, `hello`) | run with the caller's cards when spawned with inherit |

## Adversaries

| Adversary | Has | Wants | Verdict |
| --- | --- | --- | --- |
| **A1 Stolen disk** | a raw copy of `galfs.img` / the IDE slave, unlimited offline time | file bytes, password hashes, the volume key | **defended** — sealed GALF (`GALFS.md` → Sealed slots): random volume key, ChaCha20 + HMAC-SHA256 per slot, KEK from PBKDF2 of the volume passphrase. Raw dumps contain no plaintext; `galfs_disk_*` and `unlock_test_passes` assert refusal on a wrong passphrase. Actor passwords store their PBKDF2 count (GALF v12): test and TCG boots write 10 000, a `--release` kernel under KVM writes 100 000, and `passwd` re-derives at the current default. The volume KEK stays at 10 000. The bring-up passphrase is still `galfs`; the operator sets a real passphrase at first boot |
| **A2 Malicious user program** | ring 3, any ramdisk binary spawned *bare* (no inherit), any syscall with any argument | crash the kernel, read another task, escalate to galfs | **defended** — per-task address spaces, W^X user maps, NX, guard pages, SMEP / SMAP / UMIP when the CPU reports them, user-buffer page walk before every copy, `stac`/`clac` around user-VA copies, Caps = kernel grant ∩ handle snapshot, empty token table on a bare spawn. Proven by `userfault`, `wx`, `smep`, `smap`, `umip`, `capforge`, `badelf`, `negative`, `procbudget`, `treechurn`. The syscall promise is `SysError`, never a panic |
| **A3 Malicious second seat** | a login on F2 while the victim is on F1; or a stolen-but-live seat before idle logout | the victim's files, the victim's session | **defended** — a password buys `RIGHT_ALL` on your own root only; everything else is a token the owner granted (`grant`/`share`/`su` card). Caps and tokens are per task, not per TTY. Confused-deputy rules in `cards_test_passes` (kernel) and `galexy_galf::cards` host property tests. Idle logout and lockout bound the live-seat window (`idle_test_passes`, `lockout_test_passes`) |
| **A4 Hostile utility with inherit** | a ramdisk binary the shell spawns with `SPAWN_INHERIT` | the caller's whole tree | **accepted risk** — the ramdisk is trusted input (below). The rights mask in `r10` lets a caller attenuate what a utility inherits (`attenuate_tokens` is covered by the `galexy_galf::cards` host tests; no in-tree spawner sets the mask yet — the shell passes the full set) |
| **A5 Keyboard flooder / console hog** | a seat, a tight loop | starve other seats, hide audit lines | **bounded, not security** — 512 B/tick console budget per task (`pathological_test_passes`: a 3 000-call flood is admitted at ≤ 512 × (ticks + 2) bytes and never errors), a 65-byte paste at the password prompt is refused and masked (`shell_password_paste_typing_e2e`), keyboard overflow drops newest and counts (`audit_console_test_passes`); quotas cap galfs objects and bytes per actor. Fairness mechanisms, not isolation guarantees |

## Trust assumptions

Things the system believes without checking, each with the reason.

| Trusted | Why | Evidence that it is at least *measured* |
| --- | --- | --- |
| **Physical F1 / the hypervisor** | whoever can plug in a keyboard or edit the QEMU command line can also replace the disk image; no OS-level control changes that | — |
| **Bootloader and firmware** (`bootloader` 0.11, BIOS or OVMF) | they map the kernel and hand over `BootInfo`; there is no secure boot or measured boot chain | — |
| **Ramdisk publisher** | the tar is built by `runner/build.rs` from this repository's userspace crates; nothing on the running system can modify it | SHA-256 of the packed tar is printed at build (`cargo:warning=ramdisk.tar sha256=…`), exported as `GALEXY_RAMDISK_SHA256`, and re-measured by the kernel at boot (`test-ramdisk` prints it; `ramdisk_test_passes` asserts equality). There is **no allowlist and no signature**: a hash only proves the image the runner built is the image that booted |
| **The kernel itself** | SMEP, SMAP, and UMIP are on when CPUID reports them. KPTI is waived: the kernel half stays mapped in every tree (`CPU features` below) | `smep_test_passes`, `smap_test_passes`, `umip_test_passes` |
| **`galexy-crypto`** | SHA-256, HMAC, PBKDF2, ChaCha20 are in-tree, unaudited implementations chosen for `no_std` and size | host tests against published vectors; constant-time `hash_eq` |
| **RDRAND** | salts, volume keys, and nonces come from `arch::rand` (RDRAND with a tick-mixed fallback) | the fallback is weak by construction and only exists so a CPU without RDRAND still boots a test image |

## Controls in one table

What exists, which doc owns it, and the test that fails if it regresses.

| Control | Owner doc | Guard |
| --- | --- | --- |
| Capabilities on day one; no fd table, no PIDs; rights = grant ∩ snapshot | `DESIGN.md` → sched/syscalls, `PROCESS.md` | `capforge_test_passes`, `selfcap`, `procgive`, `procbudget` |
| Per-task address space, W^X, NX, guard page, kstack canary, stack and secret wipe on reap | `DESIGN.md` → Memory policy | `freshl4`, `wx`, `userfault`, `treechurn`, `reuse` |
| User-buffer page walk before every copy; length caps on names, paths, passwords, writes; user-VA copies go through `arch::user_copy` (`stac`/`clac` when SMAP is on) | `DESIGN.md` → User pointers | `syscall_test_passes`, `smap_test_passes`, `badelf` (truncated inputs), `paths` |
| ELF validation before any page is mapped: class, machine, type, phdr table bounds, `filesz ≤ memsz`, 512 MiB user window, W\|X refused, overlapping `PT_LOAD` refused, entry inside an executable segment | `DESIGN.md` → Hostile ELF images | `badelf_test_passes` |
| Pre-login seat cannot spawn (kernel rule: no `fs_root` ⇒ `AccessDenied`, independent of the loader grant); bare spawn gets empty tokens | `AUTH.md` → Spawn policy | `negative_test_passes`, `shell_nested_spawn_refused_e2e` |
| PBKDF2-HMAC-SHA256 passwords, CSPRNG salt, constant-time compare, no echo, staging wipe on every path; iteration count stored per actor | `AUTH.md` | `users`, `mustchange`, `assert_passwords_masked`, `audit_strings`, `galexy_crypto` stored-count test |
| Lockout (five misses lock actor and TTY), idle logout, forced first `passwd`, session generation | `AUTH.md` | `lockout`, `idle`, `mustchange`, `shell_must_change_typing_e2e` |
| Sealed dual-slot GALF, newest valid generation wins, refuse silent format, torn-write recovery | `GALFS.md` → On-disk | `galfs_disk_recovers_*`, `galfs_disk_refuses_format_when_both_slots_corrupt`, `crash_injection_picks_consistent_slot`, `galexy_galf::slots_test` |
| Volume key wiped on last logout and power; wrong passphrase stays RAM-only | `AUTH.md` → Sealed GALF | `unlock_test_passes`, `shutdown_test_powers_off` |
| Token algebra: grant needs every right on the object or an ancestor; revoke is exact-object; shares are durable and re-applied at login; `ONCE` cards burn on `su` | `GALFS.md` → Rights, Tokens | `galfs`, `cards`, `shares`, `share_disk`, `galexy_galf::cards_test` |
| Path grammar: ≤ 8 components, ≤ 64 bytes each, charset `[A-Za-z0-9._-]`, `.`/`..` rejected | `GALFS.md` → Paths | `paths_test_passes`, `galexy_core::path_test` exhaustive sweep |
| Quotas per actor (objects, bytes) | `GALFS.md` → Quotas | `quota_test_passes` |
| Audit lines never carry secrets; `dmesg` is a read-only ring behind a Cap | `STYLE.md` → Secrets, `DESIGN.md` → console | `audit_strings`, `audit_console_test_passes` |
| Init is the orphan root and immortal to user kill; seats are pre-login; init has no keyboard | `PROCESS.md` | `init_test_passes`, `orphan`, `jobcap` |
| Lock-order table (`THREADS` then galfs `TABLE`); IF=0 syscall path never allocates | `DESIGN.md` → Lock order | the SMP suite (`smp`, `ipi`, `smpstress`, `smpuser`) |
| Ramdisk measured at build and boot | this page → Trust | `ramdisk_test_passes` |

## CPU features

What the kernel turns on, what it waives, and what is still absent.
`arch/cpu.rs` reads CPUID leaf 7 and sets the bit only when the leaf
reports it. `-cpu max` (TCG) and `-cpu host` (KVM) both report SMEP,
SMAP, and UMIP. A CPU without the bit boots with that bit clear.

| Feature | State | Why |
| --- | --- | --- |
| FSGSBASE | **on** (required) | per-CPU GS. Boot panics if CPUID 7.0 EBX bit 0 is clear |
| SMEP | **on** when CPUID 7.0 EBX bit 7 | ring 0 cannot execute a user page. `smep_test_passes` |
| SMAP | **on** when CPUID 7.0 EBX bit 20 | ring 0 cannot read a user VA unless `EFLAGS.AC` is set. Only `arch::user_copy` executes `stac` / `clac`. `smap_test_passes` |
| UMIP | **on** when CPUID 7.0 ECX bit 2 | `sgdt` / `sidt` / `sldt` / `smsw` / `str` at CPL 3 raise `#GP`. The `#GP` vector shares the ring-3 kill path with `#PF`. `umip_test_passes` |
| Kernel KASLR | **on** | `BootloaderConfig.mappings.aslr` randomizes the kernel, stack, framebuffer, ramdisk, and boot info inside P4 indexes 1..=24. Fixed slots stay put (physical memory 128, heap 170, LAPIC 200, I/O APIC 201, recursive 511, user image 25). `kaslr_kernel_base_differs_across_boots` |
| Spectre v1 | **masked** | after the bounds check, the syscall number and Cap-slot indexes pass through `cpu::spectre_mask` (`lfence`, then an arithmetic mask). This is not a claim about other gadgets |
| KPTI / Meltdown | **waived** for v1.0 | single-tenant guest. The kernel half stays mapped in every task tree. A second tenant, or a host without the Meltdown microcode/hardware fix, changes this |
| IBRS / retpoline | **waived** for v1.0 | same assumption: one guest, a host that has already mitigated cross-VM branch history. No retpoline in this kernel |
| MDS | **waived** for v1.0 | same single-tenant guest. No `verw` / buffer-overwrite sequence |
| CET shadow stacks | **waived** | QEMU TCG coverage of CET is thin, and the kernel has no shadow-stack ABI |
| User ASLR | **waived** | every ELF links at `USER_IMAGE_BASE` (see Non-goals) |

## Absent, tracked (not waived)

These are gaps a reviewer will find. Each has an owner milestone; none
is claimed as covered.

| Gap | Today | Milestone |
| --- | --- | --- |
| Release profile, KVM, timing | everything is measured under TCG at `opt-level = 0`; side-channel timing has not been looked at | **64** |
| fsck repair into a new slot | host `fsck` detects; recovery is "pick the other slot" | GALFS follow-on |

## Non-goals (scope freeze)

Written down so the review is about what is here. Each has a reason;
"later" means a phase after `v1.0`, "never" means a design decision.

| Non-goal | Rationale |
| --- | --- |
| **No network stack** | nothing here is reachable remotely, which removes the largest adversary class from the review; Phase 10, after `v1.0` |
| **No GPU / multi-framebuffer** | the console is a text grid on the bootloader's GOP framebuffer; a second display adds device code with no security content |
| **No POSIX compatibility claim** | the ABI is Caps and `SysError`, not fds, `errno`, signals, or `waitpid`; claiming POSIX would import its ambient-authority model (`PROCESS.md` → Why not PIDs) |
| **No ambient process namespace** | there is no kill / wait / open by a guessed global integer; you hold a Cap or you do not. This is the point of the design, not a missing feature |
| **No MFA, networked IdP, or PAM** | single-machine identity; a password plus physical seat is the model, and there is no network to reach an IdP |
| **No demand-paged swap** | every user map is fixed at spawn (`DESIGN.md` → Memory policy); swap would add a page-fault path to the isolation story for no reviewer benefit on a RAM-sized workload |
| **No multiprocessor device drivers** | keyboard and framebuffer stay on the BSP; a single consumer avoids a lock class whose only payoff is typing speed (`DESIGN.md` → Concurrency model, waived in Milestone 48) |
| **No secure boot / measured boot** | the bootloader and firmware are trusted (above); the ramdisk hash is a build-reproducibility check, not a root of trust. A signed chain is a project of its own |
| **No systemd / dbus** | init is a small supervised seat table (`PROCESS.md`); a service table and `svc` are Milestone 67 and stay small |
| **No user-side ASLR** | every ELF links at `USER_IMAGE_BASE`; randomizing the slot would not hide the address from the program itself and would break the single load address the loader and ABI share (`DESIGN.md` → ASLR) |
| **No Argon2id / Poly1305** | PBKDF2-HMAC-SHA256 and HMAC-SHA256 tags reuse one primitive the crate already carries; a memory-hard KDF needs a dedicated stack (waived in Milestones 43–44) |
| **No desktop-class throughput** | the suite runs TCG at `opt-level = 0`; Milestone 64 measures before anything is optimised (`PERF.md`) |
| **No `review-rc1` tag yet** | the owner tags when the review happens; the checklist in `TODO.md` Milestone 52 is the gate |
