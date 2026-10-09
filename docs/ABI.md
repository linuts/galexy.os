# ABI — stability table

`crates/galexy-abi` is the only surface shared between the kernel and
ring 3. This page says which parts of it a user program may depend on.
The crate's doc comments carry the per-call argument layouts; this is
the index. The rules:

- **Stable** — number, register layout, and meaning do not change
  before `v1.0`. Changing one is an ABI major bump and a DESIGN entry.
- **Experimental** — may gain arguments or change return semantics in
  a named milestone. Programs in this repo are updated in the same PR.
- **Template** — exists to prove machinery; removed at ABI 1.0.

Freeze rule (Milestone 52): any change to a stable row needs
`galexy-abi` + this table + the DESIGN syscall section + the
`galexy-rt` / shell wrappers in **one PR**, and `OS_VERSION` or an ABI
version constant bumped alongside. `.github/PULL_REQUEST_TEMPLATE.md`
asks for it.

## Register convention

| Register | Role |
| --- | --- |
| `rax` (in) | syscall number — index into `SYSCALLS` |
| `rdi`, `rsi`, `rdx`, `r8`, `r9`, `r10` (in) | arguments, as each call documents |
| `rax` (out) | payload on success, `SysError` code on failure |
| `rdx` (out) | `1` = ok, `0` = error |
| `rcx`, `r11` | clobbered by `syscall` / `sysret` |

**Stable.** `SyscallResult { ok, value }` is the Rust view of the same
pair. A syscall never faults the caller: a bad buffer is `BadBuffer`,
a bad handle is `BadCap`, and the kernel never panics on an argument.

## Syscall numbers

Numbers are positions in `SYSCALLS` (`galexy-abi` host test asserts no
gaps or duplicates). Appending is allowed; inserting is not.

| # | Call | Status | Note |
| --- | --- | --- | --- |
| 0 | `exit` | stable | |
| 1 | `yield` | stable | |
| 2 | `write` | stable | console budget may return `0` bytes copied |
| 3 | `cap_info` | **template** | removed at ABI 1.0 |
| 4 | `open` | stable | ramdisk and galfs names |
| 5 | `read` | stable | `0` means EOF / no key / empty request per cap kind |
| 6 | `close` | stable | reserved caps are `BadCap` |
| 7 | `spawn` | experimental | single arg blob today; argv/env vector is **Milestone 66** and bumps this row |
| 8 | `power` | stable | |
| 9 | `create` | stable | |
| 10 | `remove` | stable | |
| 11 | `grant` | stable | rights bits in `TOKEN_*` |
| 12 | `revoke` | stable | |
| 13 | `pipe` | stable | 16-byte buffer: read cap then write cap |
| 14 | `give` | stable | files, pipes, and process Caps |
| 15 | `seek` | stable | |
| 16 | `user` | experimental | op-multiplexed; new ops append to `USER_*`, existing ops are stable |
| 17 | `rename` | stable | |
| 18 | `truncate` | stable | |
| 19 | `stat` | stable | buffer layout below |
| 20 | `sync` | stable | |
| 21 | `share` | stable | |
| 22 | `unshare` | stable | |
| 23 | `wait` | experimental | semantics fixed in Milestone 47; status flips to stable when **Milestone 67** closes init-owned shutdown |
| 24 | `kill` | experimental | same as `wait` |
| 25 | `sleep` | stable | Milestone 58 policy freeze |

`MAX_SYSCALL = 25`. The table is capped at 64 entries until there is
an ABI version story.

## Error codes (`SysError`) — stable

| Code | Name | Code | Name |
| --- | --- | --- | --- |
| 1 | `BadCap` | 6 | `NotFound` |
| 2 | `AccessDenied` | 7 | `NoResource` |
| 3 | `BadBuffer` | 8 | `Interrupted` (M58) |
| 4 | `Unsupported` | 9 | `Locked` (M43) |
| 5 | `BadValue` | | |

Unknown codes decode to `Unsupported`. New codes append.

## Handles and layouts

| Item | Layout | Status |
| --- | --- | --- |
| `Cap` | `u64`: bits 0..48 index, bits 48..64 rights snapshot | stable |
| `CapRights` | `u16` mask: READ 1, WRITE 2, SIGNAL 4, WAIT 8, EXEC 16, POWER 32, PROC_WAIT 64, PROC_KILL 128, PROC_TRANSFER 256, PROC_INSPECT 512 | stable (SIGNAL / WAIT are reserved groundwork, unused) |
| Reserved indexes | console 1, self 2, keyboard 0x8000, loader 0x8001, stats 0x8002, tasks 0x8003, threads 0x8004, power 0x8005, files 0x8006, dmesg 0x8007 | stable |
| File cap band | `FILE_CAP_BASE = 3`, eight per task | stable |
| Process cap band | `PROC_CAP_BASE = 0x40`, `MAX_PROC_CAPS = 16` per task | experimental (ceiling may rise with **Milestone 67**) |
| Query snapshots (`stats`, `tasks`, `threads`, `files`, `dmesg`, self) | text, one record per line; field names are informational | **unstable** — parse defensively; field set grows |
| `spawn` grant word (`r10`) | QUERY 1, WAIT 2, INHERIT 4, KEYBOARD 8, WITH_CAPS 16, rights mask bits 8..15, file-slot nibbles bits 16..23 | experimental with `spawn` |
| `spawn` limits | `SPAWN_NAME_MAX = 64`, `SPAWN_ARG_MAX = 256` | experimental with `spawn` |
| Token rights | READ 1, WRITE 2, LIST 4, CREATE 8, REMOVE 16, ONCE 128 (`grant`/`su` only) | stable |
| `seek` whence | SET 0, CUR 1, END 2 | stable |
| `user` ops | WHOAMI 0 … UNLOCK 11 (see crate) | existing ops stable; the op space is open |
| `stat` buffer | 48 bytes: kind u8, rights u8, 2 reserved, size u32, owner len u8, owner 32 bytes, 7 reserved | stable |
| `quota` buffer | 16 bytes: four LE `u32` — objects used, objects max, bytes used, bytes max | stable |
| `power` ops | SHUTDOWN 0, REBOOT 1 | stable |
| `sleep` clamp | `1..=SLEEP_MS_MAX (60 000)` ms | stable |
| `USER_IMAGE_BASE` | `0x0000_0C80_0000_0000`; every ELF links here; the loader accepts PT_LOADs inside `[base, base + 512 MiB)` | stable (no user ASLR — `THREAT.md`) |
| Exit status of a killed task | `137` to Cap-waiters; the cancelled waiter's own syscall returns `Interrupted` | stable (M58) |
| `OS_VERSION` | `"0.1.0"` banner string | informational |

## What is not ABI

- Task names and debug ids seen in `tasks` / `threads` / self
  snapshots. Programs address each other by Cap, never by id.
- Serial markers (`[init] …`, `[sched] reap …`). The runner reads them;
  programs must not.
- The ramdisk tar layout and the GALF on-disk format (`GALFS.md` owns
  that version number; the kernel refuses mismatches).
