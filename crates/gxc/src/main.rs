//! `gxc` — host CLI for the Galexy Rust-subset compiler.

use std::env;
use std::fs;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print_help();
        return ExitCode::SUCCESS;
    }

    let cmd = args.remove(0);
    match cmd.as_str() {
        "check" => cmd_check(&args),
        "build" => cmd_build(&args),
        "version" | "--version" | "-V" => {
            println!(
                "gxc {} (gxr v0 subset — not rustc; backend {})",
                env!("CARGO_PKG_VERSION"),
                gxc::CODEGEN_BACKEND_PLAN
            );
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("gxc: unknown command `{other}`");
            print_help();
            ExitCode::from(2)
        }
    }
}

fn cmd_check(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("gxc check: missing <file.gxr>");
        return ExitCode::from(2);
    };
    let src = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("gxc: read {path}: {e}");
            return ExitCode::from(1);
        }
    };
    match gxc::compile_check(&src) {
        Ok(prog) => {
            println!(
                "ok: {} statement(s), return {:?}; backend = {}",
                prog.body.len(),
                prog.ret,
                gxc::CODEGEN_BACKEND_PLAN
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("gxc: {e}");
            ExitCode::from(1)
        }
    }
}

fn cmd_build(args: &[String]) -> ExitCode {
    let mut input = None;
    let mut output = None;
    let mut object_only = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-c" => object_only = true,
            "-o" => {
                i += 1;
                let Some(path) = args.get(i) else {
                    eprintln!("gxc build: -o needs a path");
                    return ExitCode::from(2);
                };
                output = Some(path.clone());
            }
            s if s.starts_with('-') => {
                eprintln!("gxc build: unknown flag `{s}`");
                return ExitCode::from(2);
            }
            s => {
                if input.is_some() {
                    eprintln!("gxc build: unexpected argument `{s}`");
                    return ExitCode::from(2);
                }
                input = Some(s.to_string());
            }
        }
        i += 1;
    }
    let Some(input) = input else {
        eprintln!("gxc build: missing <file.gxr>");
        return ExitCode::from(2);
    };
    let output = output.unwrap_or_else(|| {
        let stem = input.trim_end_matches(".gxr");
        if object_only {
            format!("{stem}.o")
        } else {
            format!("{stem}.elf")
        }
    });
    let src = match fs::read_to_string(&input) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("gxc: read {input}: {e}");
            return ExitCode::from(1);
        }
    };
    let result = if object_only {
        gxc::compile_object(&src)
    } else {
        gxc::compile_elf(&src)
    };
    match result {
        Ok(bytes) => {
            if let Err(e) = fs::write(&output, &bytes) {
                eprintln!("gxc: write {output}: {e}");
                return ExitCode::from(1);
            }
            println!(
                "wrote {output} ({} bytes, backend {}{})",
                bytes.len(),
                gxc::CODEGEN_BACKEND_PLAN,
                if object_only { "" } else { ", linked by gxld" }
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("gxc: {e}");
            ExitCode::from(1)
        }
    }
}

fn print_help() {
    eprintln!(
        "\
gxc — Galexy mini Rust-subset compiler (not rustc)

Usage:
  gxc check <file.gxr>           Lex, parse, and type-check
  gxc build [-o out.elf] <file>  Object → gxld → static ELF64 @ USER_IMAGE_BASE
  gxc build -c [-o out.o] <file> Emit the relocatable object only
  gxc version

See docs/COMPILER.md."
    );
}
