//! Typed AST for gxr v0 after check.

/// A checked gxr program: exactly one `main` and its statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    /// Statements inside `main`, in order. The last expression (if any)
    /// is the return value; otherwise the function returns `0`.
    pub body: Vec<Stmt>,
    /// Return expression of `main` (always `i32` after check).
    pub ret: Expr,
}

/// A statement in `main`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    /// `write_console(<byte-string>);`
    WriteConsole(Vec<u8>),
}

/// An expression (v0: integer literals only at return).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// Signed 32-bit integer literal.
    Int(i32),
}
