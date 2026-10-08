<!-- Title: `area: short imperative summary`, or `Finish Milestone N <summary>.` -->

## What

<!-- One paragraph. Which milestone / TODO boxes does this close? -->

## Test plan

<!-- Paste the commands you ran and what they printed. "CI is green" is
     not a test plan on its own — name the boots that cover the change. -->

- [ ] Host suites: `cargo test -p galexy-core -p galexy-abi -p galexy-crypto -p galexy-galf -p gxc -p gxld`
- [ ] `cargo test -p runner --test audit_strings`
- [ ] Targeted boots: `cargo test -p runner --test boot -- <names> --test-threads=1`
- [ ] Full boot suite (required for a milestone-closing PR, BIOS + UEFI)
- [ ] New QEMU behaviour has a matching `bin/test-*` or typing e2e
- [ ] `cargo fmt --all -- --check` and the three clippy invocations in `.github/workflows/ci.yml`

## STYLE checklist (`docs/STYLE.md`)

Secrets and passwords
- [ ] No cleartext password reaches serial, console, `dmesg`, or an audit line
- [ ] Password material lives in stack buffers and is wiped on every path, including errors
- [ ] Secret comparisons are constant-time (`hash_eq`)

GALF and on-disk formats
- [ ] Any layout change bumps the GALF version and either migrates or refuses old images with a clear serial error
- [ ] Dual-slot commit order kept: write inactive → checksum/AEAD → flush → publish generation
- [ ] No plaintext file bytes or password hashes in a raw image dump

Syscall / interrupt path
- [ ] IF=0 syscall path allocates nothing and takes no lock a timer/IRQ also needs without the documented gate
- [ ] Every user buffer goes through the page walk before copy; no raw user pointer trusted
- [ ] Cap checks are kernel grant ∩ handle snapshot
- [ ] New locks extend the DESIGN lock-order table in this PR

ABI
- [ ] ABI change? Then `galexy-abi` + `docs/ABI.md` table + DESIGN syscall section + `galexy-rt`/shell wrappers all in this PR, and the entry is marked stable or experimental

Docs
- [ ] Code → doc comment → `TODO.md` box → `README.md` → deep doc, in that order
- [ ] `TODO.md` boxes ticked only for behaviour seen in QEMU / a passing test
- [ ] New test seam (feature or `test_*` fn) listed in the DESIGN seam table
- [ ] `CHANGELOG.md` line added for a milestone PR
