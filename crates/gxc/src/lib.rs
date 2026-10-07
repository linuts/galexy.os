//! gxc — Galexy mini Rust-subset compiler (host).
//!
//! **Rust subset for Galexy**, not rustc-compatible. See `docs/COMPILER.md`.
//!
//! Milestone 59: lex → parse → check. Codegen/ELF is Milestone 60.

#![deny(missing_docs)]

pub mod ast;
pub mod check;
pub mod error;
pub mod lex;
pub mod parse;

use ast::Program;
use error::Result;

/// Frontend pipeline: lex → parse → check.
pub fn compile_check(src: &str) -> Result<Program> {
    let tokens = lex::lex(src)?;
    let program = parse::parse(&tokens)?;
    check::check(program)
}

/// Codegen backend choice for Milestone 60 (recorded at M59 freeze).
///
/// Hello is ~a dozen instructions; **hand-written x86_64** keeps the
/// dependency graph empty and the ELF shape obvious. Cranelift remains an
/// option if the subset grows past what a small encoder can love.
pub const CODEGEN_BACKEND_PLAN: &str = "hand-x64";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Expr, Stmt};

    const HELLO: &str = r#"
// hello.gxr — frozen gxr v0 example
#![no_std]
#![no_main]
fn main() -> i32 {
    write_console(b"Hello from gxc!\n");
    0
}
"#;

    #[test]
    fn checks_hello() {
        let p = compile_check(HELLO).unwrap();
        assert_eq!(p.body.len(), 1);
        match &p.body[0] {
            Stmt::WriteConsole(b) => assert_eq!(b, b"Hello from gxc!\n"),
        }
        assert_eq!(p.ret, Expr::Int(0));
    }

    #[test]
    fn rejects_unknown_ident() {
        let src = r#"fn main() -> i32 { println!("no"); 0 }"#;
        let err = compile_check(src).unwrap_err();
        assert!(
            err.message.contains("unexpected") || err.message.contains("gxr v0"),
            "{err}"
        );
    }

    #[test]
    fn rejects_extra_fn() {
        let src = r#"
fn main() -> i32 { 0 }
fn other() -> i32 { 0 }
"#;
        assert!(compile_check(src).is_err());
    }

    #[test]
    fn rejects_unknown_attr() {
        let src = r#"#![feature(x)] fn main() -> i32 { 0 }"#;
        let err = compile_check(src).unwrap_err();
        assert!(err.message.contains("not in gxr v0"), "{err}");
    }

    #[test]
    fn rejects_empty_write() {
        let src = r#"fn main() -> i32 { write_console(b""); 0 }"#;
        assert!(compile_check(src).is_err());
    }

    #[test]
    fn allows_negative_return() {
        let p = compile_check(r#"fn main() -> i32 { -1 }"#).unwrap();
        assert_eq!(p.ret, Expr::Int(-1));
    }

    #[test]
    fn backend_plan_is_hand_x64() {
        assert_eq!(CODEGEN_BACKEND_PLAN, "hand-x64");
    }
}
