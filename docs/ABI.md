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
| 2 | `write` | stable | console budget may return `0` bytes copied. On a galfs file, bytes land at the cursor (overwrite, then append) and the cursor advances (Milestone 70); a pipe is unchanged |
| 3 | `cap_info` | **template** | removed at ABI 1.0 |
| 4 | `open` | stable | ramdisk and galfs names |
| 5 | `read` | stable | `0` means EOF / no key / empty request per cap kind |
| 6 | `close` | stable | reserved caps are `BadCap` |
| 7 | `spawn` | experimental | A name with no `/` is a ramdisk label. A name with `/` is a galfs path read with the caller's tokens (`/hello`; the shell turns `./hello` at `/` into that path). NUL-separated argv inside the 4096-byte blob; no environment vector. `SPAWN_NO_FG` is r10 bit 5 |
| 8 | `power` | stable | |
| 9 | `create` | stable | |
| 10 | `remove` | stable | |
| 11 | `grant` | stable | rights bits in `TOKEN_*` |
| 12 | `revoke` | stable | |
| 13 | `pipe` | stable | 16-byte buffer: read cap then write cap |
| 14 | `give` | stable | files, pipes, and process Caps |
| 15 | `seek` | stable | moves the file cursor. galfs `write` and `read` both use it (Milestone 70) |
| 16 | `user` | experimental | op-multiplexed; new ops append to `USER_*`, existing ops are stable |
| 17 | `rename` | stable | |
| 18 | `truncate` | stable | |
| 19 | `stat` | stable | buffer layout below |
| 20 | `sync` | stable | |
| 21 | `share` | stable | |
| 22 | `unshare` | stable | |
| 23 | `wait` | stable | `RSI` bit 0 = `WAIT_POLL`: a live child is `NoResource` and does not park. Init's own blocking wait may return `Interrupted` when a control message arrives; the Cap stays installed |
| 24 | `kill` | stable | killing init is `AccessDenied` |
| 25 | `sleep` | stable | Milestone 58 policy freeze |
| 26 | `map` | stable | per-task heap, `USER_HEAP_PAGES` (262_144, 1 GiB) as of Milestone 71 |
| 27 | `clock` | stable | monotonic ms (`timer_ticks`); `sleep` stays stable beside it |
| 28 | `channel` | stable | two endpoint Caps. Init's first channel is the control channel |
| 29 | `send` | stable | one queued message, up to 256 bytes and two file Caps. `send` on init index `0x8008` parks for a reply (`R8`/`R9` = reply buffer/len); the kernel stamps a 20-byte header. Opcodes 1–6 are status, start, stop, restart, shutdown, reboot. Opcodes 7–9 are raise, drop, and every (Milestone 89). Opcodes 10–12 are date, date set, and at (Milestone 90) |
| 30 | `recv` | stable | parks when empty and the peer is open. `R9` bit 0 = `RECV_POLL` returns `NoResource` instead of parking |
| 31 | `thread_spawn` | stable (M70) | `RDI` entry, `RSI` arg, `RDX` stack pages (`0` = 16 pages, at most `THREAD_STACK_PAGES_MAX` = 4352 / 17 MiB as of Milestone 72). Returns a process Cap with `PROC_PARENT` on the shared table; `wait` is join. Same CR3, file table, heap budget. Entry outside the image window is `BadValue` |
| 32 | `thread_exit` | stable (M70) | `RDI` = exit code. The calling thread only. From the process leader this is `exit` |
| 33 | `futex` | stable (M70) | `RDI` = `FUTEX_WAIT` (0) or `FUTEX_WAKE` (1), `RSI` = aligned `u32`, `RDX` = expected or count, `R8` = timeout ms (`0` = none, clamped to `SLEEP_MS_MAX`). Wait returns `0` on wake or on a mismatched word, `TimedOut` if the timeout fires first. Wake returns how many woke. Keyed by address space and address |
| 34 | `date` | stable (M90) | `RDI` = `DATE_READ` (0) or `DATE_SET` (1). Read returns UTC seconds since 1970-01-01 00:00:00, or `NoResource` when unset. Set takes those seconds in `RSI`, admin only (`AccessDenied` otherwise, `BadValue` outside 1970..=2099). No timezone, no leap second. `clock` and `sleep` stay monotonic |

`MAX_SYSCALL = 34`. The table is capped at 64 entries until there is
an ABI version story.

## Error codes (`SysError`) — stable

| Code | Name | Code | Name |
| --- | --- | --- | --- |
| 1 | `BadCap` | 6 | `NotFound` |
| 2 | `AccessDenied` | 7 | `NoResource` |
| 3 | `BadBuffer` | 8 | `Interrupted` (M58) |
| 4 | `Unsupported` | 9 | `Locked` (M43) |
| 5 | `BadValue` | 10 | `TimedOut` (M70) |

Unknown codes decode to `Unsupported`. New codes append.

## Handles and layouts

| Item | Layout | Status |
| --- | --- | --- |
| `Cap` | `u64`: bits 0..48 index, bits 48..64 rights snapshot | stable |
| `CapRights` | `u16` mask: READ 1, WRITE 2, SIGNAL 4, WAIT 8, EXEC 16, POWER 32, PROC_WAIT 64, PROC_KILL 128, PROC_TRANSFER 256, PROC_INSPECT 512 | stable (SIGNAL / WAIT are reserved groundwork, unused) |
| Reserved indexes | console 1, self 2, keyboard 0x8000, loader 0x8001, stats 0x8002, tasks 0x8003, threads 0x8004, power 0x8005, files 0x8006, dmesg 0x8007, init 0x8008 | stable |
| File cap band | `FILE_CAP_BASE = 3`, sixty-four per task (`MAX_OPEN_FILES`) | stable (ceiling raised in Milestone 71) |
| Process cap band | `PROC_CAP_BASE = 0x80`, `MAX_PROC_CAPS = 16` per task | stable as of Milestone 71 (base moved from `0x40` so file caps `3..=66` do not overlap; ceiling stays 16). `SPAWN_WITH_CAPS` nibbles still name slots 0–14 |
| Query snapshots (`stats`, `tasks`, `threads`, `files`, `dmesg`, self) | text, one record per line; field names are informational | **unstable** — parse defensively; field set grows |
| `spawn` grant word (`r10`) | QUERY 1, WAIT 2, INHERIT 4, KEYBOARD 8, WITH_CAPS 16, NO_FG 32, LOADER 64, rights mask bits 8..15, file-slot nibbles bits 16..23 | experimental with `spawn` |
| `spawn` limits | `SPAWN_NAME_MAX = 64`, `SPAWN_ARG_MAX = 4096` | experimental with `spawn` (arg ceiling raised in Milestone 71) |
| Token rights | READ 1, WRITE 2, LIST 4, CREATE 8, REMOVE 16, ONCE 128 (`grant`/`su` only) | stable |
| `seek` whence | SET 0, CUR 1, END 2 | stable |
| `user` ops | WHOAMI 0 … UNLOCK 11 (see crate) | existing ops stable; the op space is open |
| `stat` buffer | 48 bytes: kind u8, rights u8, 2 reserved, size u32, owner len u8, owner 32 bytes, 7 reserved | stable |
| `quota` buffer | 16 bytes: four LE `u32` — objects used, objects max, bytes used, bytes max | stable |
| `power` ops | SHUTDOWN 0, REBOOT 1 | stable |
| `sleep` clamp | `1..=SLEEP_MS_MAX (60 000)` ms | stable |
| `map` | `1..=USER_HEAP_PAGES (262_144)` pages, starting 512 MiB above the task image base | stable (ceiling raised to 1 GiB in Milestone 71) |
| `channel` message | `CHAN_MSG_MAX = 256` bytes, up to two file Caps; 8 channels | stable |
| init RPC header | 20 bytes: admin u8, tty u8, pad 2, debug id u64, session gen u64, then the user payload (`op`, `name_len`, name). User payload max 236. Ops: status 1, start 2, stop 3, restart 4, shutdown 5, reboot 6, raise 7, drop 8, every 9, date 10, date set 11, at 12 | stable |
| Wire Cap | index `0x8009`. Frame read/write. Only the task named `net` | **reserved** — Milestone 78. Not implemented |
| Net control Cap | index `0x800A`. `send` parks for a reply. Same 20-byte stamped header as init. Ops, when they exist: link 1, dial 2, allow 3, grant 4, revoke 5, serve 6. `link` may include our locator after Milestone 80 | **reserved** — Milestone 83. Not implemented |
| `lane` | syscall **35**. Mints a pair of lane Caps. Only `net` may call it. `read` / `write` are the plaintext of one TLS 1.3 session | **reserved** — Milestone 82. Not implemented. No `socket` / `bind` / `connect` row will be added. `date` took 34; the table stays dense |
| USB controller Cap | index `0x800B`. Transfer rings. Only the task named `usb` | **reserved** — Milestone 86. Not implemented |
| USB control Cap | index `0x800C`. `send` parks for a reply. Same 20-byte stamped header as init. Op 1 is `list` | **reserved** — Milestone 87. Not implemented |
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
- Locators and transport ports. They live in the `net` service
  directory (`NET.md`). A program dials a name.
