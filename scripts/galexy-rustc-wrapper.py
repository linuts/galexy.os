#!/usr/bin/env python3
"""`RUSTC_WRAPPER` that points rustc at the Milestone 68 std overlay.

Cargo discovers `std` sources with `rustc --print sysroot`. Passing
`--sysroot` here makes that print, and the compile, use the overlay
from `galexy-std-sysroot.py`. An existing `--sysroot` from cargo is
dropped so the overlay wins. Host rlibs live in the overlay via
symlinks, so build scripts still link.
"""

import os
import sys


def without_sysroot(args: list[str]) -> list[str]:
    out = []
    skip = False
    for arg in args:
        if skip:
            skip = False
            continue
        if arg == "--sysroot":
            skip = True
            continue
        if arg.startswith("--sysroot="):
            continue
        out.append(arg)
    return out


def main() -> None:
    if len(sys.argv) < 2:
        sys.exit("galexy-rustc-wrapper: missing rustc path")
    real = sys.argv[1]
    rest = without_sysroot(sys.argv[2:])
    sysroot = os.environ.get("GALEXY_SYSROOT")
    # `cargo clippy` chains wrappers as `wrapper clippy-driver rustc args`.
    # clippy-driver only treats the following argument as rustc when it is
    # the rustc path, so `--sysroot` has to come after that path.
    if (
        sysroot
        and rest
        and os.path.basename(rest[0]) in {"rustc", "rustc.exe"}
    ):
        args = [real, rest[0], "--sysroot", sysroot, *rest[1:]]
    elif sysroot:
        args = [real, "--sysroot", sysroot, *rest]
    else:
        args = [real, *rest]
    os.execv(real, args)


if __name__ == "__main__":
    main()
