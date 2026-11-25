# Python — RustPython at the login shell

Phase 14. Milestones **91–96**. Landed. It was required before the
`v1.0` tag, and that requirement is met. The tag still waits on
a KVM pass. This phase did not wait on a
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
download RustPython. The shell passes argv and Cap-waits. `python` with no file is the
REPL, and `python FILE` is a program: both take the keyboard when
they are in the foreground, so `input()` reads the seat.
`std::io::stdin` cooks that into a line. A pipe replaces it.
This phase does not add a syscall, a reserved Cap, or an init row.

| Word | What it is |
| --- | --- |
| **Interpreter** | One static RustPython ELF. The stdlib it ships is frozen into that ELF. |
| **Program** | A galfs file of Python source. The seat named it. The interpreter holds that seat's tokens and no others. |
| **Allow-list** | The modules Milestone 92 freezes. A name that is not on it raises `ImportError` and the run exits non-zero. The kernel stays up. |
| **Image** | `galexy-os-desk`, what `cargo run` boots when `python` or `rustc` was built. CI does not fetch RustPython. |

The shell path for `python hello.py` is the existing one: inherit
the session, the query grant, the loader grant every ramdisk label
gets, the keyboard, and a Cap-wait. The interpreter build does not call `Spawn`.
The REPL reads that line with `stdin.read_line`. Rebuild
`cargo xtask sysroot` before `scripts/build-galexy-python.sh` so the
linked `libstd` is the cooked one. An already-built `python` keeps
its previous std until that pair of host builds is run again.
Milestone 96 checks that the run did not leave a child task.

## What this refuses

| Refused | Why |
| --- | --- |
| CPython, `libpython`, and C extensions | A second language runtime, and C in a Rust kernel tree. The interpreter is RustPython. |
| Copying the RustPython tree into this repository | It is MIT or Apache-2.0. The pin stays a host checkout, the way `rustc` does. A patch, if one is required, is `scripts/galexy-rustpython.patch`. The license notices stay with the checkout. |
| `pip`, `venv`, `ensurepip`, and a package index | Same non-goal as on-OS `cargo`. |
| `ssl`, `asyncio`, and a Berkeley socket | `socket.connect` dials a name, an address, or `address:port` when a `net dial` rule names the script and that host. The value is a lane (`NET.md`). `socket.listen`, `socket.send`, and `socket.recv` are the listen and dial rules. `socket.bind` is admin. Port 53 stays refused. `setsockopt` stays `OSError`. `ssl` and `asyncio` stay off the allow-list. TLS stays inside `net`. |
| `sqlite3`, `ctypes`, `cffi` | Those need C. |
| `os.fork`, `os.exec`, `os.system`, `os.environ`, `subprocess`, `multiprocessing` | There is no environment block and no fork. Spawn stays the kernel's, and this program does not call it. |
| A second editor | `edit` is already the editor. The REPL reads the seat keyboard and does not draw a screen of its own. |
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
| `io` | `io.open`. The raw file is `_io.FileIO` over `std::fs`. Text mode is the buffered wrapper. |
| `lane` | `lane.read(name)` dials a name when a `net dial` rule names the script and that host, and returns the body. Milestone 103. |
| `socket` | `socket.connect` is that same body, for a name, an address, or `address:port`. `socket.listen`, `socket.send`, and `socket.recv` are the listen and dial rules. `socket.bind` is admin. `setsockopt` and `socket()` are `OSError`. |

Other pure modules may be added in that same milestone when they
compile with the refused features off. Each one is a line in the
file. An import of anything else is `ImportError`.

`open` is `_io.open`. `import io` is that same module: the frozen
tree has no separate `io.py`. A text `open` is a `TextIOWrapper`
over a `BufferedReader` or `BufferedWriter` over `_io.FileIO`.
`FileIO` reads and writes bytes through `std::fs`, which is galfs.
There is no file descriptor, so `fileno` and `opener` raise.
`closefd=False` raises. A missing token is `OSError`. Text mode
is UTF-8. `close`, including the end of a `with`, flushes the
buffer. An unreferenced file is collected at the end of the
statement, and that collection flushes it too.

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
| Syscalls | No new number. `Lane` stays 35. `MAX_PROC_CAPS` stays 16. Phase 14 did not bump `DISK_VERSION`. The live version is 17 after Milestone 99. |

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
this soft-float target. That `cargo +galexy` invocation also passes
`-Zembed-metadata=yes` and `CARGO_INCREMENTAL=0`, the same host-build
settings as `docs/RUSTC.md`. Without them this nightly leaves a
metadata stub in the rlib, and a dependent reports `can't find
crate`. The same script sets `RUST_MIN_STACK` to 35651584 (34 MiB,
twice this nightly's 17 MiB rustc worker stack) when the environment
is smaller, and `CARGO_BUILD_JOBS=1`. A parallel `cargo +galexy` of
this graph SIGSEGVs in `librustc_driver` while compiling
`icu_normalizer`, and the handler truncates the backtrace. The same
compiler, the same `librustc_driver` build id, and this target
compile `icu_normalizer` 2.2.0 when that rustc is the only one.
The doubled stack is the figure that handler prints. One job keeps
`icu_casemap` and `rustpython-unicode` from codegenning beside it.
Finished crates stay. No `cargo clean`. This is the host compiler's
stack. The guest kernel stack is unchanged. The host build does not
pass `--sysroot`
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
uptime. `os` is not on the list. `builtins.open` and `io.open`
are `_io.open`. `_io.FileIO` is the raw file over `std::fs`.
The POSIX `host_env` FileIO stays off: it wants file descriptors,
and this guest has none. A missing token is `OSError`. The guest
test of an unset date is Milestone 94, and it has landed.

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
exits 0. `io.open` in text mode reads the note, and `open` in
text mode writes it back. A binary `io.open` reads those same
bytes. A path with no token raises a Python exception and
exits non-zero. The kernel does not panic. `time.time` with
the date unset raises. After `Date` is set, `time.time` agrees
with that civil time, and `time.monotonic` still moves like
`Clock`.

### 95 Image

`galexy-os-python` is the normal kernel plus a ramdisk that
contains `python`. `cargo run` builds
`target/galexy-python/python` when it is missing, and the same
for `rustc`, then boots `galexy-os-desk` with both. No flag is
required. `GALEXY_SKIP_COMPILERS=1` skips that and boots the
small image. `cargo test` does not build them, so CI does not
download RustPython and the shared test ramdisk stays small.

`python` with no arguments is the REPL (`>>>`). `exit()` returns
to the shell. The proof is `python_repl_typing_e2e`. The small
image prints `python: not in this image` and
`scripts/build-galexy-python.sh`.

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

### 103 A lane

`lane` is on `scripts/python-allow.txt`. It is not a frozen
stdlib module. The interpreter inserts it into `sys.modules`
before the program runs. `lane.read(name)` is dial opcode 2 on
Cap `0x800A` with WRITE, the same payload `fetch` sends: the
name, then `whoami`. The reply's first eight bytes are the lane
Cap. The function reads that lane to EOF and closes it. A missing
grant, a bad name, or a timeout is `OSError`.

There is no new syscall number. `Syscall::Lane` stays 35 and
only `net` may call it. `import socket` is Milestone 109.

Prove: `python_lane_typing_e2e` grants `echo`, runs
`print(lane.read("echo"))`, and expects `lane ok`. That boot
was run. The old `import socket` / `ImportError` line moved to
Milestone 109. The test is skipped when
`target/galexy-python/python` was absent at image build. Rebuild
with `scripts/build-galexy-python.sh` after `cargo xtask sysroot`.
A checkout that does not yet have `fn install_socket` is reset
to the pin and the current patch is applied.

## Phase 16 — A rule, then connect

Milestone **109** adds `socket` to the allow-list, starting with
`socket.connect`. Bind, listen, send, and recv are the same lane.

A compatibility layer keeps the name programs already call.
`socket.connect` dials through `net` when a `net dial` rule names
the caller and that host. The value is still a lane, the same body
`lane.read` returns. There is no side path. Executing a script
runs this interpreter and stamps the script file, so the rule is
on the script, or on the script's folder. A rule on the name
`python` does not cover it. A miss is `OSError` and the
interpreter stays up. There is no yes/no ask, no socket syscall,
and no second stack. This is not a Berkeley socket. Python calls
the same rules (`NET.md`): `socket.bind` (admin, the stored local
address), `socket.listen`, `socket.send`, `socket.recv`, and
`socket.connect` with an address or a port. `setsockopt` stays
refused. `ssl` and `asyncio` stay refused. TLS stays inside `net`.
Reaching a host past `10.0.2.2` is Milestone 111, not this
section. An address is one type for v4 and v6 (Milestones
120–124). There is no second stack and no general browser TLS.

## Changing this subsystem

1. One milestone in `TODO.md` and in this file.
2. A new module is a line in `scripts/python-allow.txt` in the
   same change. A refused module stays refused until a milestone
   names it.
3. `socket.connect`, `socket.bind`, `socket.listen`, `socket.send`,
   and `socket.recv` are the rules in `/policy` (`NET.md`).
   The name stays. A miss is `OSError` and the value, when the
   rule matches, is a lane. A script stamps the script file.
   `setsockopt` stays `OSError`. It does not add a socket syscall.
   `ssl` and `asyncio` stay off the allow-list. A socket operation
   that is enabled is named in `scripts/python-allow.txt` in the
   same change.
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
| `io.open` | `_io.FileIO` in the RustPython patch, over `std::fs` |
| Tests | `bin/test-python`, `python_repl_typing_e2e`, `python_hello_typing_e2e`, `python_lane_typing_e2e` |
| Lane | `import lane` in `scripts/galexy-rustpython.patch` |

The gate did not add a syscall. The stack budget that did change
is Milestone 93: a 17 MiB heap stack, guest RAM still 2 GiB.
