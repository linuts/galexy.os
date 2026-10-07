//! Lexer for gxr v0.

use crate::error::{Error, Result};

/// A token with its source byte offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// Kind.
    pub kind: TokenKind,
    /// Byte offset of the first character.
    pub offset: usize,
}

/// Token kinds accepted by gxr v0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    /// `fn`
    Fn,
    /// `main`
    Main,
    /// `i32`
    I32,
    /// `write_console`
    WriteConsole,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `->`
    Arrow,
    /// `;`
    Semi,
    /// `,` (reserved; rejected in check if used oddly)
    Comma,
    /// Integer literal (`0`, `42`, `-1` is unary minus + int — v0 only allows unsigned lexeme then optional leading `-` as separate token)
    Int(i32),
    /// Byte string `b"..."` (escape: `\\`, `\n`, `\t`, `\r`, `\"`)
    ByteStr(Vec<u8>),
    /// Identifier that is not a keyword (rejected later unless known).
    Ident(String),
    /// `#![...]` inner attribute — recorded so the parser can allow a fixed set.
    InnerAttr(String),
    /// End of input.
    Eof,
}

/// Lex `src` into tokens.
pub fn lex(src: &str) -> Result<Vec<Token>> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        let start = i;
        let c = bytes[i];

        // Whitespace.
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }

        // Line comment `// ...`.
        if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // Block comment `/* ... */` — not in v0; reject clearly.
        if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            return Err(Error::at(start, "block comments are not in gxr v0"));
        }

        // Inner attribute `#![...]`.
        if c == b'#' && bytes.get(i + 1) == Some(&b'!') && bytes.get(i + 2) == Some(&b'[') {
            i += 3;
            let attr_start = i;
            while i < bytes.len() && bytes[i] != b']' {
                i += 1;
            }
            if i >= bytes.len() {
                return Err(Error::at(start, "unterminated inner attribute"));
            }
            let body = src[attr_start..i].trim().to_string();
            i += 1; // ]
            out.push(Token {
                kind: TokenKind::InnerAttr(body),
                offset: start,
            });
            continue;
        }

        // Byte string `b"..."`.
        if c == b'b' && bytes.get(i + 1) == Some(&b'"') {
            i += 2;
            let mut data = Vec::new();
            while i < bytes.len() {
                let b = bytes[i];
                if b == b'"' {
                    i += 1;
                    out.push(Token {
                        kind: TokenKind::ByteStr(data),
                        offset: start,
                    });
                    break;
                }
                if b == b'\\' {
                    i += 1;
                    let Some(esc) = bytes.get(i).copied() else {
                        return Err(Error::at(start, "unterminated byte-string escape"));
                    };
                    data.push(match esc {
                        b'n' => b'\n',
                        b't' => b'\t',
                        b'r' => b'\r',
                        b'\\' => b'\\',
                        b'"' => b'"',
                        b'0' => 0,
                        _ => {
                            return Err(Error::at(
                                i,
                                format!("unsupported byte-string escape \\{}", esc as char),
                            ));
                        }
                    });
                    i += 1;
                    continue;
                }
                if !b.is_ascii() {
                    return Err(Error::at(i, "non-ASCII bytes in byte-string literal"));
                }
                data.push(b);
                i += 1;
            }
            if matches!(out.last().map(|t| &t.kind), Some(TokenKind::ByteStr(_))) {
                continue;
            }
            return Err(Error::at(start, "unterminated byte-string literal"));
        }

        // Punctuation / arrows.
        match c {
            b'(' => {
                out.push(Token {
                    kind: TokenKind::LParen,
                    offset: start,
                });
                i += 1;
                continue;
            }
            b')' => {
                out.push(Token {
                    kind: TokenKind::RParen,
                    offset: start,
                });
                i += 1;
                continue;
            }
            b'{' => {
                out.push(Token {
                    kind: TokenKind::LBrace,
                    offset: start,
                });
                i += 1;
                continue;
            }
            b'}' => {
                out.push(Token {
                    kind: TokenKind::RBrace,
                    offset: start,
                });
                i += 1;
                continue;
            }
            b';' => {
                out.push(Token {
                    kind: TokenKind::Semi,
                    offset: start,
                });
                i += 1;
                continue;
            }
            b',' => {
                out.push(Token {
                    kind: TokenKind::Comma,
                    offset: start,
                });
                i += 1;
                continue;
            }
            b'-' if bytes.get(i + 1) == Some(&b'>') => {
                out.push(Token {
                    kind: TokenKind::Arrow,
                    offset: start,
                });
                i += 2;
                continue;
            }
            b'-' => {
                // Negative integer: `-` followed by digits.
                i += 1;
                if i < bytes.len() && bytes[i].is_ascii_digit() {
                    let (n, next) = lex_int(bytes, i)?;
                    // Negate carefully.
                    let neg = n
                        .checked_neg()
                        .ok_or_else(|| Error::at(start, "integer literal overflow"))?;
                    out.push(Token {
                        kind: TokenKind::Int(neg),
                        offset: start,
                    });
                    i = next;
                    continue;
                }
                return Err(Error::at(start, "expected `->` or a negative integer"));
            }
            _ => {}
        }

        // Integer.
        if c.is_ascii_digit() {
            let (n, next) = lex_int(bytes, i)?;
            out.push(Token {
                kind: TokenKind::Int(n),
                offset: start,
            });
            i = next;
            continue;
        }

        // Identifier / keyword.
        if c.is_ascii_alphabetic() || c == b'_' {
            let begin = i;
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
            {
                i += 1;
            }
            let name = &src[begin..i];
            let kind = match name {
                "fn" => TokenKind::Fn,
                "main" => TokenKind::Main,
                "i32" => TokenKind::I32,
                "write_console" => TokenKind::WriteConsole,
                _ => TokenKind::Ident(name.to_string()),
            };
            out.push(Token {
                kind,
                offset: start,
            });
            continue;
        }

        return Err(Error::at(
            start,
            format!("unexpected character {:?}", c as char),
        ));
    }

    out.push(Token {
        kind: TokenKind::Eof,
        offset: src.len(),
    });
    Ok(out)
}

fn lex_int(bytes: &[u8], mut i: usize) -> Result<(i32, usize)> {
    let start = i;
    if bytes[i] == b'0' && bytes.get(i + 1).is_some_and(|b| b.is_ascii_digit()) {
        return Err(Error::at(start, "leading zeros are not allowed on integers"));
    }
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let s = std::str::from_utf8(&bytes[start..i]).unwrap();
    let n: i32 = s
        .parse()
        .map_err(|_| Error::at(start, "integer literal overflow"))?;
    Ok((n, i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexes_hello_shape() {
        let src = r#"fn main() -> i32 { write_console(b"hi\n"); 0 }"#;
        let toks = lex(src).unwrap();
        assert!(matches!(toks[0].kind, TokenKind::Fn));
        assert!(matches!(toks[1].kind, TokenKind::Main));
        assert!(toks.iter().any(|t| matches!(&t.kind, TokenKind::ByteStr(b) if b == b"hi\n")));
    }
}
