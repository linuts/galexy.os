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
`docs/GALFS.md`. Passwords and galfs cards do not replace process Caps.

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

## Cap rights (planned abi)

Exact names freeze in `galexy-abi` with Milestone 47. Intent:

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
give(peer, child)                          → needs PROC_TRANSFER
```

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
3. If the parent exits first, the kernel **transfers** wait/control Caps
   (or equivalent rights) to **init**.
4. Init Cap-waits / reaps; restart policy is userspace (Milestone 53–54).
5. Zombies exist until waited; the table bound is the ceiling.

Init is distinguished by **role** (first ring-3 task / orphan root), not
by a magic “PID 1” in the public ABI.

## Boot evolution

### Today (through review readiness)

1. Kernel starts F1–F12 shells with pre-login grants.
2. `ensure_shell()` reloads a dead seat.
3. Login screen is in the shell; auth per `docs/AUTH.md`.

### Target (Phase 6)

1. Kernel loads **`init`** once from the ramdisk.
2. Init spawns seat children (one per TTY), keeps supervise Caps.
3. Seat shows the login screen; init does not prompt for passwords.
4. Seat crash → init restart policy (replaces `ensure_shell`).
5. Shutdown/reboot is ordered through init (flush, stop services, power).

Milestone 47 lands Caps/wait/kill **without** requiring init yet:
kernel-spawned shells remain until 53–54 cut over.

## Init (Milestone 53)

`crates/userspace/init` (planned):

- Cap-wait loop on children / orphans
- Boot table: which programs to start (seats + optional services)
- Restart policy per entry: `restart` | `once` | `ignore` (+ backoff)
- Attenuated caps/tokens per child — not “all rights because init”
- Shutdown request path from an operator-held right

Config v1: fixed table or a small galfs file (`/etc/init`). No
systemd/dbus graph.

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

## Sessions and jobs (Milestone 55)

- **Session** ≈ login seat (or service root).
- **Job** ≈ foreground pipeline; addressed by a **job Cap**, not an
  ambient process-group id.
- Ctrl-C delivers signals-lite to the TTY’s foreground **job Cap** only.
- Shell waits on the job Cap for pipeline status.

v1: one foreground pipeline; background jobs optional or waived.

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
| **47** | Process Cap rights; spawn returns Cap; Cap-wait + exit status; Cap-kill; debug ids optional; forge tests |
| **53** | Userspace init; orphan Cap transfer; kill-init denied; retire `ensure_shell` policy |
| **54** | Seats under init; service table + `svc`; supervise Caps |
| **55** | Session/job Caps; foreground Ctrl-C |

Until 47 lands, today’s name + `SPAWN_WAIT` + kernel `ensure_shell`
remain the shipped behavior. New code must not dig a deeper PID-shaped
API beside this plan.
