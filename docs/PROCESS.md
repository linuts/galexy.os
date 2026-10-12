# PROCESS — capability process model and init

galexy.os treats **tasks as kernel objects**. You name them with a
**process Cap**, not a global integer from 1970.

| Layer | Mechanism | Answers |
| --- | --- | --- |
| Identity (debug) | Name + optional monotonic debug id | What shows in `tasks` / serial? |
| Authority | Process Cap (`PROC_*` rights) | Who may wait / kill / transfer this task? |
| Supervision | Init + Cap transfer on orphaning | Who reaps and restarts seats/services? |

This document is the plan. Checkboxes live in `TODO.md` (Milestone **47**,
Phase 6 **53–55**). Style rules: `docs/STYLE.md` → Process model and init.
Auth/tokens stay in `docs/AUTH.md`; the galfs tree/token/disk model is
`docs/GALFS.md`. Runtime scheduling (time, block/wake, RR policy) is
`docs/SCHEDULING.md`. Passwords and galfs cards do not replace process Caps.

## Why not PIDs

Unix PIDs are a **guessable global namespace**. `kill(pid)` and
`waitpid(pid)` authorize by knowing a number. That fights the rest of
Galexy (files, galfs, devices are Caps).

We keep:

- **Spawn, not fork** — already true; attenuation at create time
- **Caps as handles** — spawn returns a Cap to the child
- **Debug ids for humans** — listings/logs only; never an open-by-id API

We do **not** claim POSIX. Process Caps are the Galexy ABI.

## Pieces

| Piece | Meaning |
| --- | --- |
| Task | Scheduled ring-3 (or kernel) thread slot with memory, grants, tokens |
| Process Cap | Cap word + rights over a task: wait, kill, transfer, inspect |
| Debug id | Monotonic KOID-style number for `tasks` / serial; **not** a handle |
| Name | Human label (`shell`, `hello`); not the wait/kill key |
| Parent | Kernel parent pointer; orphan Caps move to init |
| Zombie | Exited task kept until a Cap-holder waits/reaps |
| Init | First userspace program; orphan root; seat/service supervisor |
| Seat | Per-TTY login shell (or getty→shell); child of init |
| Job Cap | Group of tasks for a pipeline / foreground (Milestone 55) |

## Cap rights (landed in `galexy-abi`, Milestone 47)

Bits 6–9 of the rights word; pinned by the abi tests. Stable as of
Milestone 67 (`spawn` itself stays experimental):

| Right | Allows |
| --- | --- |
| `PROC_WAIT` | Block until exit; receive exit status; reap zombie |
| `PROC_KILL` | Stop / fault-kill the task (signals-lite) |
| `PROC_TRANSFER` | `give` / move this Cap to another task |
| `PROC_INSPECT` | Read debug id, name, state (`read` on self / child Cap) |

Rights attenuate on transfer. Dropping the last wait Cap without a
reaper is a bug path — orphans must land at init with wait rights.

**No** syscall of the form “here is a debug id, give me a Cap.”

## Spawn and wait

```text
child = spawn(loader, name, args, flags)   → Cap (PROC_WAIT|… as granted)
status = wait(child)                       → exit code; Cap becomes stale
kill(child)                                → needs PROC_KILL
give(peer, child)                          → needs PROC_TRANSFER (rights attenuate)
```

`give` on a process Cap clears the caller's slot and installs an
attenuated Cap on the peer (intersection of table rights and Cap word).

| Before | Now (M47 spawn Cap) |
| --- | --- |
| Wait by child **name** (`SPAWN_WAIT`) | Wait by **process Cap** (`wait` / Cap slot) |
| Unique live names as collision control | Names stay labels; Caps + `cap_gen` are unique |
| Shell scrapes names for supervision | Shell holds Caps (`SPAWN_INHERIT` + `wait`) |

`SPAWN_WAIT` remains a convenience (park until exit; Cap still installed).
There is no wait-by-name ABI.

Bare vs utility spawn **galfs** policy is unchanged (`docs/AUTH.md`):
utilities inherit tokens; bare programs get empty tokens. Process Caps
are orthogonal: the parent always receives a Cap to the child unless the
abi explicitly says otherwise.

## Hierarchy and orphans

1. Every task has a parent (kernel or another task).
2. Parent normally holds the authoritative wait Cap from `spawn`.
3. If the parent exits first and **init is live**: wait/control Caps for
   live children **transfer to init** and `parent_slot` becomes init’s
   slot. Without init (tests / early boot), children become kernel roots
   (`parent_slot = 0`) and Caps die with the parent.
4. Init Cap-waits / reaps; restart policy is userspace (Milestone 54).
5. Zombies exist until Cap-wait or Cap drop; the table bound is the ceiling
   (`MAX_PROC_CAPS` = 16 so init can supervise seats).

Init is distinguished by **role** (`is_init` flag / orphan root), not
by a magic “PID 1” in the public ABI. Cap-kill of init is always
`AccessDenied`; init exit panics the kernel.

## Boot evolution

### Today (Milestones 53–54)

1. Kernel loads **`init`** from the ramdisk when present (orphan root).
2. Init spawns F1–F12 seats (`shell`…`shell12`, pre-login grants);
   kernel skips `spawn_all_shells` / `ensure_shell` when init is live.
3. Login screen is in the seat; init has no keyboard grant.
4. Parent exit → Caps transfer to init; seat exit → init Cap-wait restart
   (round-robin v1).
5. Without init on the ramdisk, the old kernel seat path remains.

### Follow-on

1. `svc` / non-seat service table (waived for v1 seats MVP).
2. Shutdown/reboot ordered through init.
3. Session/job Caps (Milestone 55).

## Init (Milestone 53 ✅; services and shutdown ✅ Milestone 67)

`crates/userspace/init`:

- Fixed table, not `/etc/init`. Twelve seats (`restart`), `stamp`
  (`once`, started at boot), `probe` (`restart`, started only by
  `svc start`; a fast exit backs off 0, then 250, 500, 1000, and
  2000 ms), `spare` (`ignore`), `bell` (notice `ring`, `once`),
  `chime` (beat, default 1000 ms, `once`)
- Logs `[init] ready` after autostart spawns have returned. The
  kernel has one pending spawn slot; a spawn during that window is
  `NoResource`
- First `channel` call is the control channel. Init holds both ends.
  Seats `send` on the reserved init Cap (`0x8008`); the kernel stamps
  admin, tty, debug id, and session generation. One RPC is in flight
- `svc status|start|stop|restart`, `svc raise|drop`, `svc every`,
  `svc date`, `svc at`, and `shutdown` / `reboot` are that RPC.
  Operators never receive a
  service Cap. `svc status` answers any logged-in seat; `svc
  start|stop|restart`, `raise`, `drop`, `every`, `shutdown`, and
  `reboot` are admin-only and a non-admin request is refused in the
  reply (`access denied`; `init_test_passes` covers a non-admin
  `stop`, a non-admin `raise`, a notice edge, and a beat). An admin
  shutdown Cap-kills every seat except
  the sender's, syncs, then calls Power. The sender stays parked until
  the machine is off, or hears `the machine stayed up` if Power returns
- Logged-in admin seats do not hold the Power grant while init is
  alive. A no-init shell still does, and `shutdown` falls back to
  Power when the init Cap is `Unsupported`
- Cap-wait is polled (`WAIT_POLL`) so a zombie is reaped before init
  parks on a live seat. A control message cancels init's sleep or
  blocking wait with `Interrupted` and leaves the process Cap in place.
  `wait` and `sleep` also refuse to park when a message is already
  queued, so a send that lands while init is still running is not
  stuck behind a live seat
- No keyboard grant. Log lines are serial-only (init shares TTY 0
  with F1)

## Seats and services (Milestone 54)

| Kind | Who spawns | Who keeps the Cap | Crash behavior |
| --- | --- | --- | --- |
| Login seat | init | init | restart → login screen |
| Service | init | init | per-table policy |
| Shell utility | seat/shell | shell (short wait) | n/a ( Cap-wait ) |
| Bare app | seat/shell | shell (optional) | shell decides |

Locking a seat freezes that shell only. Init, `stamp`, `probe`,
`bell`, `chime`, and the other seats keep running. The volume stays
open. The locked shell cannot `svc` until the password opens it.

Operator surface: `svc status|start|stop|restart <name>`,
`svc raise|drop <notice>`, `svc every <ms> <name>`, `svc date`, and
`svc at <hh:mm> <name>` send on the init Cap. The reply is text
(`shell running`, `probe stopped`, `ring up`, `chime armed`,
`2026-10-12 15:04:00 utc`, `unset`, `chime at`, `unknown`,
`bad value`, `access denied` for a non-admin mutation). Operators
do not receive raw Caps to every service.

## Notices and beats (Milestone 89)

There is no event bus. A **notice** is a named bit init holds: up or
down, no payload, no subscribers outside the service table. The names
are `NOTICE_NAMES` in `crates/userspace/init`. A row's `notice` field
is empty or one of those names. `svc raise ring` is the rising edge:
`bell` starts if it is stopped. Raising a notice that is already up
does not start it again. `svc drop ring` is the falling edge: bound
rows are stopped and held. The next raise is a new edge.

A **beat** is a monotonic interval on a row whose compiled `every_ms`
is non-zero. `Clock` is milliseconds since boot, so there is no
calendar and no `03:00`. `svc every 250 chime` arms `chime`. The first
run is one interval later, not immediately. If that program is still
running, init logs `beat skip` and arms the next interval. One copy.
`svc stop` disarms the beat and kills the program. The armed interval
is forgotten on reboot. The compiled default (1000 ms for `chime`) is
the only number that survives. A row with `every_ms == 0`, including
every seat, answers `bad value`.

Intervals are `250..=86_400_000` milliseconds. A beat row is `once`,
so an exit does not restart it; the interval is the only starter.
Both commands are admin-only, on the same stamped RPC as `svc start`.
A service cannot raise a notice: the kernel admits the RPC from a
logged-in seat, and init refuses a non-admin sender. The table is the
subscription list. Adding a notice or a beat is a rebuild of init,
the same edit as adding a service.

While a beat is armed, init sleeps in slices of at most one second
instead of parking on one child, so the interval can fire. A seat
exit waits for the rest of that slice. With no beat armed, init parks
on `wait` as before. `svc stop` on the beat returns it to that park.

`bell` exits immediately, so a second raise is observable as a single
`[init] start name=bell` line. `chime` sleeps one second, so a 250 ms
arm overlaps and init must skip. `init_test_passes` covers both.

## Civil date (Milestone 90)

`Clock` stays milliseconds since boot. `Date` is a second clock: UTC
seconds since 1970-01-01 00:00:00. There is no timezone and no leap
second. A day is 86400 seconds. Years outside 1970..=2099 are refused.
Unset is a real state (`NoResource`), not the epoch.

The CMOS clock seeds the anchor at boot when the chip holds a year in
2000..=2099. A zeroed chip stays unset. An admin set writes the
anchor and the chip. A read adds elapsed monotonic seconds to that
anchor, so it does not touch the ports, and setting the date does not
move `Sleep`, lockout, idle logout, or `svc every`.

`svc date` replies `YYYY-MM-DD HH:MM:SS utc` or `unset`. Any logged-in
seat may read. The set form is admin-only and the name is that
19-byte text. `svc at HH:MM <name>` arms one fire per civil UTC day
on a row that can be a beat. The current minute may fire. A
still-running program logs `[init] at skip name=` and is not stacked.
The fire line is `[init] at name=`. `svc stop` disarms both. The
armed civil time is forgotten on reboot. The chip keeps the date.

While a civil beat is armed, init sleeps in slices of at most one
second, the same as a monotonic beat. With neither armed, it parks
on `wait`. If the date is unset, init logs `[init] date unset` once
and does not fire.

F1–F12 **console switching** stays in the kernel; only task lifecycle
moves to init.

## Sessions and jobs (Milestone 55) ✅

- **Session** ≈ login seat (init’s supervise Cap).
- **Job** (v1) ≈ the seat’s foreground child; the process Cap from
  `spawn` is the job Cap (no separate job object yet).
- Kernel tracks per-TTY foreground slot (set on seat spawn drain).
- Ctrl-C kills that foreground task (exit `137`) and is **not** delivered
  into the seat’s keyboard ring; with no foreground, `^C` still cancels
  prompts in the shell.
- Shell Cap-waits the spawned Cap for pipeline status.

v1 was one foreground child. Milestone 66 adds a fixed shell job table
(`cmd &`, `jobs`, `fg`). Ctrl-Z stays waived. Ctrl-C still kills only the
foreground task (exit `137`); a background spawn does not become that
foreground until `fg` Cap-waits it.

## Auth interaction

| Concern | Doc |
| --- | --- |
| Password login / logout / actors | `AUTH.md` |
| galfs tokens on a task | `AUTH.md` |
| Who may wait/kill that task | **this doc** (process Cap) |
| Seat restart / getty | Phase 6 (init holds Caps) |

A logged-in seat still needs loader/query grants to spawn. Holding a
process Cap never grants galfs rights on the child’s files.

## Explicit non-goals

- POSIX/`waitpid` / kill-by-pid ambient namespace
- `fork` + COW address spaces
- `open_process(debug_id)`
- Full POSIX signal set / job-control ioctl zoo
- systemd, dbus, socket activation, cgroups. Notices and beats
  (Milestone 89) and civil UTC (Milestone 90) are the substitute: a
  bit that starts table rows, a monotonic interval, and one minute
  per UTC day. No unit graph, no payload bus, no cron string, no
  timezone
- Claiming Linux ABI compatibility

## Milestone map

| Milestone | Delivers |
| --- | --- |
| **47** ✅ | Process Cap rights; spawn returns Cap; Cap-wait + exit status; Cap-kill; give/ceiling; ring-3 DS/ES reload; reserved/file forge + soft frame reserve |
| **53** ✅ | Userspace init; orphan Cap transfer; kill-init denied; retire `ensure_shell` policy |
| **54** ✅ | Seats under init; supervise Caps (service table + `svc` moved to 67) |
| **55** ✅ | Session/job Caps; foreground Ctrl-C |
| **66** ✅ | Capability channels (below); shell job table (`&`, `jobs`, `fg`) |
| **67** ✅ | Ordered shutdown through init; service table with backoff; `svc` (mutations admin-only); session id on the seat Cap; ABI freeze |
| **89** ✅ | Notices (`svc raise` / `svc drop`) and monotonic beats (`svc every`). No bus, no wall clock |
| **90** ✅ | Civil UTC (`Date`, `svc date`, `svc at`). `Clock` stays monotonic. `Lane` is syscall 35 |

`SPAWN_WAIT` remains a convenience beside Cap-wait. New code must not
dig a deeper PID-shaped API beside this plan.

## Capability channels (Milestone 66)

Channels are a second IPC object beside pipes. A pipe moves bytes. A
channel moves one message that may also carry capabilities.

`Syscall::Channel` matches `Pipe`: the caller passes a 16-byte user
buffer and receives two endpoint Caps, both with READ|WRITE. Either
end may `Send` or `Recv`. The kernel keeps a fixed table (8 channels).
A full table is `NoResource`.

One message is outstanding per channel, not per direction. The message
is at most 256 bytes plus up to two file Caps (not process Caps, and
not either endpoint of this same channel). `Send` copies the bytes and
**moves** the Caps out of the sender immediately. If a message is
already queued, `Send` returns `NoResource` and the Caps stay with the
sender (it does not park). `Recv` on an empty channel parks in
`STATE_WAITING` until a message arrives or the other end is closed
(Milestone 57). A closed peer and an empty queue is end-of-file (`0`).

Caps are delivered at `Recv`, installed in the receiver's file table,
and the new Cap bits are written to a 16-byte user buffer (`0` when
that slot was empty). If the receiver has no free file slot for a Cap
the message is carrying, the message **stays queued** and `Recv`
returns `NoResource` so a later retry can install them. Caps are never
dropped on that path. When both endpoints have closed, a still-queued
message's Caps are released (pipe ends closed, channel ends closed).

`Send` / `Recv` are their own syscall numbers, appended after `Channel`.
`Map`, `Clock`, `Channel`, `Send`, `Recv`, `wait`, and `kill` are
stable as of Milestone 67. `spawn` stays experimental. `Recv` `R9` bit
0 (`RECV_POLL`) returns `NoResource` instead of parking when the queue
is empty. `Send` to the init Cap is the control RPC, not a normal
channel send: it parks until init replies.

`Clock` reads the same monotonic millisecond counter as `Sleep`
(`timer_ticks`). It takes no Cap and does not change `Sleep`.

`Map` grows the calling task's heap: NX|RW|user pages 512 MiB above
that task's image base (ramdisk ELFs: `USER_IMAGE_BASE + 512 MiB`), at
most `USER_HEAP_PAGES` (262_144, 1 GiB; Milestone 71). The return value is the base of the newly mapped pages.
Past the budget is `NoResource`. Reap walks the task's own P4 slot, so
those frames come back with the rest of the tree.
`galexy-rt` bumps an allocator over that region; `dealloc` does not
unmap. `SPAWN_NO_FG` (r10 bit 5) is how the shell starts a background
job without making it the TTY's Ctrl-C target.

## Threads and futexes (Milestone 70)

A process is still one address space and one Cap table. `ThreadSpawn`
adds a thread inside it: a new kernel slot, the same CR3, a fresh user
stack (default 16 pages, at most 4352 / 17 MiB as of Milestone 72) and a fresh kernel stack, `FS`
base 0. The return value is a process Cap with `PROC_PARENT` on the
shared table; `Wait` on it is join and yields the thread's exit code.
The caller parks until the thread is registered — the kernel allocates
on the main loop, not inside the syscall. More than 16 live records in
one address space, or a full kernel thread table, is `NoResource`. An
entry outside the image window, or a stack past the maximum, is
`BadValue`.

`ThreadExit` ends the calling thread only. From the process leader (the
slot the loader created) it is `Exit`: the whole process ends, and
siblings die with it. A leader exit reaps the address space; a sibling
exit stays a joinable record until `Wait`.

`Futex` is the park/wake primitive `std`'s locks use. `FUTEX_WAIT`
parks while the aligned `u32` at `addr` still holds `expected` (a
mismatch returns immediately). `FUTEX_WAKE` wakes up to `count` waiters.
Waiters are keyed by address space and address, so a word is private to
one process. A non-zero timeout (clamped to `SLEEP_MS_MAX`) returns
`TimedOut`. The kernel never writes `FS`; each thread sets its own base
with `wrfsbase` for key-based TLS.

`ThreadSpawn`, `ThreadExit`, and `Futex` are stable as of Milestone 70.
`spawn` stays experimental.
