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
| 2 | `write` | stable | console budget may return `0` bytes copied. On a galfs file, bytes land at the cursor (overwrite, then append) and the cursor advances (Milestone 70); a pipe is unchanged. **Experimental** (Milestone 117): `r8` bit 0 = `RECV_POLL`. A pipe or lane that would park returns `NoResource` and does not park. `r8 = 0` parks as before |
| 3 | `cap_info` | **template** | removed at ABI 1.0 |
| 4 | `open` | stable | ramdisk and galfs names |
| 5 | `read` | stable | `0` means EOF / no key / empty request per cap kind. **Experimental** (Milestone 117): `r8` bit 0 = `RECV_POLL`. A pipe or lane that would park returns `NoResource` and does not park. `r8 = 0` parks as before |
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
| 35 | `lane` | experimental (M82) | Only the task named `net` may call it. `RDI` is 16 writable user bytes. On success the kernel writes two Cap words, end 0 then end 1. Everyone else is `AccessDenied`. `read` and `write` on those Caps move plaintext. 32 lanes, 16 KiB each direction. The number does not change shape after Milestone 85 without an entry here |

`MAX_SYSCALL = 35`. The table is capped at 64 entries until there is
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
| Reserved indexes | console 1, self 2, keyboard 0x8000, loader 0x8001, stats 0x8002, tasks 0x8003, threads 0x8004, power 0x8005, files 0x8006, dmesg 0x8007, init 0x8008, wire 0x8009, net 0x800A, USB controller 0x800B, USB control 0x800C | stable through init. Wire and net are experimental (M78, M82): only the task named `net` may read or write the wire. `send` on net index `0x800A` parks for a reply the way init does. The first 8 reply bytes are a lane Cap (0 if none), then the text. Opcodes 1–12 are link, dial, allow, serve, ping, resolve, pin, bind, listen, datagram, datagram receive, and stream. Resolve, pin, bind, listen, datagram, datagram receive, and stream are experimental with this record. Stream is opcode 12: one direction byte (`TOKEN_READ` dials, `TOKEN_WRITE` accepts) and then the name. The program passes no address. An empty pin replies `unseen` and the peer key, with no lane. Direction 3 stores the key `net` just saw, after the seat answers yes. Direction 4 drops it. A stored pin that does not match replies `changed`. The reply lane stays open and `net` copies TLS plaintext both ways. A dial (opcode 2) with `NET_HELD_OUT` and an empty pin is a plaintext lane on that same opcode: the program writes the request and reads the response. A stored pin, and every dial without that bit, stays the one-shot TLS fetch. Listen is still write on the port card. Port parsing rejects a leading zero, port 0, and a port above 65535. DNS-over-TLS stays on port 853. A second acceptor is `held`. This is not a new syscall. Resolve returns an address and no lane. Pin stores the DNS-over-TLS resolver: four address bytes trust the bundle in the image, and a following 32-byte key is that pin, with the bundle not consulted. `10.0.2.2` and `10.0.2.3` are refused. Bind binds the stored local address and does not listen. Listen accepts on the stored port. Datagram sends to the stored peer. Datagram receive reads one datagram from that same peer. Write does not capture. Port 53 is refused. Ping is ICMP echo. Four payload bytes are an IPv4 address. A longer payload is a length-prefixed DNS name, resolved over the pinned DNS-over-TLS resolver, then echoed. A payload whose first byte is 0, then a length-prefixed name, resolves and does not echo, and the reply is one address per line. An address or a name sends three echoes and lists each round-trip time, then sent, received, and those times. The lane Cap is zero. A missing ping card sends no query. Milestones 120–124 plan that address as one type for v4 and v6 on opcodes 1–14, and they add no opcode. Until those boxes are checked, four payload bytes stay an IPv4 address. Logged-out is `AccessDenied`. The kernel stamps the same 20-byte header as init. These opcodes do not change shape after Milestone 85 without an entry here. USB controller `0x800B` is experimental (M86): only the task named `usb` may read it. USB control `0x800C` is experimental (M87): `send` parks for plain text, opcode 1 is list, and the reply does not begin with a lane Cap. Logged-out is `AccessDenied`. The kernel stamps the same 20-byte header |
| File cap band | `FILE_CAP_BASE = 3`, sixty-four per task (`MAX_OPEN_FILES`) | stable (ceiling raised in Milestone 71) |
| Process cap band | `PROC_CAP_BASE = 0x80`, `MAX_PROC_CAPS = 16` per task | stable as of Milestone 71 (base moved from `0x40` so file caps `3..=66` do not overlap; ceiling stays 16). `SPAWN_WITH_CAPS` nibbles still name slots 0–14 |
| Query snapshots (`stats`, `tasks`, `threads`, `files`, `dmesg`, self) | text, one record per line; field names are informational | **unstable** — parse defensively; field set grows |
| `spawn` grant word (`r10`) | QUERY 1, WAIT 2, INHERIT 4, KEYBOARD 8, WITH_CAPS 16, NO_FG 32, LOADER 64, rights mask bits 8..15, file-slot nibbles bits 16..23 | experimental with `spawn` |
| `spawn` limits | `SPAWN_NAME_MAX = 64`, `SPAWN_ARG_MAX = 4096` | experimental with `spawn` (arg ceiling raised in Milestone 71) |
| Token rights | READ 1, WRITE 2, LIST 4, CREATE 8, REMOVE 16, ONCE 128 (`grant`/`su` only) | stable |
| `seek` whence | SET 0, CUR 1, END 2 | stable |
| `user` ops | WHOAMI 0 … VOLPASS 15 (see crate). `USER_UNLOCK` with `R9 = 0` returns 0 open, 1 sealed and locked, 2 no passphrase yet. A fresh volume also takes the admin password in `RDI`/`RSI` and refuses the format default. `USER_VOLPASS` is admin-only: `RDI`/`RSI` current (`RSI = 0` if none yet), `R8`/`R9` new | existing ops stable; the op space is open |
| `stat` buffer | 52 bytes: kind u8, rights u8, 2 reserved, size low u32, owner len u8, owner 32 bytes, 3 reserved, size high u32, object id u32. Files under 4 GiB leave the high half zero. [`ABI_EPOCH`] is 2; [`OS_VERSION`] stays `1.0.0` | stable |
| `quota` buffer | 16 bytes: four LE `u32` — objects used, objects max, bytes used, bytes max | stable |
| `power` ops | SHUTDOWN 0, REBOOT 1 | stable |
| `sleep` clamp | `1..=SLEEP_MS_MAX (60 000)` ms | stable |
| `map` | `1..=USER_HEAP_PAGES (262_144)` pages, starting 512 MiB above the task image base | stable (ceiling raised to 1 GiB in Milestone 71) |
| `channel` message | `CHAN_MSG_MAX = 256` bytes, up to two file Caps; 8 channels | stable |
| init RPC header | 44 bytes: admin u8, tty u8, name length u8, one reserved byte, entry object id u32, parent object id u32, debug id u64, session gen u64, then the task name (16 bytes). The caller cannot set these. `net` and `usb` match them against `/policy`. User payload max 212. Ops: status 1, start 2, stop 3, restart 4, shutdown 5, reboot 6, raise 7, drop 8, every 9, date 10, date set 11, at 12, sec 13. No new syscall number | stable |
| Wire Cap | index `0x8009`. Frame read/write. Only the task named `net`. The kernel moves frames and does not parse IP | **experimental** — Milestone 78 |
| Net control Cap | index `0x800A`. `send` parks for a reply. Same 20-byte stamped header as init. Opcodes 1–14 are the reserved-indexes row: link, dial, allow, grant, revoke, serve, ping, resolve, pin, bind, listen, datagram, datagram receive, and stream. Ping is four address bytes, a length-prefixed DNS name, or a leading 0 and a length-prefixed name that resolves without an echo. Stream is a live lane. Milestones 120–124 plan one address type on these opcodes and add no opcode. Logged-out is `AccessDenied` | **experimental** — Milestones 82, 83, 110–116, and 117. No new syscall. Milestones 120–124 add no syscall |
| `lane` | syscall **35**. Mints a pair of lane Caps. Only `net` may call it. `read` / `write` are the plaintext of one TLS 1.3 session | **experimental** — Milestone 82. No `socket`, `bind`, or `connect` syscall. Milestones 112–116 grant those calls with a card on this net cap. Milestone 117 (SSH) adds no syscall. Opcode 12 on net cap `0x800A` keeps one lane open. `read` and `write` `r8` bit 0 is the experimental poll flag above. `date` took 34; the table stays dense |
| USB controller Cap | index `0x800B`. `read` copies one device record (port, address, flags, 18 device bytes, configuration, raw string descriptor). A `write` of exactly one byte, the port, arms the boot keyboard on that port (Milestone 102). The stored configuration must contain an interface of class 3, subclass 1, protocol 1; anything else is `BadValue`. Only the task named `usb`, and the write needs READ and WRITE | **experimental** — Milestone 86. A record larger than the caller's buffer is `BadValue` and is not consumed |
| USB control Cap | index `0x800C`. `send` parks for a reply. Same 20-byte stamped header as init. Op 1 is `list`. The reply is plain text, one device per line | **experimental** — Milestone 87. Logged-out is `AccessDenied`. The reply does not begin with a lane Cap |
| Python | no syscall and no reserved index. RustPython is a ring-3 program (`docs/PYTHON.md`). `lane.read` and `socket.connect` are dial opcode 2 on net Cap `0x800A` and then `read` of that lane. `socket.bind` is opcode 10, `socket.listen` is opcode 11, `socket.send` is opcode 12, and `socket.recv` is opcode 13. An address or a port is the address card and the port card. `setsockopt`, `ssl`, and `asyncio` are not those calls | landed — Milestones 91–96, 103, and 109. The card calls use opcodes 10–13 |
| `USER_IMAGE_BASE` | `0x0000_0C80_0000_0000`; every ELF links here; the loader accepts PT_LOADs inside `[base, base + 512 MiB)` | stable (no user ASLR — `THREAT.md`) |
| Exit status of a killed task | `137` to Cap-waiters; the cancelled waiter's own syscall returns `Interrupted` | stable (M58) |
| `OS_VERSION` | `"1.0.0"` banner string | informational |

## What is not ABI

- Task names and debug ids seen in `tasks` / `threads` / self
  snapshots. Programs address each other by Cap, never by id. The
  `tasks` text starts with `tasks: N live` and one line per running or
  waiting user program (`id`, `tty`, a wait label, `cpu`, `parent`,
  `name`). That shape can grow.
- Serial markers (`[init] …`, `[sched] reap …`). The runner reads them;
  programs must not.
- The ramdisk tar layout and the GALF on-disk format (`GALFS.md` owns
  that version number; the kernel refuses mismatches).
- Locators and transport ports. They live in the `net` service
  directory (`NET.md`). A program passes one stored address or port
  on net cap `0x800A`, and only with that card. SSH (Milestone
  117) is opcode 12 on that same cap. It adds no syscall.
  Milestones 120–124 are one address type for v4 and v6 on these
  opcodes. They add no opcode and no syscall. A v6-only opcode is
  not the plan.
