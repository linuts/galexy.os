//! Whether `cargo +galexy` can actually run.
//!
//! `scripts/build-galexy-python.sh` builds with `cargo +galexy`, the
//! host sysroot from `cargo xtask sysroot`. `rustup toolchain list`
//! still prints that name after the linked directory is gone (a
//! `cargo clean`, or any removal of `target/galexy-toolchain`). The
//! symlink under rustup's toolchains directory is left behind.
//! `cargo +galexy` then exits with `toolchain 'galexy' is not installed`.
//! The name in the list is not a link.

use std::path::Path;

pub const TOOLCHAIN_NAME: &str = "galexy";
/// Same file `cargo xtask sysroot` writes. A directory without it is
/// not ours to relink.
pub const MARKER: &str = "galexy-toolchain.txt";
const TARGET_LIB: &str = "lib/rustlib/x86_64-unknown-galexy/lib";

/// `rustup which --toolchain galexy rustc` succeeded and printed a
/// file. A dangling link makes `which` fail; an empty or missing path
/// is the same as no toolchain.
pub fn which_rustc_usable(which_ok: bool, stdout: &str) -> bool {
    if !which_ok {
        return false;
    }
    let path = stdout.trim();
    !path.is_empty() && Path::new(path).is_file()
}

/// `target/galexy-toolchain` is complete enough to `rustup toolchain link`
/// without building std again: the marker, `bin/rustc`, and at least
/// one target rlib.
pub fn sysroot_dir_ready(dir: &Path) -> bool {
    if !dir.join(MARKER).is_file() || !dir.join("bin/rustc").is_file() {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(dir.join(TARGET_LIB)) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| entry.path().extension().is_some_and(|ext| ext == "rlib"))
}

#[cfg(test)]
mod tests {
    use super::{sysroot_dir_ready, which_rustc_usable};

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "galexy-toolchain-link-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn name_is_the_cargo_plus_toolchain() {
        assert_eq!(super::TOOLCHAIN_NAME, "galexy");
        assert_eq!(super::MARKER, "galexy-toolchain.txt");
    }

    #[test]
    fn which_requires_a_file() {
        assert!(!which_rustc_usable(false, "/tmp/does-not-matter"));
        assert!(!which_rustc_usable(true, ""));
        assert!(!which_rustc_usable(true, "   \n"));
        assert!(!which_rustc_usable(true, "/tmp/galexy-no-such-rustc\n"));

        let dir = scratch("which");
        let rustc = dir.join("rustc");
        std::fs::write(&rustc, b"#!/bin/sh\n").unwrap();
        let listed = format!("{}\n", rustc.display());
        assert!(which_rustc_usable(true, &listed));
        std::fs::remove_file(&rustc).unwrap();
        assert!(!which_rustc_usable(true, &listed));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ready_dir_has_marker_rustc_and_an_rlib() {
        let dir = scratch("ready");
        assert!(!sysroot_dir_ready(&dir));
        std::fs::write(dir.join("galexy-toolchain.txt"), b"marker\n").unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/rustc"), b"#!/bin/sh\n").unwrap();
        assert!(!sysroot_dir_ready(&dir));
        let lib = dir.join("lib/rustlib/x86_64-unknown-galexy/lib");
        std::fs::create_dir_all(&lib).unwrap();
        assert!(!sysroot_dir_ready(&dir));
        std::fs::write(lib.join("libcore-fake.rlib"), b"rlib").unwrap();
        assert!(sysroot_dir_ready(&dir));
        std::fs::remove_file(dir.join("galexy-toolchain.txt")).unwrap();
        assert!(!sysroot_dir_ready(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
