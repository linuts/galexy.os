//! GNU-ld-style command line, the subset `rustc -Clinker-flavor=ld`
//! emits for a static executable. Flags that only mean something to a
//! dynamic linker are accepted and ignored so `rustc` can drive `gxld`
//! unchanged.

use crate::error::{Error, Result};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// A parsed input file reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputArg {
    /// Path, or a `-l` library name resolved by the caller.
    pub path: String,
    /// `-lfoo` rather than a path.
    pub is_lib: bool,
    /// Inside `--whole-archive … --no-whole-archive`.
    pub whole_archive: bool,
}

/// What the command line asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// `-o`.
    pub output: Option<String>,
    /// Inputs in order.
    pub inputs: Vec<InputArg>,
    /// `-L` search directories.
    pub lib_dirs: Vec<String>,
    /// `--image-base`.
    pub image_base: Option<u64>,
    /// `-e` / `--entry`.
    pub entry: Option<String>,
    /// `--gc-sections` (default) vs `--no-gc-sections`.
    pub gc_sections: bool,
    /// `--version` / `-v`.
    pub show_version: bool,
    /// `--help`.
    pub show_help: bool,
}

/// Parse `argv[1..]`. `@file` expansion is the caller's job (it needs I/O).
pub fn parse<S: AsRef<str>>(argv: &[S]) -> Result<Args> {
    let mut args = Args {
        output: None,
        inputs: Vec::new(),
        lib_dirs: Vec::new(),
        image_base: None,
        entry: None,
        gc_sections: true,
        show_version: false,
        show_help: false,
    };
    let mut whole = false;
    let mut i = 0;
    let take_value = |i: &mut usize, flag: &str| -> Result<String> {
        *i += 1;
        argv.get(*i)
            .map(|s| s.as_ref().to_string())
            .ok_or_else(|| Error::Args(format!("{flag} needs a value")))
    };
    while i < argv.len() {
        let a = argv[i].as_ref();
        match a {
            "-o" => args.output = Some(take_value(&mut i, "-o")?),
            "-e" | "--entry" => args.entry = Some(take_value(&mut i, a)?),
            "-L" => args.lib_dirs.push(take_value(&mut i, "-L")?),
            "-l" => args.inputs.push(InputArg {
                path: take_value(&mut i, "-l")?,
                is_lib: true,
                whole_archive: whole,
            }),
            "--image-base" => {
                args.image_base = Some(parse_u64(&take_value(&mut i, "--image-base")?)?)
            }
            "-z" | "-m" | "--flavor" | "-flavor" | "--hash-style" | "--sysroot" => {
                take_value(&mut i, a)?;
            }
            "--gc-sections" => args.gc_sections = true,
            "--no-gc-sections" => args.gc_sections = false,
            "--whole-archive" => whole = true,
            "--no-whole-archive" => whole = false,
            "--version" | "-v" | "-V" => args.show_version = true,
            "--help" | "-h" => args.show_help = true,
            "--as-needed"
            | "--no-as-needed"
            | "-Bstatic"
            | "-Bdynamic"
            | "-static"
            | "--static"
            | "-pie"
            | "--pie"
            | "-no-pie"
            | "--no-pie"
            | "--eh-frame-hdr"
            | "--no-eh-frame-hdr"
            | "-nostdlib"
            | "--strip-debug"
            | "-S"
            | "--strip-all"
            | "-s"
            | "--build-id"
            | "--no-undefined"
            | "--no-undefined-version"
            | "--fatal-warnings"
            | "--no-rosegment"
            | "--rosegment"
            | "--pack-dyn-relocs"
            | "--no-dynamic-linker"
            | "-Bsymbolic"
            | "--export-dynamic"
            | "-E"
            | "--no-export-dynamic"
            | "--relax"
            | "--no-relax"
            | "-nmagic"
            | "-n"
            | "-N"
            | "--omagic" => {}
            _ if a.starts_with("-o=") => args.output = Some(a[3..].to_string()),
            _ if a.starts_with("--output=") => args.output = Some(a[9..].to_string()),
            _ if a.starts_with("--entry=") => args.entry = Some(a[8..].to_string()),
            _ if a.starts_with("--image-base=") => args.image_base = Some(parse_u64(&a[13..])?),
            _ if a.starts_with("-L") => args.lib_dirs.push(a[2..].to_string()),
            _ if a.starts_with("-l") => args.inputs.push(InputArg {
                path: a[2..].to_string(),
                is_lib: true,
                whole_archive: whole,
            }),
            _ if a.starts_with("-O") || a.starts_with("--build-id=") || a.starts_with("-z") => {}
            _ if a.starts_with("--hash-style=")
                || a.starts_with("--sysroot=")
                || a.starts_with("-plugin")
                || a.starts_with("--plugin")
                || a.starts_with("--dependency-file=")
                || a.starts_with("--version-script=")
                || a.starts_with("--dynamic-linker=")
                || a.starts_with("-m") =>
            {
                if a == "--version-script" || a == "--dependency-file" {
                    take_value(&mut i, a)?;
                }
            }
            _ if a.starts_with('@') => {
                return Err(Error::Args(format!(
                    "response file {a} must be expanded by the caller"
                )))
            }
            _ if a.starts_with('-') && a.len() > 1 => {
                return Err(Error::Args(format!("unknown option {a}")))
            }
            _ => args.inputs.push(InputArg {
                path: a.to_string(),
                is_lib: false,
                whole_archive: whole,
            }),
        }
        i += 1;
    }
    Ok(args)
}

fn parse_u64(s: &str) -> Result<u64> {
    let r = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16)
    } else {
        s.parse::<u64>()
    };
    r.map_err(|_| Error::Args(format!("bad number {s}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rustc_ld_line() {
        let argv = [
            "symbols.o",
            "crate.o",
            "--as-needed",
            "-Bstatic",
            "libcore.rlib",
            "-L",
            "/tmp/raw-dylibs",
            "-Bdynamic",
            "--eh-frame-hdr",
            "-z",
            "noexecstack",
            "-o",
            "out",
            "--gc-sections",
            "-pie",
            "-z",
            "relro",
            "-z",
            "now",
            "--image-base=0xc8000000000",
            "--no-pie",
        ];
        let a = parse(&argv).unwrap();
        assert_eq!(a.output.as_deref(), Some("out"));
        assert_eq!(a.image_base, Some(0xc80_0000_0000));
        assert_eq!(a.inputs.len(), 3);
        assert_eq!(a.lib_dirs, ["/tmp/raw-dylibs"]);
        assert!(a.gc_sections);
    }

    #[test]
    fn whole_archive_and_libs() {
        let a = parse(&["--whole-archive", "-lfoo", "--no-whole-archive", "bar.o"]).unwrap();
        assert!(a.inputs[0].is_lib && a.inputs[0].whole_archive);
        assert!(!a.inputs[1].whole_archive);
    }

    #[test]
    fn rejects_unknown_option() {
        assert!(parse(&["--bogus"]).is_err());
        assert!(parse(&["-o"]).is_err());
    }
}
