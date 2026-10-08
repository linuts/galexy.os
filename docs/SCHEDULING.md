# SCHEDULING — runtime, time, and wait

galexy.os schedules **tasks** (kernel threads and ring-3 programs) on a
per-CPU round-robin rotation. Process identity and supervision are Caps
(`docs/PROCESS.md`); this document is the **frozen v1 runtime policy**:
when a task runs, when it sleeps, and how it blocks.

| Layer | Mechanism | Answers |
| --- | --- | --- |
| Placement | Pin-at-spawn + idle steal | Which CPU owns this slot? |
| Dispatch | Per-CPU RR rotation + preempt quantum | Who runs next? |
| Time | Monotonic `timer_ticks` + LAPIC one-shot | When does the timer fire? |
| Wait | Park / wake (child, sleep, keyboard, pipe) | Why is this slot not runnable? |

Checkboxes: `TODO.md` Phase 7 (Milestones **56–58** ✅). Style:
`docs/STYLE.md` → Scheduling. Wiring detail and lock audit:
`docs/DESIGN.md` → sched / APIC. Process Caps / init: `docs/PROCESS.md`.

## Why this is not CFS

Linux fair scheduling, POSIX `nice`, and realtime classes assume a
different ABI and a different trust story. Galexy keeps a small, reviewable
policy:

- **Round-robin** among runnable slots on a CPU
- **Pin at spawn** (round-robin over online CPUs)
- **Idle-pass steal** only (busy CPUs do not migrate work away)
- **One preempt quantum** per online CPU (`quantum_ms() == online()`)

We do **not** claim POSIX time or priority APIs.

## Pieces

| Piece | Meaning |
| --- | --- |
| Slot | Index in the global `THREADS` table; tombstones, never compacted |
| Pin / owner | CPU whose rotation may run the slot |
| Runnable | Eligible for the owner's scan (not waiting, not tombstoned) |
| Waiting | Parked until an event (child load/exit, sleep, keyboard, pipe) |
| Quantum | One-shot LAPIC window while the CPU is busy (`online()` ms) |
| Idle stretch | One-shot until the next whole second (status bar / uptime) |
| Steal cooldown | 100 ticks after a steal before another idle CPU may take it |

Cooperative state-machine tasks still exist for early demos; the live
system is the preemptive thread/user-task rotation.

## What is shipped

### Preempt and SMP (Phase 2, M18–19)

- Naked timer handler swaps full CPU context (GPRs + FXSAVE).
- Per-CPU rotation (`CPU_SCHED`); `THREADS` is the cross-CPU lock.
- Owner reaps; stealer only flips ownership when the victim's context is
  stable (naked-tail flag).
- Keyboard / framebuffer / cooperative shell remain BSP-only by design.

### Tickless idle MVP (M43 / M48)

- LAPIC timer is **deadline one-shot**, not a free-running 1 kHz period.
- Busy path: IRQ re-arms a preempt quantum.
- Idle path: `arm_timer_for_load` stretches to the next whole second
  (or an earlier device IRQ wakes the CPU).
- `timer_ticks` advances by the **armed window** so uptime stays honest
  under TCG when idle stretches.

### Park / wake

- Spawn waiter: park until child load (and optionally exit via
  `SPAWN_WAIT`).
- Cap-wait: park until the Cap'd child exits.
- Cap-kill wakes Cap-waiters with exit status `137`; a kill of a
  sleep / keyboard / pipe waiter clears the park with
  `SysError::Interrupted` before the slot goes `EXITED`.
- **`sleep(ms)`** (Milestone 56): park until monotonic `timer_ticks`
  reaches a deadline; no Cap. Idle LAPIC arm is
  `min(next second, next sleeper)`. Busy IRQ path still re-arms a
  preempt quantum.
- **Keyboard / pipe block** (Milestone 57): empty keyboard `read` and
  empty/full pipe `read`/`write` park in the same `WAITING` state;
  keyboard IRQ and peer pipe activity (or close → EOF / Closed) wake
  and complete the syscall into the waiter's buffer. Archive and galfs
  file I/O stay non-blocking (short read / short write) — no silent
  spin.

## Time model

| Clock | Source | Use |
| --- | --- | --- |
| Monotonic | `timer_ticks()` (≈ 1 ms units) | Auth cool-downs, sleep, uptime, audits |
| Wall clock | Optional / waived | Not required for scheduling |

Rules reviewers need:

1. Monotonic time advances by the armed LAPIC duration on each timer IRQ.
2. Busy CPUs stay quantum-paced; idle CPUs may sleep up to 1 s.
3. Device IRQs (keyboard, …) wake a `hlt` early; the next idle pass
   re-arms.
4. Sleep deadlines shorten the idle arm to
   `min(next_sleeper, next_second)` (clamped by `IDLE_MAX_MS`).
5. TSC-deadline mode is optional if one-shot drift ever matters; until
   then, PIT-calibrated one-shot is the story.

## Target shape (Phase 7 — shipped)

```text
busy CPU  → arm quantum; rotate RR
idle CPU  → arm min(next sleeper, next second); hlt
sleeper   → park until deadline
reader    → park until keyboard/pipe data (or peer close)
waiter    → park until Cap event
```

### Milestone 56 — Time & deadlines ✅

- `sleep` on monotonic ticks (`SLEEP_MS_MAX` = 60_000)
- Sleep deadlines on the thread; timer / idle arm wakes due sleepers
- Program-next-deadline arming (sleepers can beat the 1 s idle stretch)

### Milestone 57 — Block & wake ✅

- Unified `STATE_WAITING` shared by sleep, Cap-wait, I/O
- Blocking keyboard `read` and pipe read/write
- Kill of a parked sleep/I/O waiter stamps `Interrupted` then EXITED
  (Cap-wait sees `137`)

### Milestone 58 — Policy freeze ✅

- Numbers and non-goals cited from code (below)
- ABI for `Sleep` + `SysError::Interrupted` marked **stable**
- Lock-order / IRQ-gate cross-check with Milestone 48 (sched rules here;
  full kernel lock table remains an M48 checkbox)

## Frozen policy (v1)

| Rule | Value | Code |
| --- | --- | --- |
| Dispatch | RR over runnable owned slots; main is slot 0 | `sched/mod.rs` rotation scan |
| Placement | RR pin at spawn across `online()` CPUs | spawn path pin assignment |
| Migration | Idle-pass steal only | idle scan + `CTX_STABLE` |
| Steal cooldown | **100** ticks | `STEAL_COOLDOWN_TICKS` |
| Quantum | **`online()` ms** (min 1) | `apic::quantum_ms` |
| Idle max | **1000** ms | `apic::IDLE_MAX_MS` |
| Max live slots | **64** | `MAX_THREADS` |
| Soft spawn reserve | **64** free frames | `SPAWN_FRAME_RESERVE` |
| Sleep clamp | **1..=60_000** ms | `galexy_abi::SLEEP_MS_MAX` |
| Busy tickless | **No** | busy IRQ re-arms `quantum_ms()` |
| Priorities | **None** | — |
| Affinity ABI | **None** | pin is kernel policy |

Changing a frozen number is a milestone (or an explicit DESIGN waiver),
not a drive-by constant tweak.

## ABI (stable — Milestone 58)

| Surface | Status | Notes |
| --- | --- | --- |
| `Syscall::Sleep` | **Stable** | No Cap; `RDI` ms clamped to `1..=SLEEP_MS_MAX` |
| `SLEEP_MS_MAX` | **Stable** | `60_000` |
| `SysError::Interrupted` (= 8) | **Stable** | Kill cancelled a sleep / keyboard / pipe wait before EXITED |
| Cap-wait / Cap-kill | Experimental | Phase 6 init / seat freeze (`PROCESS.md`) |

Blocking `read`/`write` behavior (park vs short count) is kernel policy
documented here and in DESIGN — not a new syscall number.

## Sched lock / IRQ rules (cross-check with M48)

These are the **scheduler-specific** concurrency rules. The full kernel
lock-order table remains Milestone 48; do not invent a second story here.

1. **`THREADS` is THE cross-CPU sched lock** (`spin::Mutex`). Spawn, reap,
   park/wake, steal ownership flip, and Cap-wait/kill serialize on it.
2. **IRQ gate**: every public path that takes `THREADS` (or other
   preemptable locks used from sched) runs under `without_interrupts` —
   same M48 / DESIGN lock-audit rule (gate in the API, not at call sites).
3. **Naked switch holds no lock** across the RSP swap; stealer waits for
   `CTX_STABLE` before flipping ownership.
4. **Wake from IRQ** (keyboard): `wake_keyboard_waiters` takes `THREADS`
   under IF=0; completes into the waiter's tree via `with_table` (phys
   map) — does not free stacks while `WAITING`.
5. **Pipe wake**: `wake_pipe_waiters` takes `THREADS`; never call it while
   already holding `THREADS` (close path wakes after drop).
6. **Nesting used today**: `THREADS` → pipe table / galfs table on some
   paths (grant, file I/O). Do not reverse that order. Shootdown handlers
   take **no** locks (M48 / DESIGN).
7. **Zombie IF=1**: after `thread_exit`, the park loop stays
   interrupts-enabled (DESIGN zombie rule).

## Auth and process interaction

| Concern | Doc |
| --- | --- |
| Who may wait/kill a task | `PROCESS.md` (process Cap) |
| Login cool-down / idle logout clock | monotonic `timer_ticks` (lockout landed, M43; idle logout still open) |
| Seat restart / Cap-wait supervision | `PROCESS.md` Phase 6; wake primitives here |
| galfs tokens on a task | `AUTH.md` / `GALFS.md` |

Holding a process Cap does not change scheduling priority. Sleep needs
no Cap and no galfs card.

## Explicit non-goals (v1)

- POSIX `nanosleep` / `clock_gettime` / `timer_create` surface
- CFS, MLFQ, weighted fair queueing
- POSIX `nice` / realtime priority classes
- Per-task CPU affinity Caps (unless a later phase adds them)
- Tickless busy (stretching deadlines under CPU-bound load)
- Hard realtime latency claims
- Claiming Linux scheduler compatibility
- Cooperative `run()` sweep fairness nits (waived unless a bug shows;
  same as M48 note)

## Milestone map

| Milestone | Delivers |
| --- | --- |
| Phase 2 / M9–10 | Cooperative + preemptive RR; lock-audit rule |
| M18–19 | Per-CPU rotation, owner reap, idle steal, shootdown |
| M43 / M48 | Tickless idle MVP (one-shot quantum / next-second) |
| **56** ✅ | `sleep`, sleep queues, program-next-deadline arming |
| **57** ✅ | General block/wake (keyboard/pipe); supervisor hygiene |
| **58** ✅ | Policy + numbers + ABI freeze; reviewer one-pager |

Phase 6 (init / seats) can land in parallel; it consumes Cap-wait and
prefers event wake over busy-poll. Scheduling “complete” means
Milestones **56–58** are checked off — not that every future fairness
experiment is forbidden forever.
