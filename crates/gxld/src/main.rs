//! `gxld` command line: GNU-ld-style arguments, exit 0 on success, 1 on a
//! link error with the message on stderr.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let argv = match expand_response_files(raw) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    let args = match gxld::args::parse(&argv) {
        Ok(a) => a,
        Err(e) => return fail(&e.to_string()),
    };
    if args.show_version {
        println!("{}", gxld::VERSION);
        return ExitCode::SUCCESS;
    }
    if args.show_help {
        println!("usage: gxld [-o out] [--image-base=ADDR] [-e SYM] [--no-gc-sections] inputs...");
        println!("static ELF64 x86_64 linker for Galexy (docs/LINKER.md)");
        return ExitCode::SUCCESS;
    }
    let Some(output) = args.output.clone() else {
        return fail("no output file (-o)");
    };
    if args.inputs.is_empty() {
        return fail("no input files");
    }

    let mut files: Vec<(String, Vec<u8>, bool)> = Vec::new();
    for inp in &args.inputs {
        let path = if inp.is_lib {
            match find_lib(&inp.path, &args.lib_dirs) {
                Some(p) => p,
                None => return fail(&format!("library not found: -l{}", inp.path)),
            }
        } else {
            PathBuf::from(&inp.path)
        };
        match std::fs::read(&path) {
            Ok(bytes) => files.push((path.display().to_string(), bytes, inp.whole_archive)),
            Err(e) => return fail(&format!("{}: {e}", path.display())),
        }
    }
    let inputs: Vec<gxld::Input<'_>> = files
        .iter()
        .map(|(name, bytes, whole)| gxld::Input {
            name: name.clone(),
            bytes,
            whole_archive: *whole,
        })
        .collect();
    let mut opts = gxld::Options::default();
    if let Some(base) = args.image_base {
        opts.image_base = base;
    }
    if let Some(entry) = args.entry {
        opts.entry = entry;
    }
    opts.gc_sections = args.gc_sections;

    match gxld::link(&inputs, &opts) {
        Ok(image) => match std::fs::write(&output, image) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(&format!("{output}: {e}")),
        },
        Err(e) => fail(&e.to_string()),
    }
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("gxld: error: {msg}");
    ExitCode::FAILURE
}

fn find_lib(name: &str, dirs: &[String]) -> Option<PathBuf> {
    let candidates = [
        format!("lib{name}.a"),
        format!("lib{name}.rlib"),
        name.to_string(),
    ];
    for dir in dirs {
        for c in &candidates {
            let p = Path::new(dir).join(c);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// `@file` → the file's whitespace-separated (optionally quoted) tokens.
fn expand_response_files(argv: Vec<String>) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for a in argv {
        if let Some(path) = a.strip_prefix('@') {
            let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
            out.extend(split_response(&text));
        } else {
            out.push(a);
        }
    }
    Ok(out)
}

fn split_response(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) if c == '\\' => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '\\' => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            None if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}
