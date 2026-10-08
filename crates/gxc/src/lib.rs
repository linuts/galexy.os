//! gxc — Galexy mini Rust-subset compiler (host).
//!
//! **Rust subset for Galexy**, not rustc-compatible. See `docs/COMPILER.md`.
//!
//! - Milestone 59: lex → parse → check
//! - Milestone 60: hand-x64 codegen
//! - Milestone 69: relocatable object out, linked by `gxld`
//!
//! Frozen at gxr v0 (`docs/COMPILER.md`); kept as the smallest producer of
//! a relocatable object the linker has to accept.

#![deny(missing_docs)]

pub mod ast;
pub mod check;
pub mod codegen;
pub mod error;
pub mod lex;
pub mod obj;
pub mod parse;

use ast::Program;
use error::{Error, Result};

/// Frontend pipeline: lex → parse → check.
pub fn compile_check(src: &str) -> Result<Program> {
    let tokens = lex::lex(src)?;
    let program = parse::parse(&tokens)?;
    check::check(program)
}

/// Host compile to a relocatable object: check → hand-x64 → `ET_REL`.
pub fn compile_object(src: &str) -> Result<Vec<u8>> {
    let program = compile_check(src)?;
    let code = codegen::codegen(&program);
    Ok(obj::emit_object(&code))
}

/// Full host compile: object → `gxld` → static ELF64 at `USER_IMAGE_BASE`.
pub fn compile_elf(src: &str) -> Result<Vec<u8>> {
    let object = compile_object(src)?;
    let inputs = [gxld::Input {
        name: "gxc.o".into(),
        bytes: &object,
        whole_archive: false,
    }];
    gxld::link(&inputs, &gxld::Options::default()).map_err(|e| Error::msg(format!("link: {e}")))
}

/// Codegen backend: hand-written x86_64 (Milestone 60).
pub const CODEGEN_BACKEND_PLAN: &str = "hand-x64";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Expr, Stmt};
    use galexy_abi::USER_IMAGE_BASE;

    const HELLO: &str = include_str!("../examples/hello.gxr");

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

    #[test]
    fn compile_elf_hello_entry() {
        let bytes = compile_elf(HELLO).unwrap();
        gxld::validate(&bytes, USER_IMAGE_BASE).unwrap();
        // gxld layout: R (headers + rodata) in page 0, text at page 1.
        let entry = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
        assert_eq!(entry, USER_IMAGE_BASE + 0x1000);
        // The linked `mov rsi, imm64` points at the hello string.
        let rsi = u64::from_le_bytes(bytes[0x1000 + 22..0x1000 + 30].try_into().unwrap());
        let off = (rsi - USER_IMAGE_BASE) as usize;
        assert_eq!(&bytes[off..off + 16], b"Hello from gxc!\n");
    }
}
