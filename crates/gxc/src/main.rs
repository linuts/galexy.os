//! `gxc` — host CLI for the Galexy Rust-subset compiler.
//!
//! Milestone 59 commands: `check`. Codegen lands in Milestone 60.

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
        "check" => {
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
                        "ok: {} statement(s), return {:?}; backend plan = {}",
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
        "version" | "--version" | "-V" => {
            println!("gxc {} (gxr v0 subset — not rustc)", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("gxc: unknown command `{other}`");
            print_help();
            ExitCode::from(2)
        }
    }
}

fn print_help() {
    eprintln!(
        "\
gxc — Galexy mini Rust-subset compiler (not rustc)

Usage:
  gxc check <file.gxr>   Lex, parse, and type-check (Milestone 59)
  gxc version

See docs/COMPILER.md for the language slice and roadmap."
    );
}
