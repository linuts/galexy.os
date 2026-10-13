//! When `cargo run` should build the login-image compilers.
//!
//! `cargo test` and CI must not. The ELFs are host builds, and a
//! missing file used to leave `cargo run` on the small image.

/// `skip` is `GALEXY_SKIP_COMPILERS`. `force` is `GALEXY_BUILD_COMPILERS`.
/// `subcommand` is cargo's positional command (`run`, `test`, `build`).
pub fn want_login_compilers(skip: bool, force: bool, subcommand: Option<&str>) -> bool {
    if skip {
        return false;
    }
    force || subcommand == Some("run")
}

/// First positional argument, skipping the program name, `-flags`, and
/// `+toolchain`.
pub fn positional_subcommand<'a>(args: &'a [&'a [u8]]) -> Option<&'a [u8]> {
    args.iter()
        .skip(1)
        .copied()
        .find(|arg| !arg.starts_with(b"-") && !arg.starts_with(b"+"))
}

#[cfg(test)]
mod tests {
    use super::{positional_subcommand, want_login_compilers};

    #[test]
    fn cargo_run_builds_compilers() {
        assert!(want_login_compilers(false, false, Some("run")));
        assert!(!want_login_compilers(false, false, Some("test")));
        assert!(!want_login_compilers(false, false, Some("build")));
        assert!(!want_login_compilers(false, false, None));
    }

    #[test]
    fn skip_wins_and_force_covers_other_commands() {
        assert!(!want_login_compilers(true, false, Some("run")));
        assert!(!want_login_compilers(true, true, Some("run")));
        assert!(want_login_compilers(false, true, Some("test")));
    }

    #[test]
    fn positional_skips_flags_and_toolchain() {
        let args: &[&[u8]] = &[b"cargo", b"+nightly-2026-10-08", b"run", b"--release"];
        assert_eq!(positional_subcommand(args), Some(b"run".as_slice()));
        let test: &[&[u8]] = &[b"cargo", b"test", b"-p", b"runner"];
        assert_eq!(positional_subcommand(test), Some(b"test".as_slice()));
    }
}
