# Galexy.OS

A small **capability-based** operating system in Rust. Boots BIOS and UEFI,
runs on two CPUs, and has a real ring-3 userland: login seats under a
userspace init, a shell with pipelines and line editing, utilities, a
sealed disk-backed filesystem (galfs), process Caps, sleep / block /
wake scheduling, and a tiny Rust-subset compiler that emits runnable
programs.

Not a Linux clone. No POSIX claim. Authority is **capabilities** (and
galfs access cards), not global file descriptors or PIDs.

**Status (October 2026):** Milestones 1–50 and 53–61 are merged; the
QEMU suite is 87 boots plus host tests, all green. The next two
milestones (51–52) produce `review-rc1`; Phase 9 (63–67) is the
hardening, performance, modern-platform, and userland work toward
`v1.0`. The honest scorecard is in [`docs/ROADMAP.md`](docs/ROADMAP.md)
→ Where we stand. Known gaps today: no SMEP/SMAP/UMIP/KASLR, nothing
profiled, legacy device paths (PIT, 8259, PS/2, PIO IDE, legacy virtio)
are the only paths, no user heap or argv, no network.

| You want… | Read |
| --- | --- |
| Passwords, login, access cards | [`docs/AUTH.md`](docs/AUTH.md) |
| galfs trees, tokens, sealed disk | [`docs/GALFS.md`](docs/GALFS.md) |
| Process Caps, init, seats | [`docs/PROCESS.md`](docs/PROCESS.md) |
| Scheduler, time, block/wake (frozen v1) | [`docs/SCHEDULING.md`](docs/SCHEDULING.md) |
| Mini Rust compiler / gxc | [`docs/COMPILER.md`](docs/COMPILER.md) |
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

cargo run                 # BIOS (default); COM1 on your terminal
cargo run -- --uefi       # UEFI — set OVMF_FD if the default path is missing
```

`cargo run` attaches a persistent `galfs.img` as **virtio-blk-pci**
(legacy IO BAR; boot image stays IDE master `index=0` with `snapshot=on`).
Guest galfs prefers virtio-blk when present, else the ATA primary slave.
Override the image path with `GALEXY_GALFS_IMG=/path/to/img`. Set
`GALEXY_GALFS_IDE=1` to force the IDE-slave attach. Delete `galfs.img` to
force a fresh format after a layout bump. Persistence e2e covers IDE
(`cache=writethrough` / `writeback` / `none`) and virtio-blk (see
`docs/GALFS.md`).

UEFI firmware path defaults to `/usr/share/ovmf/x64/OVMF.4m.fd`. On many
distros:

```sh
OVMF_FD=/usr/share/ovmf/OVMF.fd cargo run -- --uefi
```

### First boot

You should see a login screen on tty1 (serial mirrors the visible console):

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
| `ls` / `echo hi` / `mkdir box` / `cat` / `cp` / `mv` / `rm` / `stat` / `truncate` | Files under your tree |
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
- **gxc** — a Rust-subset compiler on the host that emits a hello ELF the loader runs (`docs/COMPILER.md`)

## Tests

```sh
cargo test -p galexy-core -p galexy-abi -p galexy-crypto -p galexy-galf -p gxc   # host suites
cargo test -p runner --test audit_strings   # no "password" in any serial line
cargo test -p runner --test boot -- --test-threads=1   # QEMU suite (87 boots, -smp 2)
```

UEFI cases need `OVMF_FD` if the default firmware path is absent. Disk
and typing cases want `--test-threads=1`; the suite runs under TCG
(no KVM yet — Milestone 64) so a full pass takes a while. Test kernels
live under `crates/galexy-os/src/bin/` (68 today); the runner builds
one image per binary and checks exit codes + serial. There is no CI
workflow yet (Milestone 51).

```sh
cargo build --release
find target -name "galexy-os-*.img"
```

## Repo map

```
docs/           STYLE, ROADMAP, DESIGN, AUTH, GALFS, PROCESS, SCHEDULING, COMPILER, DEMO
TODO.md         milestone checkboxes
crates/
  galexy-abi/     syscall numbers, Cap model, errors (host-tested)
  galexy-core/    pure primitives (host-tested)
  galexy-crypto/  PBKDF2-HMAC-SHA256, ChaCha20, HMAC (host-tested)
  galexy-galf/    GALF on-disk layout + sealed unlock (host-tested)
  galfs-fsck/     host fsck CLI over galfs.img
  galexy-os/      kernel (lib + main + 68 test bins)
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
- **Phase 8** ✅ — mini Rust-subset compiler (`gxc`) for hello — COMPILER.md
- **Phase 9** — hardened (SMEP/SMAP/UMIP/KASLR, hostile ELF), fast
  (bench, KVM, LTO, IRQ completion), modern (q35, ECAM, virtio 1.x,
  MSI-X, TSC-deadline), full userland (heap, argv, clock, channels,
  jobs), init-owned shutdown → `v1.0`
- **Phase 10** — network (virtio-net, small stack, sockets as Caps),
  after `v1.0`

Host-compile the gxr hello (not rustc):

```sh
cargo run -p gxc -- build -o hello-gxc.elf crates/gxc/examples/hello.gxr
```

The runner packs that ELF as ramdisk `hello-gxc` for QEMU
(`test-hellogxc`). Details: [`docs/COMPILER.md`](docs/COMPILER.md).

Details: [`docs/ROADMAP.md`](docs/ROADMAP.md).

## References

- [Writing an OS in Rust](https://os.phil-opp.com/) — structural foundation
- [bootloader](https://github.com/rust-osdev/bootloader) · [rust-osdev](https://github.com/rust-osdev)
- [Cranelift](https://github.com/bytecodealliance/wasmtime/tree/main/cranelift) ·
  [rustc-lite](https://github.com/suhteevah/rustc-lite) — codegen / subset
  ideas for `gxc` (see COMPILER.md)
