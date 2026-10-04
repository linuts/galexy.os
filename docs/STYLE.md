# STYLE — galexy.os conventions

Rules for how code in this repo is written. Reviewers (and future me) should
enforce these.

## Rust idioms

- **`#![no_std]` always** in kernel crates. `alloc` only when a module
  explicitly opts in with its own gating (e.g. `#![feature(alloc)]` in one
  place, reviewed).
- **`#![deny(missing_docs)]` on public kernel modules.** Every public item
  gets a doc comment explaining *hardware behavior*, not just signature.
- **No comments unless asked for in review** — doc comments (`///`) are fine
  and expected; inline `//` only for genuinely non-obvious reasoning.
- Error types: implement `Debug` + a short `Display`; never `.unwrap()` in
  code that can fail at runtime — return `Result` up to `main`, panic only in
  genuinely unrecoverable states.
- Unsafe: every `unsafe` block gets a `// SAFETY:` comment stating the exact
  invariant being relied upon. No bare `unsafe`.

## Module organization

- One module per directory (`arch/`, `drivers/`, `kcore/`, `sched/`, ...),
  `mod.rs` re-exports the public surface only.
- **Layer boundaries are law** (see `docs/DESIGN.md`): `main.rs` is wiring
  only; only `arch/` touches ports/registers; drivers never call drivers;
  `kcore` has no layer dependencies.
- Each module exposes the **narrowest** API it can. Cross-module calls go
  through the owning module's public functions, never through its internals.
- Statics: `spin::Mutex` / `LazyLock` initialization; no global mutable
  `static mut` anywhere, ever.
- `main.rs` is a wiring file: it calls `init()` functions and then loops. All
  logic lives in modules.

## Formatting

- `cargo fmt` is the law; `cargo clippy -- -D warnings` must pass.
- Max line length: whatever rustfmt uses (100 by default).
- Names: modules `snake_case`, types `UpperCamelCase`, kernel-agnostic names
  preferred (`ScreenWriter`, not `VgaTextBufferWriterFactory`).

## Testing

- Pure logic (e.g. scancode → char, line-buffer edit) gets `#[test_case]`
  kernel tests runnable in QEMU.
- Anything touching hardware ports gets verified manually in QEMU; note the
  verification step in the PR/commit message.

## Docs hygiene

- When behavior changes, update in this order: code → doc comment → `TODO.md`
  checkbox → `README.md` feature list. `ROADMAP.md` only for direction shifts.
- `TODO.md` checkboxes are only checked after the item is *verified working*
  (e.g. seen in QEMU), never when "written".

## Git

- Commit message style: `milestone: short imperative summary` (e.g.
  `m2: vga scrolling`).
- Never commit build artifacts; `target/` and `*.img` are gitignored.
