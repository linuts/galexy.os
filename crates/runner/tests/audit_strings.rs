//! Audit format strings must not carry password material.

use std::fs;
use std::path::Path;

#[test]
fn serial_audit_lines_omit_password_material() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../galexy-os/src");
    let mut hits = Vec::new();
    walk(&root, &mut hits);
    assert!(
        hits.is_empty(),
        "serial_println! must not mention password:\n{}",
        hits.join("\n")
    );
}

fn walk(dir: &Path, hits: &mut Vec<String>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, hits);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let text =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        scan(&path, &text, hits);
    }
}

fn scan(path: &Path, text: &str, hits: &mut Vec<String>) {
    let bytes = text.as_bytes();
    let needle = b"serial_println!";
    let mut i = 0usize;
    while let Some(rel) = text[i..].find("serial_println!") {
        let start = i + rel;
        let after = start + needle.len();
        let Some(end) = invocation_end(&bytes[after..]) else {
            break;
        };
        let body = &text[after..after + end];
        if body.to_ascii_lowercase().contains("password") {
            let line = text[..start].bytes().filter(|b| *b == b'\n').count() + 1;
            hits.push(format!("{}:{}", path.display(), line));
        }
        i = after + end;
    }
}

/// Byte length of a `serial_println!` argument list, starting after `!`.
fn invocation_end(rest: &[u8]) -> Option<usize> {
    let mut i = 0usize;
    while i < rest.len() && rest[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= rest.len() || rest[i] != b'(' {
        return Some(i);
    }
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    while i < rest.len() {
        let b = rest[i];
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else if b == b'"' {
            in_str = true;
        } else if b == b'(' {
            depth += 1;
        } else if b == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(i + 1);
            }
        }
        i += 1;
    }
    None
}
