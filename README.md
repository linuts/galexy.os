# Galexy.OS

A small **capability-based** operating system in Rust. Boots BIOS and UEFI,
runs on two CPUs, and has a real ring-3 userland: login seats, a shell,
utilities, and a disk-backed filesystem (galfs).

Not a Linux clone. No POSIX claim. Authority is **capabilities** (and
galfs access cards), not global file descriptors or PIDs.

| You want… | Read |
| --- | --- |
| Passwords, login, access cards | [`docs/AUTH.md`](docs/AUTH.md) |
| galfs trees, tokens, sealed disk | [`docs/GALFS.md`](docs/GALFS.md) |
| Process Caps, init, seats (plan) | [`docs/PROCESS.md`](docs/PROCESS.md) |
| How the kernel is wired | [`docs/DESIGN.md`](docs/DESIGN.md) |
| Coding rules | [`docs/STYLE.md`](docs/STYLE.md) |
| What’s next | [`docs/ROADMAP.md`](docs/ROADMAP.md) · [`TODO.md`](TODO.md) |

## Try it

**Needs:** Linux, QEMU, Rust nightly (from `rust-toolchain.toml`), and for
UEFI an OVMF image.

```sh
# Arch example
sudo pacman -S --needed qemu-desktop ovmf

cargo run                 # BIOS (default); COM1 on your terminal
cargo run -- --uefi       # UEFI — set OVMF_FD if the default path is missing
```

`cargo run` attaches a persistent `galfs.img` (IDE slave) next to the
runner so users and files survive reboot. Override with
`GALEXY_GALFS_IMG=/path/to/img`.

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

Fresh format password is **`admin`** / **`admin`**. After login the
prompt is `admin@galexy> `.

| Keys / command | What it does |
| --- | --- |
| F1–F12 | Switch consoles (each seat has its own login) |
| `help` | Commands |
| `ls` / `echo hi` / `mkdir box` | Files under your tree |
| `whoami` / `users` / `useradd` | Identity |
| `grant` / `revoke` / `su` | Access cards (see AUTH.md) |
| `logout` | Back to the login screen |
| `hello` | Sample user program (keeps running) |
| `shutdown` / `reboot` | Power (admin) |

Typing `shell` is refused — seats are F-keys, not programs you spawn.

## What it is (short)

- **SMP kernel** — two CPUs, per-CPU timers, work-stealing idle, TLB shootdown
- **Ring-3 userland** — ELF from a tar ramdisk, per-task address spaces, SYSCALL ABI in `galexy-abi`
- **Capabilities day one** — keyboard, console, loader, files, power, queries; no fd table
- **galfs** — multi-user tree on disk (GALF image), tokens as access cards
- **Auth** — every seat boots logged out; password login; `logout` clears the session
- **Shell supervisor** — a dead seat is reloaded (userspace init is planned — PROCESS.md)

## Tests

```sh
cargo test -p galexy-core    # host unit tests
cargo test -p galexy-abi     # ABI / capability model
cargo test -p runner --test boot   # full QEMU suite (~47 boots, -smp 2)
```

UEFI cases need `OVMF_FD` if the default firmware path is absent. Test
kernels live under `crates/galexy-os/src/bin/`; the runner builds one
image per binary and checks exit codes + serial.

```sh
cargo build --release
find target -name "galexy-os-*.img"
```

## Repo map

```
docs/           STYLE, ROADMAP, DESIGN, AUTH, GALFS, PROCESS
TODO.md         milestone checkboxes
crates/
  galexy-abi/     syscall numbers, Cap model, errors (host-tested)
  galexy-core/    pure primitives (host-tested)
  galexy-crypto/  PBKDF2-HMAC-SHA256 password KDF (host-tested)
  galexy-os/      kernel (lib + main + test bins)
  userspace/      galexy-rt, shell, util, hello
  runner/         image build, `cargo run`, QEMU boot tests
```

## Design stance

1. **Capabilities over ambient authority** — knowing a name or number is
   not enough; you need a Cap or a galfs card.
2. **Spawn, not fork** — new tasks are loaded ELFs with attenuated rights.
3. **Process Caps next** — wait/kill by Cap (not PIDs); see PROCESS.md.
4. **Test what can be tested** — host tests for pure code; one QEMU boot
   per integration kernel; suite stays green on every milestone.
5. **Modules stay swappable** — screen, serial, keyboard behind narrow APIs;
   `main.rs` is wiring only.

## Where we’re going

- **Phase 5** — review readiness (auth hardening, sealed disk, galfs, solid
  kernel/shell edges) → `review-rc1`
- **Phase 6** — userspace init, seats under init, job Caps

Details: [`docs/ROADMAP.md`](docs/ROADMAP.md).

## References

- [Writing an OS in Rust](https://os.phil-opp.com/) — structural foundation
- [bootloader](https://github.com/rust-osdev/bootloader) · [rust-osdev](https://github.com/rust-osdev)
