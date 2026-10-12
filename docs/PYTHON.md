# Python — RustPython at the login shell

Phase 14. Milestones **91–96**. Landed. It was required before the
`v1.0` tag, and that requirement is met. The tag still waits on
Phase 10, Phase 13, and a KVM pass. This phase did not wait on a
lane, and a lane does not wait on it. Checkboxes live in `TODO.md`.

Galexy already runs upstream `rustc` in ring 3. The same kind of
tenant is the Python interpreter: upstream [RustPython](https://github.com/RustPython/RustPython),
built on the host for `x86_64-unknown-galexy`, and started from a
login seat. The command is `python`. CPython is not imported.

The tag gate is a typed login on the image `cargo run` boots:

```text
echo 'print("hello from python on galexy")' > hello.py
python hello.py
```

The line prints, and the prompt comes back. A second program reads
one galfs file and writes another. That is the whole requirement.

## What a program is

`python` is a ramdisk program. `cargo run` carries it, and `rustc`
with its sysroot, whenever those ELFs were built. No flag selects
the image. The test kernels keep the small ramdisk, so CI does not
download RustPython. The shell passes argv and Cap-waits. `python`
with no file also takes the keyboard, because that is the REPL.
This phase does not add a syscall, a reserved Cap, or an init row.

| Word | What it is |
| --- | --- |
| **Interpreter** | One static RustPython ELF. The stdlib it ships is frozen into that ELF. |
| **Program** | A galfs file of Python source. The seat named it. The interpreter holds that seat's tokens and no others. |
| **Allow-list** | The modules Milestone 92 freezes. A name that is not on it raises `ImportError` and the run exits non-zero. The kernel stays up. |
| **Image** | `galexy-os-desk`, what `cargo run` boots when `python` or `rustc` was built. CI does not fetch RustPython. |

The shell path for `python hello.py` is the existing one: inherit
the session, the query grant, the loader grant every ramdisk label
gets, and a Cap-wait. The interpreter build does not call `Spawn`.
Milestone 96 checks that the run did not leave a child task.

## What this refuses

| Refused | Why |
| --- | --- |
| CPython, `libpython`, and C extensions | A second language runtime, and C in a Rust kernel tree. The interpreter is RustPython. |
| Copying the RustPython tree into this repository | It is MIT or Apache-2.0. The pin stays a host checkout, the way `rustc` does. A patch, if one is required, is `scripts/galexy-rustpython.patch`. The license notices stay with the checkout. |
| `pip`, `venv`, `ensurepip`, and a package index | Same non-goal as on-OS `cargo`. |
| `socket`, `ssl`, and `asyncio` networks | `std::net` stays unsupported (`NET.md`). A later milestone may speak a lane. It is not this phase, and it is not a BSD socket with a Python name. |
| `sqlite3`, `ctypes`, `cffi` | Those need C. |
| `os.fork`, `os.exec`, `os.system`, `os.environ`, `subprocess`, `multiprocessing` | There is no environment block and no fork. Spawn stays the kernel's, and this program does not call it. |
| A second editor | `nano` is already the editor. The REPL reads the seat keyboard and does not draw a screen of its own. |
| A new syscall or a Python service | The ABI freeze holds. Init does not supervise the interpreter. The seat does, for one run. |
| Putting `python` on the shared test ramdisk | Test kernels and CI stay small. The login image `cargo run` boots is the one that carries it. |
| Changing `targets/x86_64-unknown-galexy.json` | The spec is frozen. If the build cannot use it, Milestone 91 stops and says why. It does not edit the file. |
| `--sysroot` pointed at this repository | Host builds use `cargo xtask sysroot` and the linked `galexy` toolchain, the same rule as every other `std` program. |

## The allow-list

Milestone 92 writes the list into `scripts/python-allow.txt`, one
module name per line, and the build freezes those modules and no
others. A host test fails when the frozen set and the file disagree.

The gate needs these names, and they are the ones that list starts with:

| Module | What it may do |
| --- | --- |
| `builtins` | The language, including `print` and `open`. |
| `sys` | `argv`, `stdout`, `stderr`, `version`. No environment. |
| `io` | File objects over `std::fs`, which is galfs. |

Other pure modules may be added in that same milestone when they
compile with the refused features off. Each one is a line in the
file. An import of anything else is `ImportError`.

`open` is the file API. If RustPython's `open` insists on an `os`
module, Milestone 92 adds the smallest `os` that `open` calls
(path strings and the flags the PAL already implements) and every
other `os` function raises. `os.fork` is not that surface.

Time stays two clocks, and they are not mixed:

| Call | Clock |
| --- | --- |
| `time.monotonic` | `Clock` (`timer_ticks`) |
| `time.time` | `Date`, civil UTC. When the date is unset, this raises. It does not report uptime as a wall clock. |

The PAL's `SystemTime::now` falls back to uptime when `Date` is
unset. Python does not follow that fallback. The shim is in the
RustPython patch, not a change to `std`.

A path the seat cannot read or write is the existing galfs error,
surfaced as a Python exception. The interpreter does not become
admin because it is the interpreter.

## Budgets

Fixed until a milestone writes a new number here before changing
the runner.

| Budget | Value |
| --- | --- |
| Guest RAM | 2 GiB, the current QEMU `-m`. The fault below did not ask for more. |
| Stack | Leader stays 4 pages. Before it reads the file, the interpreter switches onto a 17 MiB heap buffer (the same size as `THREAD_STACK_PAGES_MAX`). Milestone 93: `py-hello` wrote the unmapped guard under the leader (`err=0x6`, `rip=0xc8001ac8472`, `cr2=0xc803ffffa70`, 1424 bytes below the stack at image+1GiB) and the kernel recorded exit 0. A `thread::Builder` request for 17 MiB came up as 64 pages. |
| Argv | `SPAWN_ARG_MAX` (4 KiB), already the shell's limit. |
| Source file | One galfs file inside the existing file cap. |
| Stdlib | Frozen in the ELF. No `Lib/` directory on the disk. |
| Children | None. The interpreter does not spawn. |
| Syscalls | No new number. `Lane` stays 35. `MAX_PROC_CAPS` stays 16. `DISK_VERSION` stays 12. |

## Milestones

### 91 Pin and link

Pin a RustPython commit in `scripts/rustpython.rev`. Build one
static ELF for `x86_64-unknown-galexy` with the nightly in
`rust-toolchain.toml` (`nightly-2026-10-08`) and the existing
sysroot. Features that need `std::net`, libc, or C stay off.
Link with the host linker the other `std` programs use.

Prove: the ELF is a Galexy executable under the same check
`cargo xtask sysroot --check` uses. It does not have to boot.
The revision and the license of that checkout are recorded in
this file. The checkout itself is not committed.

The pin is `85d5956dbbc17b359b103f9988276589b126a08a` (RustPython
0.6.1, MIT). `scripts/build-galexy-python.sh` clones that commit,
applies `scripts/galexy-rustpython.patch`, and links with the
`galexy` sysroot. Codegen is Cranelift, the same backend as the
sysroot: LLVM 23 cannot legalize an f16 operation in `half` for
this soft-float target. The host build does not pass `--sysroot`
of this repository, and `targets/x86_64-unknown-galexy.json` is
unchanged. There is no C library and no `std::net`. The Rust
`libc` crate inside the sysroot is the same empty one every
other `std` program already links. The ELF lands at
`target/galexy-python/python`. `cargo test -p xtask
python_elf::pinned_python_elf_is_loadable` is the check. CI
skips it when the file is absent.

### 92 Allow-list

`scripts/python-allow.txt` is the frozen set. The host test
compares it to `scripts/python-frozen.txt` and to the checkout's
frozen Python files when the build script runs. Importing a name
that is not listed is `ImportError` (the interpreter installs a
`meta_path` finder before the program). The run prints the
exception and exits non-zero. The kernel stays up. `time.monotonic` is
syscall `Clock` (27). `time.time` is syscall `Date` (34) and
raises `OSError` when the date is unset, not the monotonic
uptime. `os` is not on the list. `_io.FileIO` exists only with
the host_env feature, which this build does not enable, so
`builtins.open` is a small `std::fs` wrapper. `io.open` stays
unsupported. A missing token is `OSError`. The guest test of an
unset date is Milestone 94, and it has landed.

### 93 Ring 3

`bin/test-python` boots the ELF. A fixed program prints
`hello from python on galexy` on the console and exits 0.
A failed import does not panic the kernel. The first boot
faulted on the 4-page leader stack; the budget table and
`PERF.md` record that fault, and the interpreter retries on
a 17 MiB heap stack. `DISK_VERSION` and the target JSON stay put.
Guest RAM stays 2 GiB. A guard-page fault exits 0, so the
boot test also rejects `[pf]`.

### 94 Files

The same test kernel runs a second program. It reads a galfs
file the task holds a token for, writes a second file, and
exits 0. `open` is the `builtins` wrapper from section 92, not
`_io.FileIO`. A path with no token raises a Python exception and
exits non-zero. The kernel does not panic. `time.time` with
the date unset raises. After `Date` is set, `time.time` agrees
with that civil time, and `time.monotonic` still moves like
`Clock`.

### 95 Image

`galexy-os-python` is the normal kernel plus a ramdisk that
contains `python`. `cargo run` boots `galexy-os-desk` instead,
which is that ramdisk plus the `rustc` sysroot when the compiler
ELF was built too. No flag is required. The runner skips the
desk image when neither ELF was present at build. CI does not
download RustPython, and the shared test ramdisk stays small.

`python` with no arguments is the REPL (`>>>`). `exit()` returns
to the shell. The proof is `python_repl_typing_e2e`.

### 96 Run it — Phase 14 gate

Typed login on `galexy-os-desk` (TCG), the image `cargo run`
boots. The seat writes `hello.py` with `echo`, runs
`python hello.py`, and sees `hello from python on galexy`. It
then runs a program that reads one file and writes another.
The prompt returns after each command. `tasks` shows no child
of that run. When `rustc` was built, the same boot has already
imported the sysroot.

That boot is `python_hello_typing_e2e`. It is skipped when
the ELF was absent at image build.

## Changing this subsystem

1. One milestone in `TODO.md` and in this file.
2. A new module is a line in `scripts/python-allow.txt` in the
   same change. A refused module stays refused until a milestone
   names it.
3. A socket-shaped API is still refused. A later milestone that
   wants the network speaks a lane (`NET.md`). It does not enable
   `std::net`.
4. `docs/ABI.md` gains a row only if a number appears. This phase
   has none. `docs/STYLE.md` carries the rules below.
5. The wiki page Python stays in step with this file.
6. A box is checked only after a run shows the behavior.

## Files this phase touched

| Piece | Where |
| --- | --- |
| Pin and patch | `scripts/rustpython.rev`, `scripts/galexy-rustpython.patch` |
| Allow-list | `scripts/python-allow.txt` |
| Build | `scripts/build-galexy-python.sh` |
| Image | `galexy-os-desk` in the runner, beside `galexy-os-rustc` |
| REPL | bare `python` in the shell, with the keyboard grant |
| Tests | `bin/test-python`, `python_repl_typing_e2e`, `python_hello_typing_e2e` |

The gate did not add a syscall. The stack budget that did change
is Milestone 93: a 17 MiB heap stack, guest RAM still 2 GiB.
