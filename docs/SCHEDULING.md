# SCHEDULING — runtime, time, and wait

galexy.os schedules **tasks** (kernel threads and ring-3 programs) on a
per-CPU round-robin rotation. Process identity and supervision are Caps
(`docs/PROCESS.md`); this document is the **runtime** plan: when a task
runs, when it sleeps, and how it blocks.

| Layer | Mechanism | Answers |
| --- | --- | --- |
| Placement | Pin-at-spawn + idle steal | Which CPU owns this slot? |
| Dispatch | Per-CPU RR rotation + preempt quantum | Who runs next? |
| Time | Monotonic `timer_ticks` + LAPIC one-shot | When does the timer fire? |
| Wait | Park / wake (child wait today; general later) | Why is this slot not runnable? |

This document is the plan. Checkboxes live in `TODO.md` (Phase 7 —
Milestones **56–58**). Style rules: `docs/STYLE.md` → Scheduling.
Wiring detail and lock audit: `docs/DESIGN.md` → sched / APIC.
Process Caps / init: `docs/PROCESS.md`.

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
| Waiting | Parked until an event (child load/exit today; sleep/I/O in Phase 7) |
| Quantum | One-shot LAPIC window while the CPU is busy (~`online()` ms) |
| Idle stretch | One-shot until the next whole second (status bar / uptime) |
| Steal cooldown | ~100 ticks after a steal before another idle CPU may take it |

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

### Park / wake today

- Spawn waiter: park until child load (and optionally exit via
  `SPAWN_WAIT`).
- Cap-wait: park until the Cap'd child exits.
- Cap-kill wakes waiters with a defined exit status.
- **`sleep(ms)`** (Milestone 56): park until monotonic `timer_ticks`
  reaches a deadline; no Cap. Idle LAPIC arm is
  `min(next second, next sleeper)`. Busy IRQ path still re-arms a
  preempt quantum. Keyboard/pipe block is Milestone 57.

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
4. Sleep queues (Milestone 56) shorten the next arm to
   `min(quantum, next_sleeper, next_second)` when that is useful.
5. TSC-deadline mode is optional if one-shot drift ever matters; until
   then, PIT-calibrated one-shot is the story.

## Target shape (Phase 7)

```text
busy CPU  → arm quantum; rotate RR
idle CPU  → arm min(next sleeper, next second); hlt
sleeper   → park on sleep queue until deadline
reader    → park until keyboard/pipe data (or peer close)
waiter    → park until Cap event (already shipped)
```

### Milestone 56 — Time & deadlines

- `sleep` / `yield_until` on monotonic ticks
- Ordered sleep queue; timer IRQ moves due sleepers to runnable
- Program-next-deadline arming (sleepers can beat the 1 s idle stretch)
- Reviewer time-model paragraph frozen here (M43 draft may land early)

### Milestone 57 — Block & wake

- Unified “not runnable until event” state shared by sleep, Cap-wait, I/O
- Blocking keyboard `read` and pipe read/write
- No silent busy-spin in demos; kill/Ctrl-C paths unblock with a defined
  error
- Supplies wake primitives Phase 6 init supervision will prefer over
  name-polling

### Milestone 58 — Policy freeze

- Numbers and non-goals written next to the code they describe
- ABI for sleep / wake errors marked stable or explicitly experimental
- Lock-order / IRQ-gate cross-check with Milestone 48

## Policy (v1, to freeze in M58)

| Rule | Value |
| --- | --- |
| Dispatch | Round-robin over runnable owned slots; main is always slot 0 |
| Placement | Round-robin pin at spawn across `online()` CPUs |
| Migration | Idle-pass steal only; ~100-tick cooldown (`STEAL_COOLDOWN_TICKS`) |
| Quantum | `online()` ms share-split (`apic::quantum_ms`) |
| Idle max | 1000 ms (`IDLE_MAX_MS`) — next whole-second boundary |
| Busy tickless | **No** — CPU-bound work stays quantum-paced |
| Priorities | **None** — no nice, no realtime classes |
| Affinity ABI | **None** — pin is kernel policy |

Exact constants must be cited from code when Milestone 58 checks off.

## Auth and process interaction

| Concern | Doc |
| --- | --- |
| Who may wait/kill a task | `PROCESS.md` (process Cap) |
| Login cool-down / idle logout clock | monotonic time (this doc + M43) |
| Seat restart / Cap-wait supervision | `PROCESS.md` Phase 6; wake primitives here |
| galfs tokens on a task | `AUTH.md` / `GALFS.md` |

Holding a process Cap does not change scheduling priority. Sleep does not
need a galfs card; Cap rights for sleep (none vs a trivial Time Cap) freeze
with Milestone 56 in `galexy-abi` + DESIGN.

## Explicit non-goals

- POSIX `nanosleep` / `clock_gettime` / `timer_create` surface
- CFS, MLFQ, weighted fair queueing
- POSIX `nice` / realtime priority classes
- Per-task CPU affinity Caps (unless a later phase adds them)
- Tickless busy (stretching deadlines under CPU-bound load)
- Hard realtime latency claims
- Claiming Linux scheduler compatibility

## Milestone map

| Milestone | Delivers |
| --- | --- |
| Phase 2 / M9–10 | Cooperative + preemptive RR; lock-audit rule |
| M18–19 | Per-CPU rotation, owner reap, idle steal, shootdown |
| M43 / M48 | Tickless idle MVP (one-shot quantum / next-second) |
| **56** | `sleep`, sleep queues, program-next-deadline arming |
| **57** | General block/wake (keyboard/pipe); supervisor hygiene |
| **58** | Policy + numbers + ABI freeze; reviewer one-pager |

Phase 6 (init / seats) can land in parallel; it consumes Cap-wait and,
after M57, prefers event wake over busy-poll. Scheduling “complete” means
Milestones **56–58** are checked off with a green suite — not that every
future fairness experiment is forbidden forever.
