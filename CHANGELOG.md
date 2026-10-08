# Changelog

One line per merged milestone PR, newest first, starting at #62
(the first PR of the Phase 5 review push). Earlier history is the git
log and `TODO.md` Milestones 1–42.

Format: `#PR — Milestone — what landed`. PR numbers link to GitHub.

## Unreleased

- [#78](https://github.com/linuts/galexy.os/pull/78) — M51/M52 — soak and
  fairness evidence: `bin/test-soak` (ten pipe/file/spawn rounds with
  frame, pipe, thread, and galfs-block leak checks), `bin/test-fairness`
  (two-CPU steal fairness), `bin/test-pathological` (console budget
  under a flood of writes), password-paste and no-`crash`-seam shell
  e2e; `pipe::in_use()`; Milestone 52 checklist closed except the
  owner-deferred `review-rc1` tag; intermittent SMP hang tracked for
  Milestone 63
- [#77](https://github.com/linuts/galexy.os/pull/77) — M51 — reviewer
  docs pack: `docs/THREAT.md`, `docs/ABI.md`, `docs/PERF.md`,
  `LICENSE` (MIT), `SECURITY.md`, this changelog, a PR template,
  `scripts/review-smoke.sh`; DESIGN test-seam and coverage tables; AUTH
  and GALFS status refresh; README "For reviewers"
- [#76](https://github.com/linuts/galexy.os/pull/76) — M51 — hardening
  evidence: `loader::validate_elf` + `bin/test-badelf` (hostile ELF
  oracle), `bin/test-negative` (pre-login spawn denied, bare spawn cannot
  touch galfs), `galexy_core::parse_path` with an exhaustive host sweep,
  `galexy_galf::cards` property tests (grant∩ancestor closure, revoke
  exact-object, dual-slot generation monotonicity), ramdisk SHA-256 at
  build and re-measured at boot
- [#75](https://github.com/linuts/galexy.os/pull/75) — M51 — GitHub
  Actions `ci.yml`: `host` job (fmt, host suites, three clippy
  invocations) and `qemu` job (image build, `audit_strings`, full boot
  suite under TCG, serial logs on failure); README "CI" section maps
  each matrix axis to named boot tests
- [#73](https://github.com/linuts/galexy.os/pull/73) — M51 — toolchain
  hygiene: clippy green with `-D warnings` on kernel, host, userspace,
  and runner; one formatting-only rustfmt baseline; nightly pinned to a
  date in `rust-toolchain.toml`
- [#72](https://github.com/linuts/galexy.os/pull/72) — M69 — LINKER.md
  wording: `gxld::validate`, hand-emitted output, stale `gxc::elf`
  references dropped
- [#71](https://github.com/linuts/galexy.os/pull/71) — M69 — `gxld`:
  `no_std` static ELF64 linker library + GNU-ld-compatible CLI;
  `rustc -Clinker=gxld` links every userspace program; the
  `galexy-os-gxld` image passes the same e2e as the `rust-lld` one;
  `gxc` emits a relocatable object and links through `gxld`
- [#70](https://github.com/linuts/galexy.os/pull/70) — docs — `gxc`
  frozen as a fixture; `gxld` planned as Milestone 69; Phase 11
  renumbered
- [#69](https://github.com/linuts/galexy.os/pull/69) — docs — project
  review ("Where we stand" table), Phase 9 Milestones 63–67, Phase 11
  rustc-on-Galexy plan
- [#68](https://github.com/linuts/galexy.os/pull/68) — M50 — shell for
  real demos: `echo hi | cat` pipeline via `pipe` + `give`, one `*`
  glob per word, line editing (arrows, Ctrl-A/E/U), cwd kept across
  login / `su` / failed `cd`
- [#67](https://github.com/linuts/galexy.os/pull/67) — M49 — console,
  audit, and operator UX: blinking cursor, keyboard overflow drop
  counter, auth/grant audit lines without secrets, `dmesg` capability,
  `verbose-sched` steal trace
- [#66](https://github.com/linuts/galexy.os/pull/66) — M48 — memory,
  safety, and concurrency: W^X user maps, stack and secret wipe, fixed
  user maps (demand paging waived), heap policy, FSGSBASE required,
  lock-order table
- [#65](https://github.com/linuts/galexy.os/pull/65) — M45 — galfs crash
  injection (`test-crash`, consistent slot after a torn commit) and ATA
  I/O error propagation (`Unsupported`, never a panic)
- [#64](https://github.com/linuts/galexy.os/pull/64) — M44 — sealed GALF
  finished: interactive volume unlock (`USER_UNLOCK`), key wipe on last
  logout and on power off; wrong passphrase stays RAM-only
- [#63](https://github.com/linuts/galexy.os/pull/63) — M43 — auth
  hardening: PBKDF2-HMAC-SHA256 + CSPRNG salts, no-echo prompts, idle
  logout, kernel-enforced must-change for the default admin password,
  session generation, narrow admin bypass, spawn rights mask
- [#62](https://github.com/linuts/galexy.os/pull/62) — M43 — login
  lockout: five misses lock the actor and the TTY for a cool-down;
  `SysError::Locked` does not check the password
