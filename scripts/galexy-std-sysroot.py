#!/usr/bin/env python3
"""Build a sysroot overlay so `-Zbuild-std=std` works for os = "galexy".

nightly-2026-10-08's `std` has no fallback for an OS it has never heard
of: `sys/alloc`, `sys/io/error`, and `sys/random` `cfg_select` with no
default, and the single-thread TLS path is not selected by
`not(target_has_threads)`. Milestone 68 does not fork rust-lang/rust
(that is Milestone 70). This copies the toolchain's `library/` and
applies the unsupported fallbacks `docs/RUSTC.md` describes: a null
allocator (the program's `#[global_allocator]` replaces it), generic
I/O errors, static TLS on single-thread targets, and the address-derived
`HashMap` keys. Host rlibs stay symlinks to the real sysroot.
"""

import hashlib
import os
import shutil
import subprocess
import sys
from pathlib import Path

PATCH_VERSION = "1"


def rustc_sysroot() -> Path:
    rustc = os.environ.get("RUSTC", "rustc")
    out = subprocess.check_output([rustc, "--print", "sysroot"], text=True)
    return Path(out.strip())


def patch_text(path: Path, old: str, new: str) -> None:
    text = path.read_text()
    if old not in text:
        sys.exit(f"galexy-std-sysroot: pattern missing in {path}")
    if text.count(old) != 1:
        sys.exit(f"galexy-std-sysroot: pattern not unique in {path}")
    path.write_text(text.replace(old, new, 1))


def apply_patches(library: Path) -> None:
    alloc = library / "std/src/sys/alloc/mod.rs"
    patch_text(
        alloc,
        """    target_os = "zkvm" => {
        mod zkvm;
        use zkvm as imp;
    }
}
""",
        """    target_os = "zkvm" => {
        mod zkvm;
        use zkvm as imp;
    }
    _ => {
        // Unlisted OS (JSON targets). Returns null; the program's
        // `#[global_allocator]` is what actually serves `Vec` / `String`.
        mod unsupported;
        use unsupported as imp;
    }
}
""",
    )
    (library / "std/src/sys/alloc/unsupported.rs").write_text(
        """use crate::alloc::Layout;
use crate::ptr;

pub unsafe fn alloc(_layout: Layout) -> *mut u8 {
    ptr::null_mut()
}

pub unsafe fn dealloc(_ptr: *mut u8, _layout: Layout) {}

pub unsafe fn realloc(_ptr: *mut u8, _layout: Layout, _new_size: usize) -> *mut u8 {
    ptr::null_mut()
}

pub unsafe fn alloc_zeroed(_layout: Layout) -> *mut u8 {
    ptr::null_mut()
}
"""
    )

    patch_text(
        library / "std/src/sys/io/error/mod.rs",
        """    any(target_os = "vexos", target_family = "wasm", target_os = "zkvm", target_os = "trusty") => {
        mod generic;
        pub use generic::*;
    }
}
""",
        """    any(target_os = "vexos", target_family = "wasm", target_os = "zkvm", target_os = "trusty") => {
        mod generic;
        pub use generic::*;
    }
    _ => {
        mod generic;
        pub use generic::*;
    }
}
""",
    )

    # TLS storage. Single-thread targets use statics, not key-based TLS.
    patch_text(
        library / "std/src/sys/thread_local/mod.rs",
        """    any(
        all(target_family = "wasm", not(target_feature = "atomics"), not(target_env = "p3")),
        target_os = "uefi",
        target_os = "zkvm",
        target_os = "trusty",
        target_os = "vexos",
    ) => {
""",
        """    any(
        not(target_has_threads),
        all(target_family = "wasm", not(target_feature = "atomics"), not(target_env = "p3")),
        target_os = "uefi",
        target_os = "zkvm",
        target_os = "trusty",
        target_os = "vexos",
    ) => {
""",
    )
    # Destructor guard. Same set, plus single-thread targets, or the
    # fallback `guard/key.rs` imports a key module this OS does not have.
    patch_text(
        library / "std/src/sys/thread_local/mod.rs",
        """        any(
            all(target_family = "wasm", not(target_env = "p3")),
            target_os = "uefi",
            target_os = "zkvm",
            target_os = "trusty",
            target_os = "vexos",
        ) => {
""",
        """        any(
            not(target_has_threads),
            all(target_family = "wasm", not(target_env = "p3")),
            target_os = "uefi",
            target_os = "zkvm",
            target_os = "trusty",
            target_os = "vexos",
        ) => {
""",
    )

    patch_text(
        library / "std/src/sys/random/mod.rs",
        """    _ => {}
}
""",
        """    _ => {
        // Unlisted OS: address-derived keys, same as xous / wasm.
        // `fill_bytes` still panics; `HashMap` does not call it.
        mod unsupported;
        pub use unsupported::{fill_bytes, hashmap_random_keys};
    }
}
""",
    )
    patch_text(
        library / "std/src/sys/random/mod.rs",
        """    target_os = "xous",
    target_os = "vexos",
    target_os = "l4re",
)))]
""",
        """    target_os = "xous",
    target_os = "vexos",
    target_os = "l4re",
    // Default arm above already exports hashmap_random_keys.
    target_os = "galexy",
)))]
""",
    )


def main() -> None:
    if len(sys.argv) != 3 or sys.argv[1] != "--out":
        sys.exit("usage: galexy-std-sysroot.py --out DIR")
    out = Path(sys.argv[2]).resolve()
    real = rustc_sysroot()
    real_lib = real / "lib/rustlib/src/rust/library"
    if not real_lib.is_dir():
        sys.exit(f"galexy-std-sysroot: rust-src missing at {real_lib}")

    version = subprocess.check_output(
        [os.environ.get("RUSTC", "rustc"), "--version"], text=True
    ).strip()
    stamp_body = f"{version}\n{PATCH_VERSION}\n{real}\n"
    stamp_body += hashlib.sha256(Path(__file__).read_bytes()).hexdigest() + "\n"
    stamp = out / ".galexy-std-stamp"
    sysroot = out / "sysroot"
    if stamp.is_file() and stamp.read_text() == stamp_body and sysroot.is_dir():
        print(sysroot)
        return

    if out.exists():
        shutil.rmtree(out)
    rustlib = sysroot / "lib/rustlib"
    rustlib.mkdir(parents=True)
    for child in (real / "lib/rustlib").iterdir():
        if child.name == "src":
            continue
        (rustlib / child.name).symlink_to(child)
    library = rustlib / "src/rust/library"
    shutil.copytree(real_lib, library, symlinks=True)
    apply_patches(library)
    stamp.write_text(stamp_body)
    print(sysroot)


if __name__ == "__main__":
    main()
