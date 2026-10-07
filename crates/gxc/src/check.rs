//! Name resolution and type checks for gxr v0.

use crate::ast::{Expr, Program, Stmt};
use crate::error::{Error, Result};

/// Check a parsed program. v0 rules are almost entirely structural already;
/// this pass rejects empty / oversized console writes and documents the
/// prelude surface.
pub fn check(program: Program) -> Result<Program> {
    for stmt in &program.body {
        match stmt {
            Stmt::WriteConsole(bytes) => {
                if bytes.is_empty() {
                    return Err(Error::msg(
                        "write_console argument must be a non-empty byte-string",
                    ));
                }
                // Kernel write path uses a u64 length; keep demos small.
                if bytes.len() > 4096 {
                    return Err(Error::msg(
                        "write_console argument exceeds 4096 bytes (gxr v0 limit)",
                    ));
                }
            }
        }
    }
    match program.ret {
        Expr::Int(_) => {}
    }
    Ok(program)
}
