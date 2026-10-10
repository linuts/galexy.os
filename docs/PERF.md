# PERF — budgets and capacities

Measured budgets and the fixed capacities that bound the kernel.
`bin/test-bench` prints `[bench] name=<id> us=<n>` for five operations.
The runner asserts the KVM ceilings below only when it actually boots
with `-accel kvm` (`GALEXY_ACCEL=tcg` skips them; TCG swings with host
load). Desktop-class throughput is an explicit non-goal (`THREAT.md`):
the target is *does not fall over when exercised*.

## Measured (release, TCG)

`cargo test -p runner --test boot --release` image,
`GALEXY_ACCEL=tcg`, `-cpu max`, `-smp 2`. Guest TSC calibrated over a
20 ms halt against `timer_ticks`. There is no KVM column from the
landing host. `/dev/kvm` opens and `KVM_CREATE_VM` succeeds, then
`KVM_CREATE_VCPU` faults: `dmesg` shows `kernel BUG at
arch/x86/kvm/x86.c:702` (`kvm_spurious_fault`) on `VMCLEAR` inside
`alloc_loaded_vmcs`. `CR4.VMXE` is set because the kernel ran `VMXON`
at boot (`kvm.enable_virt_at_load=Y`), and the snapshot this VM was
restored from does not keep that VMX state. The guest is never
entered. The runner probes `KVM_CREATE_VCPU` in a child process and
passes `-accel kvm` only when that child exits 0. `GALEXY_ACCEL=kvm`
on this host fails immediately. Re-run
`GALEXY_ACCEL=kvm cargo test -p runner --test boot --release -- --exact bench_test_passes`
on a host where the probe succeeds (a cold boot of the same kernel
can), and replace the KVM cells.

| Bench | What it times | TCG release (µs) at `7f7b416` | KVM ceiling the runner asserts (µs) |
| --- | --- | --- | --- |
| `yield` | ring-3 `yield` × 10 000 (r13 counter; rcx does not survive `SYSCALL`) | 21 804 022 | 2 000 000 |
| `spawn` | one user spawn + exit, from `rdtsc` around `spawn_user_task` until the name is gone | 11 369 | 2 000 000 |
| `pipe` | 4 KiB copied in 256-byte chunks through one pipe, in-kernel `pipe::write` / `read` (syscall cost is the yield row) | 3 525 | 2 000 000 |
| `galfs` | 32 KiB append of a static buffer + `sync_explicit`. On a RAM volume sync returns immediately; the number is the append | 3 849 | 5 000 000 |
| `repaint` | `repaint_shown` after the shown cells are filled with `M` (the fill is not timed) | 281 066 | 5 000 000 |

Yield on TCG is about 2.2 ms per call. A `dev` profile run on the same
host landed within a few percent (21 423 122 µs), so the cost is the
emulator's syscall exit, not missing `-C opt-level`. The KVM ceilings
are loose upper bounds until a real KVM run replaces them; they exist
so a working KVM host fails a regression that jumps into seconds.

## Waived after the numbers

- **PCID.** A `yield` does not write CR3. The same task returns through
  `sysret`. No PCID work.
- **A new heap allocator.** The yield loop does not allocate. The
  linked-list heap stays.

## Budgets (targets)

| Thing | Budget class | Why that class | Checked by |
| --- | --- | --- | --- |
| Syscall round trip, no I/O (`yield`) | TCG release ≈ 2.2 ms per call for 10 000; KVM ceiling 2 s for the whole loop | the IF=0 path allocates nothing and takes at most the `THREADS` lock | `bench_test_passes` |
| galfs mutate (`create`, `write`, `remove`, `rename`) on the RAM table | **tens of microseconds** class on real hardware; TCG append of 32 KiB was 3.8 ms | fixed tables, no heap, no disk | `bench_test_passes` (`galfs`), `galfs_test_passes` |
| galfs mutate with a disk | **milliseconds** class: one AEAD over the 288-sector slot plus a flush | every mutate publishes a sealed slot; the cost is the seal, not the data. virtio completion is INTx (`IO_BLOCK`), not a 10 M-spin | `galfs_disk_*`, `shell_query_typing_e2e` (`sync`) |
| galfs mutate rate on QEMU (TCG, disk) | **≥ 10 / s** sustained; the suite's capacity smoke does hundreds per boot | slot seal dominates; ATA PIO remains the legacy fallback | `blocks_test_passes`, `quota_test_passes`, `indirect_test_passes` |
| Console output | **512 bytes per task per tick**; excess returns success with `0` copied | one flooding task cannot starve a seat or hide audit lines | `pathological_test_passes` (3 000-call flood: admitted ≤ 512 × (ticks + 2), never an error), `shell_util_typing_e2e` (`linger`) |
| Keyboard | ring of 64 keys per TTY; overflow drops the newest and counts | typing never blocks an IRQ | `audit_console_test_passes` |
| Spawn (ELF validate + map + first schedule) | TCG release one spawn+exit ≈ 11 ms for a tiny blob; userspace images are larger | page-by-page map of PT_LOADs; no copy of the ramdisk | `bench_test_passes` (`spawn`), `treechurn_test_passes`, `soak_test_passes` |
| Login (PBKDF2) | **≈ 1 s** TCG debug at 10 000 iterations; a `--release` kernel under KVM stores 100 000 | deliberate; the count is per actor (GALF v12) | `users_test_passes`, `lockout_test_passes` |
| Boot to login prompt | **≤ 5 s** TCG debug without disk; + unlock prompt with a sealed disk | the runner's boot tests fail on a 60 s timeout, so this has headroom | every boot test |
| Idle | CPU halts with the LAPIC armed to the next second; no periodic tick | tickless idle keeps `timer_ticks` honest under TCG | `main_kernel_boots_and_timer_ticks`, `idle_test_passes`, `fairness_test_passes` (an idle AP wakes and steals) |
| Full-screen repaint | TCG release ≈ 281 ms for one paint of the shown grid | `show_tty` paints only rows whose cells changed; scroll copies pixel rows and clears the vacated row | `bench_test_passes` (`repaint`) |

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
| User heap (`Map`) | 32 pages / 128 KiB per task | `galexy_abi::USER_HEAP_PAGES` |
| Channels | 8 system-wide; one message of 256 bytes and two file Caps | `sched::channel` |
| `sleep` | 1 ms … 60 s per call | `galexy_abi::SLEEP_MS_MAX` |
| dmesg ring | fixed line ring; `read` returns the newest lines that fit | `DESIGN.md` → console |

## What is not budgeted

- **Wall-clock accuracy.** There is no RTC read; `timer_ticks` is
  monotonic. The rate comes from a measured HPET (or PIT) window,
  cross-checked against CPUID 0x15 / 0x16 (`SCHEDULING.md`).
- **Disk bandwidth.** ATA is PIO. virtio-blk completes on MSI-X (INTx
  if the function has no MSI-X table). Correctness under `cache=none` /
  `writeback` / `writethrough` is tested; throughput is not.
- **Memory.** Free frames are reported by `stats`; a soft reserve
  refuses user spawns before the kernel heap is starved
  (`DESIGN.md` → Memory policy). There is no OOM killer because there
  is no overcommit.

## How to re-measure

```sh
GALEXY_ACCEL=tcg cargo test -p runner --test boot --release -- --exact bench_test_passes
GALEXY_ACCEL=kvm cargo test -p runner --test boot --release -- --exact bench_test_passes
```

Paste the five `[bench]` lines into the table above with the commit.
The KVM run is the one that must stay under the ceilings in
`crates/runner/tests/boot.rs` (`BENCH_*_US`).
