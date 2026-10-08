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

Bits 6–9 of the rights word; pinned by the abi tests. Experimental
until Milestone 67 freezes them:

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

## Init (Milestone 53 ✅; services and shutdown → Milestone 67)

`crates/userspace/init` today:

- Cap-wait loop on children / orphans; round-robin seat restart
- Fixed seat table (`shell`…`shell12`); no keyboard grant; off the console
- Attenuated caps/tokens per child (`Grants::init` → seats get
  `pre_login`)

Milestone 67 adds: per-entry restart policy (`restart` | `once` |
`ignore` + backoff), non-seat services with `svc` over a capability
channel (Milestone 66), and an ordered shutdown path where the Power
grant lives only on init. Config stays a fixed table; `/etc/init` on
galfs is optional. No systemd/dbus graph.

## Seats and services (Milestone 54)

| Kind | Who spawns | Who keeps the Cap | Crash behavior |
| --- | --- | --- | --- |
| Login seat | init | init | restart → login screen |
| Service | init | init | per-table policy |
| Shell utility | seat/shell | shell (short wait) | n/a ( Cap-wait ) |
| Bare app | seat/shell | shell (optional) | shell decides |

Operator surface: `svc status|start|stop|restart <name>` talks to init
over a **capability-gated** IPC (pipe, control file, or syscall — pick
one in DESIGN when implementing). Operators do not receive raw Caps to
every service by default.

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

v1: one foreground child; background jobs waived.

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
- systemd, dbus, socket activation, cgroups
- Claiming Linux ABI compatibility

## Milestone map

| Milestone | Delivers |
| --- | --- |
| **47** ✅ | Process Cap rights; spawn returns Cap; Cap-wait + exit status; Cap-kill; give/ceiling; ring-3 DS/ES reload; reserved/file forge + soft frame reserve |
| **53** ✅ | Userspace init; orphan Cap transfer; kill-init denied; retire `ensure_shell` policy |
| **54** ✅ | Seats under init; supervise Caps (service table + `svc` moved to 67) |
| **55** ✅ | Session/job Caps; foreground Ctrl-C |
| **66** | Capability channels (`Channel` / `send` / `recv` carrying Caps); background job table in the shell |
| **67** | Ordered shutdown through init; service table with backoff; `svc`; session id on the seat Cap; ABI freeze |

`SPAWN_WAIT` remains a convenience beside Cap-wait. New code must not
dig a deeper PID-shaped API beside this plan.
