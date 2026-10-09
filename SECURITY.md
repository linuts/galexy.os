# Security policy

Galexy.OS is a research operating system under review (`review-rc1`
bar, see `docs/ROADMAP.md`). It is not deployed anywhere; there is no
fleet to patch. This file says how to report a problem and what counts
as one.

## Reporting

Use GitHub's private vulnerability reporting on this repository
(**Security → Report a vulnerability**). If that is unavailable, open a
regular issue titled `security: <one line>` **without** the exploit
details and a maintainer will move the conversation to a private
channel.

Include:

- the commit (`git rev-parse HEAD`) and the toolchain (`rustc -V`, pinned
  by `rust-toolchain.toml`);
- how you ran it (`cargo run`, `cargo run -- --uefi`, or a runner test
  name) and the QEMU command line if you changed it;
- the serial log (`/tmp/galexy-serial-*.log` from the runner, or your
  terminal for `cargo run`) with any passwords you typed removed — the
  kernel never prints them, and `audit_strings` asserts that;
- a `bin/test-*` kernel or a typing e2e that reproduces it, when you
  have one. A failing test is the fastest path to a fix.

There is no bug bounty. Reports are acknowledged within a week and
fixed in a milestone PR with a changelog line.

## Scope

`docs/THREAT.md` is the authority. In short:

**In scope**

- A ring-3 program that crashes, hangs, or corrupts the kernel, another
  task, or galfs — the kernel promises `SysError`, never a panic, for
  any syscall argument.
- Reading or writing another actor's files without a token, share, or
  `su` card that names them (`docs/AUTH.md`).
- Recovering file bytes or password hashes from a raw `galfs.img`
  (sealed GALF, `docs/GALFS.md`).
- Passwords or key material in serial, console, `dmesg`, or a reaped
  page (`docs/STYLE.md` → Secrets and passwords).
- Login lockout, idle logout, or must-change being bypassable.
- A pre-login seat or a bare (`SPAWN` without inherit) program reaching
  galfs or the loader.

**Out of scope** (stated non-goals; see `THREAT.md` for the rationale)

- Physical access to the console, cold-boot RAM extraction, a
  compromised bootloader or firmware, or a hostile hypervisor.
- A hostile ramdisk: the tar is trusted input, measured (SHA-256 at
  build and boot) but not signed.
- KPTI, IBRS, MDS, and CET (waived for this single-tenant guest;
  `THREAT.md` → CPU features). SMEP, SMAP, UMIP, and kernel KASLR are
  on. Actor password cost is stored per actor; the volume KEK stays
  at 10 000 iterations.
- Anything behind a network: there is no network stack.
- Denial of service by the operator at the keyboard (the console budget
  and quotas are fairness, not security, mechanisms).

## Supported versions

Only `master` (and the most recent `review-rc` tag once one exists) is
supported. Older commits are history.
