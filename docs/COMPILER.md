# COMPILER — `gxc` is removed

Phase 8 (Milestones 59–61) shipped `gxc`, a host compiler for a frozen
Rust subset. It emitted one hello object, and Milestone 69 linked that
object through `gxld`. Milestone 62 (on-OS `gxc`) was already superseded
by Phase 11.

Milestone 73 removes the crate. The on-OS compiler is upstream
Cranelift `rustc` (`docs/RUSTC.md`). The linker is `gxld`
(`docs/LINKER.md`). There is no gxr language and no `hello-gxc` ramdisk
entry.
