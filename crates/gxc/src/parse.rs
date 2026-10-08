//! Recursive-descent parser for gxr v0.

use crate::ast::{Expr, Program, Stmt};
use crate::error::{Error, Result};
use crate::lex::{Token, TokenKind};

/// Parse tokens into an unchecked [`Program`].
///
/// Shape:
/// ```text
/// ( #![no_std] | #![no_main] )*
/// fn main() -> i32 { stmt* expr }
/// ```
pub fn parse(tokens: &[Token]) -> Result<Program> {
    let mut p = Parser { tokens, i: 0 };
    p.skip_allowed_attrs()?;
    p.expect_kind(&TokenKind::Fn, "expected `fn`")?;
    p.expect_kind(&TokenKind::Main, "expected `main`")?;
    p.expect_kind(&TokenKind::LParen, "expected `(`")?;
    p.expect_kind(&TokenKind::RParen, "expected `)`")?;
    p.expect_kind(&TokenKind::Arrow, "expected `->`")?;
    p.expect_kind(&TokenKind::I32, "expected `i32` return type")?;
    p.expect_kind(&TokenKind::LBrace, "expected `{`")?;

    let mut body = Vec::new();
    let mut ret = Expr::Int(0);

    loop {
        if p.peek_kind() == Some(&TokenKind::RBrace) {
            break;
        }
        if p.peek_kind() == Some(&TokenKind::Eof) {
            return Err(Error::at(p.offset(), "unterminated `main` body"));
        }

        // Statement: write_console(...);
        if p.peek_kind() == Some(&TokenKind::WriteConsole) {
            p.bump();
            p.expect_kind(&TokenKind::LParen, "expected `(` after write_console")?;
            let bytes = match p.bump_kind() {
                Some(TokenKind::ByteStr(b)) => b,
                _ => {
                    return Err(Error::at(
                        p.offset(),
                        "write_console expects a byte-string literal `b\"...\"`",
                    ));
                }
            };
            p.expect_kind(&TokenKind::RParen, "expected `)`")?;
            p.expect_kind(&TokenKind::Semi, "expected `;` after write_console(...)")?;
            body.push(Stmt::WriteConsole(bytes));
            continue;
        }

        // Trailing return expression: integer then optional `;` then `}`.
        if let Some(TokenKind::Int(n)) = p.peek_kind().cloned() {
            p.bump();
            ret = Expr::Int(n);
            // Optional semicolon before `}`.
            if p.peek_kind() == Some(&TokenKind::Semi) {
                p.bump();
            }
            if p.peek_kind() != Some(&TokenKind::RBrace) {
                return Err(Error::at(
                    p.offset(),
                    "only a trailing integer return is allowed after statements",
                ));
            }
            break;
        }

        return Err(Error::at(
            p.offset(),
            "unexpected token in `main` (gxr v0 allows `write_console(...);` and a trailing `i32`)"
                .to_string(),
        ));
    }

    p.expect_kind(&TokenKind::RBrace, "expected `}`")?;
    p.expect_kind(&TokenKind::Eof, "extra tokens after `main`")?;
    Ok(Program { body, ret })
}

struct Parser<'a> {
    tokens: &'a [Token],
    i: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a Token> {
        self.tokens.get(self.i)
    }

    fn peek_kind(&self) -> Option<&'a TokenKind> {
        self.peek().map(|t| &t.kind)
    }

    fn offset(&self) -> usize {
        self.peek().map(|t| t.offset).unwrap_or(0)
    }

    fn bump(&mut self) -> Option<&'a Token> {
        let t = self.peek();
        if t.is_some() {
            self.i += 1;
        }
        t
    }

    fn bump_kind(&mut self) -> Option<TokenKind> {
        self.bump().map(|t| t.kind.clone())
    }

    fn expect_kind(&mut self, want: &TokenKind, msg: &str) -> Result<()> {
        match self.peek_kind() {
            Some(k) if kind_eq(k, want) => {
                self.bump();
                Ok(())
            }
            Some(k) => Err(Error::at(
                self.offset(),
                format!("{msg}, found {}", kind_name(k)),
            )),
            None => Err(Error::msg(format!("{msg}, found end of input"))),
        }
    }

    fn skip_allowed_attrs(&mut self) -> Result<()> {
        while let Some(TokenKind::InnerAttr(name)) = self.peek_kind().cloned() {
            match name.as_str() {
                "no_std" | "no_main" => {
                    self.bump();
                }
                other => {
                    return Err(Error::at(
                        self.offset(),
                        format!(
                            "inner attribute `#![{other}]` is not in gxr v0 (only no_std / no_main)"
                        ),
                    ));
                }
            }
        }
        Ok(())
    }
}

fn kind_eq(a: &TokenKind, b: &TokenKind) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
        && match (a, b) {
            (TokenKind::Int(_), TokenKind::Int(_)) => true,
            (TokenKind::ByteStr(_), TokenKind::ByteStr(_)) => true,
            (TokenKind::Ident(_), TokenKind::Ident(_)) => true,
            (TokenKind::InnerAttr(_), TokenKind::InnerAttr(_)) => true,
            _ => a == b,
        }
}

fn kind_name(k: &TokenKind) -> String {
    match k {
        TokenKind::Fn => "`fn`".into(),
        TokenKind::Main => "`main`".into(),
        TokenKind::I32 => "`i32`".into(),
        TokenKind::WriteConsole => "`write_console`".into(),
        TokenKind::LParen => "`(`".into(),
        TokenKind::RParen => "`)`".into(),
        TokenKind::LBrace => "`{`".into(),
        TokenKind::RBrace => "`}`".into(),
        TokenKind::Arrow => "`->`".into(),
        TokenKind::Semi => "`;`".into(),
        TokenKind::Comma => "`,`".into(),
        TokenKind::Int(n) => format!("integer `{n}`"),
        TokenKind::ByteStr(_) => "byte-string".into(),
        TokenKind::Ident(s) => format!("identifier `{s}`"),
        TokenKind::InnerAttr(s) => format!("#![{s}]"),
        TokenKind::Eof => "end of input".into(),
    }
}
