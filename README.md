<p align="center">
  <img src="docs/logo.png" alt="Galexy.OS" width="280">
</p>

# Galexy.OS

A small **capability-based** operating system in Rust. Boots BIOS and UEFI,
runs on two CPUs, and has a real ring-3 userland: login seats under a
userspace init, a shell with pipelines and line editing, utilities, a
sealed disk-backed filesystem (galfs), process Caps, sleep / block /
wake scheduling, and a tiny Rust-subset compiler that emits runnable
programs.

Not a Linux clone. No POSIX claim. Authority is **capabilities** (and
galfs access cards), not global file descriptors or PIDs.

**Status (October 2026):** Milestones 1–61 and 63–67 are in this tree,
plus 69 (62 was superseded); `nano` is in the ramdisk. Milestone 67
gives init shutdown, `svc`, and the ABI freeze (`spawn` stays
experimental). The QEMU suite is 111 boots plus host tests. The
Milestone 52 review checklist is complete except the `review-rc1` tag,
which is the owner's call. The `v1.0` tag is a separate gate
(BIOS+UEFI, TCG+KVM, q35+pc, `PERF.md`).
The honest scorecard is in [`docs/ROADMAP.md`](docs/ROADMAP.md) → Where
we stand. Known gap today: there is no network. Legacy device paths
(PIT, 8259, PS/2, PIO IDE, legacy virtio) remain as named fallbacks.
KPTI, IBRS, MDS, and CET are waived for this single-tenant guest
(`docs/THREAT.md`). PCID and a new kernel heap allocator stay waived
(`docs/PERF.md`). `Map`, `Clock`, and channels are stable. `spawn`
stays experimental.

| You want… | Read |
| --- | --- |
| Passwords, login, access cards | [`docs/AUTH.md`](docs/AUTH.md) |
| galfs trees, tokens, sealed disk | [`docs/GALFS.md`](docs/GALFS.md) |
| Process Caps, init, seats | [`docs/PROCESS.md`](docs/PROCESS.md) |
| Scheduler, time, block/wake (frozen v1) | [`docs/SCHEDULING.md`](docs/SCHEDULING.md) |
| Mini Rust compiler / gxc | [`docs/COMPILER.md`](docs/COMPILER.md) |
| Rust target + upstream `rustc` on Galexy | [`docs/RUSTC.md`](docs/RUSTC.md) |
| `gxld` static linker | [`docs/LINKER.md`](docs/LINKER.md) |
| How the kernel is wired | [`docs/DESIGN.md`](docs/DESIGN.md) |
| Coding rules | [`docs/STYLE.md`](docs/STYLE.md) |
| What’s next | [`docs/ROADMAP.md`](docs/ROADMAP.md) · [`TODO.md`](TODO.md) |
| Reviewer demo (login, card, two TTYs) | [`docs/DEMO.md`](docs/DEMO.md) |

## Try it

**Needs:** Linux, QEMU, Rust nightly (from `rust-toolchain.toml`), and for
UEFI an OVMF image.

```sh
# Arch example
sudo pacman -S --needed qemu-desktop ovmf

cargo run --release                 # BIOS, headless; type on this terminal
cargo run --release -- --display    # also open the framebuffer window (F1–F12)
cargo run --release -- --uefi       # UEFI — set OVMF_FD if the default path is missing
```

`cargo run --release` attaches a persistent `galfs.img` as **virtio-blk-pci**
on `-M q35` (virtio 1.x; boot image stays IDE index 0 with `snapshot=on`,
which q35 places on AHCI). A `virtio-keyboard-pci` device is the keyboard;
PS/2 is the fallback when that device is absent. Guest galfs prefers
virtio-blk when present, else the ATA primary slave.
Override the image path with `GALEXY_GALFS_IMG=/path/to/img`. Set
`GALEXY_GALFS_IDE=1` to force the IDE-slave attach. Delete `galfs.img` to
force a fresh format after a layout bump. Persistence e2e covers IDE
(`cache=writethrough` / `writeback` / `none`) and virtio-blk (see
`docs/GALFS.md`).

UEFI firmware path defaults to `/usr/share/ovmf/x64/OVMF.4m.fd`. On many
distros:

```sh
OVMF_FD=/usr/share/ovmf/OVMF.fd cargo run --release -- --uefi
```

### First boot

You should see a login screen on this terminal (tty1; the serial line is
the console). Quit with **Ctrl-A** then **X**. **Ctrl-A** then **C** is
the QEMU monitor, so Ctrl-A does not reach the shell in this mode (the
window's keyboard still sends it). F1–F12 need the window
(`--display`); they are PS/2 keys and have no UART equivalent. Kernel
log lines share this stream with the console.

```text
Galexy.OS v0.1.0 (tty1)

Login as: admin
Password: *****
```

Fresh format password is **`admin`** / **`admin`**. After login a
fastfetch-style **command center** dashboard prints (OS / TTY / user /
uptime / heap / galfs / tasks), then the prompt is `admin@galexy> `.
The bottom status bar keeps a live strip (uptime, heap, galfs, tasks,
frames). Up/down arrows recall session history (written to
`shell.history` on `logout`).

| Keys / command | What it does |
| --- | --- |
| F1–F12 | Switch consoles (each seat has its own login) |
| ↑ / ↓ | Command history (`shell.history`) |
| ← / → · Ctrl-A · Ctrl-E · Ctrl-U | Move inside the line, jump to the ends, clear it |
| Ctrl-C | Cancel a prompt, or kill the foreground job |
| `fetch` | Re-show the login dashboard |
| `help` | Commands |
| `ls` / `echo hi` / `mkdir box` / `cat` / `nano` / `cp` / `mv` / `rm` / `stat` / `truncate` | Files under your tree. `nano <path>` edits; Ctrl-O saves, Ctrl-X leaves |
| `echo hi \| cat` · `echo *` | One pipeline (pipe + `give`); one `*` per word in the current directory |
| `whoami` / `users` / `useradd` / `passwd` / `quota` / `tokens` | Identity, cards, limits |
| `grant` / `revoke` / `share` / `unshare` / `su` | Access cards (see AUTH.md) |
| `stats` / `tasks` / `threads` / `dmesg` / `sync` | Queries and the kernel log (after login) |
| `echo $?` | Exit status of the last Cap-waited program |
| `logout` | Back to the login screen |
| `hello` / `linger` / `nap` | Sample user programs |
| `shutdown` / `reboot` | Power (admin) |

Typing `shell` is refused — seats are F-keys, not programs you spawn.

## What it is (short)

- **SMP kernel** — two CPUs, per-CPU deadline timers, tickless idle, work-stealing idle, TLB shootdown
- **Ring-3 userland** — ELF from a tar ramdisk, per-task address spaces, W^X maps, SYSCALL ABI in `galexy-abi` (26 syscalls)
- **Capabilities day one** — keyboard, console, loader, files, power, queries, dmesg; process Caps for wait / kill / give; no fd table, no PIDs
- **galfs** — sealed dual-slot GALF v11 on virtio-blk or IDE; 32 actors, quotas, durable shares, rename / truncate / stat, host `fsck`
- **Auth** — PBKDF2 passwords, lockout, idle logout, forced first `passwd`; every seat boots logged out; audit lines carry no secrets
- **Init and seats** — userspace `init` is the orphan root and respawns F-key seats; Ctrl-C kills the TTY foreground job
- **Scheduling** — RR + pin-at-spawn + idle steal; `sleep`; blocking keyboard and pipe reads park and wake (`docs/SCHEDULING.md`)
- **gxc** — a frozen Rust-subset compiler on the host that emits a hello object the linker turns into an ELF the loader runs; a fixture, not the self-host path (`docs/COMPILER.md`)
- **gxld** — the static ELF64 linker: `no_std` library + GNU-ld-compatible CLI; `rustc -Clinker=gxld` links every userspace program and the `galexy-os-gxld` image passes the same tests as the `rust-lld` one (`docs/LINKER.md`)

## Tests

```sh
cargo test -p galexy-core -p galexy-abi -p galexy-crypto -p galexy-galf -p gxc -p gxld   # host suites
cargo test -p runner --test audit_strings   # no "password" in any serial line
cargo test -p runner --test boot --release -- --test-threads=1   # QEMU suite (109 boots, -smp 2, release profile)
```

UEFI cases need `OVMF_FD` if the default firmware path is absent. Disk
and typing cases want `--test-threads=1`; the suite runs under TCG
(no KVM yet — Milestone 64) so a full pass takes a while. Test kernels
live under `crates/galexy-os/src/bin/` (73 today); the runner builds
one image per binary and checks exit codes + serial.

### CI

`.github/workflows/ci.yml` runs on every push to `master` and every
pull request, on the toolchain pinned in `rust-toolchain.toml`:

| Job | What it proves |
|---|---|
| `host` | `cargo fmt --all -- --check`; host suites (abi, core, crypto, galf, fsck, gxc, gxld); `clippy -D warnings` on the host crates (`--all-targets`), the kernel bins (`x86_64-unknown-none`), and the userspace programs |
| `qemu` | builds every image, `clippy` on the runner and its tests, `audit_strings`, then the full boot suite with `--test-threads=1` under TCG on `ubuntu-latest` (`qemu-system-x86` + `ovmf`, `OVMF_FD=/usr/share/ovmf/OVMF.fd`) |

The boot suite *is* the matrix — each axis is a named test rather than
a workflow dimension, so a failure points at one boot:

| Axis | Covered by |
|---|---|
| Firmware | BIOS for every kernel; UEFI via `uefi_image_boots_and_timer_ticks`, `shell_run_hello_typing_e2e_uefi` |
| CPUs | `-smp 2 -cpu max` on every boot; `smp_*`, `ipi_*`, `smpuser_*`, `smpstress_*` |
| galfs disk | off for most kernels; on for `galfs_disk_*`, `share_disk_*`, `assert_galfs_disk_persists`, `crash_injection_*` |
| Disk transport | virtio 1.x (`galfs_disk_persists_virtio_blk`), legacy virtio I/O BAR (`galfs_disk_persists_virtio_legacy`), IDE/ATA on `-M pc` (`galfs_disk_persists_across_reboot`, `ata_absent_returns_unsupported`), partition offset |
| Cache mode | `galfs_disk_persists_writeback_cache`, `galfs_disk_persists_none_cache` |
| Linker | `rust-lld` image for every kernel; `gxld` image via `gxld_image_*_e2e` |

Serial logs (`/tmp/galexy-serial-*.log`) are uploaded as an artifact
when the QEMU job fails.

```sh
cargo build --release
find target -name "galexy-os-*.img"
```

## For reviewers

Everything a systems engineer needs to poke at it in one place. Read
`docs/THREAT.md` first (assets, adversaries, non-goals), then
`docs/DEMO.md` for an eight-step walk through login, a second account,
an access card, and two consoles.

**Toolchain.** `rust-toolchain.toml` pins `nightly-2026-10-08`
(`rustc 1.101.0-nightly (1d81eb4ad 2026-10-07)`) with the
`x86_64-unknown-none` / `x86_64-unknown-uefi` targets and `rust-src`;
`rustup` installs it on the first `cargo` command (add `rustfmt` and
`clippy` to run the CI checks locally). Host packages: `qemu-system-x86_64` and an
OVMF image for UEFI. Nothing else.

**Build and run.**

```sh
cargo build -p runner                       # every image under target/
cargo run --release                         # BIOS, headless, -smp 2; KVM if /dev/kvm is writable, else TCG
cargo run --release -- --display            # same, plus the framebuffer window
OVMF_FD=/usr/share/ovmf/OVMF.fd cargo run --release -- --uefi   # UEFI; path varies per distro
scripts/review-smoke.sh                     # focused subset: host suites + 11 boots (~10 min TCG)
```

**Disk.** `cargo run --release` creates and attaches `galfs.img` in the repo root
(gitignored, virtio-blk). Fresh image ⇒ fresh format ⇒ the shell asks
for a volume passphrase only once there is a sealed slot; the
bring-up passphrase is `galfs`. `GALEXY_GALFS_IMG=/path` relocates it;
`GALEXY_GALFS_IDE=1` attaches it as the IDE slave instead. If a boot
refuses the disk after a GALF version bump (`[galfs] disk corrupt;
refusing silent format`, or `fsck` reports a foreign version), delete `galfs.img`
and boot again — the kernel never silently reformats a volume it can
read but not parse.

**Accounts.** Fresh format: `admin` / `admin`, and the first command
must be `passwd`. Every F-key seat (F1–F12) is its own login; nothing
is logged in at boot. `useradd`, `grant`, `share`, `su` are in
`docs/AUTH.md`.

**Where the evidence goes.**

| What | Where |
| --- | --- |
| Kernel log and the console | COM1 (this terminal under `cargo run`; type here). The same kernel lines via `dmesg` after login. `--display` adds the framebuffer window |
| Runner serial logs | `$TMPDIR/galexy-serial-<image>-<n>.log` (`/tmp` on Linux), one per boot; uploaded as a CI artifact on failure |
| Ramdisk measurement | `cargo:warning=ramdisk.tar sha256=…` at build; `[test-ramdisk] sha256 …` at boot |
| Images | `target/debug/build/runner-*/out/galexy-os-{bios,uefi}.img` is the main kernel; `galexy-os-gxld-*.img` carries the `gxld`-linked ramdisk; `galexy-os-crashseam-*.img` the `crash-seam` shell; `galexy-os-test-*-*.img` one per test kernel |
| Screen | `scripts/boot-test.sh <image> <secs>` dumps `shot.ppm` and `serial.log` under `$WORKDIR` |

**What to read while it boots.** `docs/ABI.md` (which syscalls are
stable), `docs/PERF.md` (budgets and fixed capacities), `DESIGN.md` →
Testing strategy (which test kernel guards which milestone, and the
feature-gated seams that are off in the default image), `TODO.md`
Milestone 52 (the review gate itself).

## Repo map

```
docs/           logo.png, STYLE, ROADMAP, THREAT, ABI, PERF, DESIGN, AUTH, GALFS, PROCESS, SCHEDULING, COMPILER, LINKER, RUSTC, DEMO
LICENSE         MIT · SECURITY.md reporting + scope · CHANGELOG.md one line per milestone PR
TODO.md         milestone checkboxes
crates/
  galexy-abi/     syscall numbers, Cap model, errors (host-tested)
  galexy-core/    pure primitives (host-tested)
  galexy-crypto/  PBKDF2-HMAC-SHA256, ChaCha20, HMAC (host-tested)
  galexy-galf/    GALF on-disk layout + sealed unlock (host-tested)
  galfs-fsck/     host fsck CLI over galfs.img
  galexy-os/      kernel (lib + main + 70 test bins)
  userspace/      galexy-rt, init, shell, util, hello
  gxc/            mini Rust-subset compiler (gxr) — COMPILER.md
  runner/         image build, `cargo run`, QEMU boot tests
```

## Design stance

1. **Capabilities over ambient authority** — knowing a name or number is
   not enough; you need a Cap or a galfs card.
2. **Spawn, not fork** — new tasks are loaded ELFs with attenuated rights.
3. **Process Caps, not PIDs** — wait/kill/give by Cap; see PROCESS.md.
4. **Test what can be tested** — host tests for pure code; one QEMU boot
   per integration kernel; suite stays green on every milestone.
5. **Modules stay swappable** — screen, serial, keyboard behind narrow APIs;
   `main.rs` is wiring only.
6. **Modern path first, legacy as a named fallback** — and measure before
   optimizing (ROADMAP standing principles).

## Where we’re going

- **Phase 5** — review readiness: Milestones 43–50 ✅; **51** (docs,
  CI, soak) and **52** (`review-rc1`) are next
- **Phase 6** ✅ — userspace init, seats under init, job Caps
  (shutdown through init and `svc` → Milestone 67)
- **Phase 7** ✅ — scheduling complete (sleep / block-wake / policy) — SCHEDULING.md
- **Phase 8** ✅ — mini Rust-subset compiler (`gxc`) for hello, now frozen — COMPILER.md
- **Phase 9** — hardened (SMEP/SMAP/UMIP/KASLR, hostile ELF), fast
  (bench, KVM, LTO, IRQ completion), modern (q35, ECAM, virtio 1.x,
  MSI-X, TSC-deadline), full userland (heap, argv, clock, channels,
  jobs), init-owned shutdown → `v1.0`
- **Phase 10** — network (virtio-net, small stack, sockets as Caps),
  after `v1.0`
- **Phase 11** — Rust on Galexy: `x86_64-unknown-galexy` target, `gxld`
  static linker (LINKER.md, ✅ Milestone 69), `std` PAL, upstream
  `rustc` with the Cranelift backend compiling and linking on-OS —
  RUSTC.md

Host-compile the gxr hello (not rustc) and link it with `gxld`:

```sh
cargo run -p gxc -- build -o hello-gxc.elf crates/gxc/examples/hello.gxr   # object → gxld in-process
cargo run -p gxc -- build -c -o hello-gxc.o crates/gxc/examples/hello.gxr  # object only
cargo run -p gxld -- -o hello-gxc.elf hello-gxc.o                           # same bytes
```

The runner packs that ELF as ramdisk `hello-gxc` for QEMU
(`test-hellogxc`). Any userspace program links the same way:
`RUSTFLAGS="-Clinker=target/release/gxld -Clinker-flavor=ld" cargo build -p shell --target x86_64-unknown-none`.
Details: [`docs/COMPILER.md`](docs/COMPILER.md), [`docs/LINKER.md`](docs/LINKER.md).

Details: [`docs/ROADMAP.md`](docs/ROADMAP.md).

## References

- [Writing an OS in Rust](https://os.phil-opp.com/) — structural foundation
- [bootloader](https://github.com/rust-osdev/bootloader) · [rust-osdev](https://github.com/rust-osdev)
- [Cranelift](https://github.com/bytecodealliance/wasmtime/tree/main/cranelift) ·
  [rustc-lite](https://github.com/suhteevah/rustc-lite) — codegen / subset
  ideas for `gxc` (see COMPILER.md)
