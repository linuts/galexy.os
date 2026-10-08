# PERF — budgets and capacities

Order-of-magnitude budgets a reviewer can hold the kernel to, and the
fixed capacities that bound it. Nothing here is a measurement:
**Milestone 64** adds a release profile, a KVM path, and numbers. Until
then the suite runs QEMU TCG at `opt-level = 0`, where a 10 000-iteration
PBKDF2 takes about a second and hashing the 11 MB ramdisk at boot takes
about thirteen (`ramdisk_test_passes` prints the tick count). Treat TCG
timings as upper bounds only.

Explicit non-goal (`THREAT.md`): desktop-class throughput. The target
is *does not fall over when exercised*, not *fast*.

## Budgets (targets)

| Thing | Budget class | Why that class | Checked by |
| --- | --- | --- | --- |
| Syscall round trip, no I/O (`yield`, `cap_info`, `write` of a short line) | **microseconds** on KVM / release; ≤ 1 ms under TCG debug | the IF=0 path allocates nothing and takes at most the `THREADS` lock; one `swapgs` pair and a stack switch | `syscall_test_passes` (correctness); M64 adds a counter |
| galfs mutate (`create`, `write`, `remove`, `rename`) on the RAM table | **tens of microseconds** class | fixed tables, no heap, no disk | `galfs_test_passes` |
| galfs mutate with a disk | **milliseconds** class: one AEAD over the 288-sector slot plus a flush | every mutate publishes a sealed slot; the cost is the seal, not the data | `galfs_disk_*`, `shell_query_typing_e2e` (`sync`) |
| galfs mutate rate on QEMU (TCG, disk) | **≥ 10 / s** sustained; the suite's capacity smoke does hundreds per boot | slot seal dominates; PIO IDE and the virtio 10 M-spin poll are M64 items | `blocks_test_passes`, `quota_test_passes`, `indirect_test_passes` |
| Console output | **512 bytes per task per tick**; excess returns success with `0` copied | one flooding task cannot starve a seat or hide audit lines | `pathological_test_passes` (3 000-call flood: admitted ≤ 512 × (ticks + 2), never an error), `shell_util_typing_e2e` (`linger`) |
| Keyboard | ring of 64 keys per TTY; overflow drops the newest and counts | typing never blocks an IRQ | `audit_console_test_passes` |
| Spawn (ELF validate + map + first schedule) | **single-digit ms** class under TCG for the ~100 KiB userspace images | page-by-page map of PT_LOADs; no copy of the ramdisk | `treechurn_test_passes` (many spawn/exit cycles), `soak_test_passes` (ten spawn/wait/exit rounds with exact table closure), `smpstress_test_passes` |
| Login (PBKDF2 10 000 iterations) | **≈ 1 s** TCG debug; milliseconds on KVM / release | deliberate; M63 stores cost per actor so production formats higher | `users_test_passes`, `lockout_test_passes` |
| Boot to login prompt | **≤ 5 s** TCG debug without disk; + unlock prompt with a sealed disk | the runner's boot tests fail on a 60 s timeout, so this has headroom | every boot test |
| Idle | CPU halts with the LAPIC armed to the next second; no periodic tick | tickless idle keeps `timer_ticks` honest under TCG | `main_kernel_boots_and_timer_ticks`, `idle_test_passes`, `fairness_test_passes` (an idle AP wakes and steals) |

## Capacities (fixed tables)

The kernel preallocates everything the IF=0 path touches. Hitting a
ceiling is `NoResource`, never a panic.

| Resource | Cap | Where |
| --- | --- | --- |
| Thread slots (kernel threads + user tasks, including seats) | 64 | `sched::MAX_THREADS` |
| CPUs | `MAX_CPUS = 8` per-CPU slots; the suite and `cargo run` boot `-smp 2` | `arch/cpu` |
| TTYs / seats | 12 (F1–F12) | `console` |
| Open file caps per task | 8 (`FILE_CAP_BASE..+8`) | `sched::MAX_OPEN_FILES` |
| Process caps per task | 16 | `galexy_abi::MAX_PROC_CAPS` |
| Pipes system-wide / bytes per pipe | 8 / 256 | `sched::pipe` |
| galfs tokens per task | 8 | `GALFS.md` → Table limits |
| galfs actors / objects / shares | 32 / 128 / 32 | `GALFS.md` |
| galfs block pool / max file | 256 × 512 B / 32 KiB | `GALFS.md` |
| Default actor quota | 16 objects, 16 KiB | `GALFS.md` |
| Path | 8 components × 64 bytes; actor names 32 | `galexy_core::path` |
| Spawn name / arg blob | 64 / 256 bytes | `galexy_abi` |
| User stack | 4 pages + guard | `DESIGN.md` → Memory policy |
| User image window | 512 MiB above `USER_IMAGE_BASE` | `loader::USER_IMAGE_WINDOW` |
| `sleep` | 1 ms … 60 s per call | `galexy_abi::SLEEP_MS_MAX` |
| dmesg ring | fixed line ring; `read` returns the newest lines that fit | `DESIGN.md` → console |

## What is not budgeted

- **Wall-clock accuracy.** There is no RTC read; `timer_ticks` is
  monotonic and PIT-calibrated at boot (`SCHEDULING.md`).
- **Disk bandwidth.** ATA is PIO and virtio-blk polls; both are
  Milestone 64/65 work. Correctness under `cache=none` / `writeback` /
  `writethrough` is tested; throughput is not.
- **Memory.** Free frames are reported by `stats`; a soft reserve
  refuses user spawns before the kernel heap is starved
  (`DESIGN.md` → Memory policy). There is no OOM killer because there
  is no overcommit.

## How Milestone 64 will measure

A `bin/test-bench` kernel printing `[bench] <name> <ticks>` lines for
each budget row, run under both TCG and `-enable-kvm` by the runner,
with the numbers pasted into this page per commit. Until that lands,
anything in this document that is a number is a design intent.
