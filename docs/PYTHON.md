# Python — RustPython at the login shell

Phase 14. Milestones **91–96**. Required before the `v1.0` tag.
It can land beside Phase 10 and Phase 13. It does not wait on a
lane, and a lane does not wait on it. Checkboxes live in `TODO.md`.
Nothing in this phase is built yet.

Galexy already runs upstream `rustc` in ring 3. The same kind of
tenant is the Python interpreter: upstream [RustPython](https://github.com/RustPython/RustPython),
built on the host for `x86_64-unknown-galexy`, and started from a
login seat. The command is `python`. CPython is not imported.

The tag gate is a typed login on the Python image:

```text
echo 'print("hello from python on galexy")' > hello.py
python hello.py
```

The line prints, and the prompt comes back. A second program reads
one galfs file and writes another. That is the whole requirement.

## What a program is

`python` is a ramdisk program on `galexy-os-python`, the same shape
as `rustc` on `galexy-os-rustc`. A normal `cargo run` does not carry
it. The shell already passes argv and Cap-waits for a ramdisk name
with arguments. This phase does not add a syscall, a reserved Cap,
or an init row.

| Word | What it is |
| --- | --- |
| **Interpreter** | One static RustPython ELF. The stdlib it ships is frozen into that ELF. |
| **Program** | A galfs file of Python source. The seat named it. The interpreter holds that seat's tokens and no others. |
| **Allow-list** | The modules Milestone 92 freezes. A name that is not on it raises `ImportError`. The process stays up. |
| **Image** | `galexy-os-python`. Built only when the ELF exists. CI does not fetch RustPython. |

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
| A REPL | No keyboard grant. `python` with no file prints a usage line and exits. An editor is a later milestone. |
| A new syscall or a Python service | The ABI freeze holds. Init does not supervise the interpreter. The seat does, for one run. |
| Putting `python` on the shared ramdisk | The compiler image already showed why. `cargo run` stays the small image. |
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
| Guest RAM | 2 GiB, the current QEMU `-m`. A larger number is a Milestone 93 edit of this row and of `PERF.md`, after a boot shows the fault. |
| Stack | The normal user stack. A raise cites the fault, the way `rustc`'s 17 MiB stack did. |
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
compares it to the build. Importing a name that is not listed
is `ImportError` on the host harness for that ELF (a small
driver, not a guest boot). `time.time` is specified as above,
even if the guest test of the unset date waits for Milestone 94.

### 93 Ring 3

`bin/test-python` boots the ELF. A fixed program prints
`hello from python on galexy` on the console and exits 0.
A failed import does not panic the kernel. If the process
faults for stack or heap, this milestone writes the new
budget in the table above and in `PERF.md`, then retries.
`DISK_VERSION` and the target JSON stay put.

### 94 Files

The same test kernel runs a second program. It reads a galfs
file the task holds a token for, writes a second file, and
exits 0. A path with no token raises a Python exception and
exits non-zero. The kernel does not panic. `time.time` with
the date unset raises. After `Date` is set, `time.time` agrees
with that civil time, and `time.monotonic` still moves like
`Clock`.

### 95 Image

`galexy-os-python` is the normal kernel plus a ramdisk that
contains `python`. `cargo run --release -- --python` boots it.
`cargo run` stays the small image. The runner skips the image
when the ELF was absent at build, the same way it skips
`galexy-os-rustc`. CI does not download RustPython.

`python` with no arguments prints a usage line and returns to
the prompt. It does not draw a REPL.

### 96 Run it — Phase 14 gate

Typed login on `galexy-os-python` (TCG). The seat writes
`hello.py` with `echo`, runs `python hello.py`, and sees
`hello from python on galexy`. It then runs a program that
reads one file and writes another. The prompt returns after
each command. `tasks` shows no child of that run.

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

## Files this phase will touch

When the milestones start, not in this plan:

| Piece | Where |
| --- | --- |
| Pin and patch | `scripts/rustpython.rev`, `scripts/galexy-rustpython.patch` |
| Allow-list | `scripts/python-allow.txt` |
| Build | `scripts/build-galexy-python.sh` |
| Image | the runner, beside `galexy-os-rustc` |
| Tests | `bin/test-python`, `python_hello_typing_e2e` |

No kernel file has to change for the gate. A stack or heap budget
that does change is Milestone 93, written down first.
