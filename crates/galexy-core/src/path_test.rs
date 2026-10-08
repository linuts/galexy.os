//! Host tests of the galfs path grammar: pinned cases plus an exhaustive
//! sweep over every string up to a fixed length from a small alphabet
//! that covers each grammar-relevant byte class.

use super::path::{component_ok, parse_path, ParsedPath, MAX_DEPTH, NAME_CAP};

extern crate std;
use std::string::String;
use std::vec::Vec;

#[test]
fn component_rules() {
    assert!(component_ok("a"));
    assert!(component_ok("Readme.txt"));
    assert!(component_ok("snake_case-name.v2"));
    assert!(component_ok(&"x".repeat(NAME_CAP)));
    assert!(!component_ok(""));
    assert!(!component_ok("."));
    assert!(!component_ok(".."));
    assert!(component_ok("..."));
    assert!(component_ok(".hidden"));
    assert!(!component_ok(&"x".repeat(NAME_CAP + 1)));
    assert!(!component_ok("a b"));
    assert!(!component_ok("a/b"));
    assert!(!component_ok("a@b"));
    assert!(!component_ok("ünïcödé"));
    assert!(!component_ok("nul\0byte"));
}

fn parsed(name: &str) -> ParsedPath<'_> {
    parse_path(name).unwrap_or_else(|| panic!("{name:?} should parse"))
}

#[test]
fn accepts_documented_shapes() {
    let p = parsed("notes");
    assert_eq!(p.owner, None);
    assert_eq!(p.components(), ["notes"]);
    assert!(!p.dir);

    let p = parsed("/notes");
    assert_eq!(p.components(), ["notes"]);

    let p = parsed("docs/");
    assert_eq!(p.components(), ["docs"]);
    assert!(p.dir);

    let p = parsed("docs/a/b.txt");
    assert_eq!(p.components(), ["docs", "a", "b.txt"]);
    assert!(!p.dir);

    let p = parsed("eve@notes");
    assert_eq!(p.owner, Some("eve"));
    assert_eq!(p.components(), ["notes"]);

    let p = parsed("eve@docs/x");
    assert_eq!(p.owner, Some("eve"));
    assert_eq!(p.components(), ["docs", "x"]);

    let p = parsed("eve@/");
    assert_eq!(p.owner, Some("eve"));
    assert_eq!(p.n, 0);
    assert!(p.dir);

    let deep = (0..MAX_DEPTH)
        .map(|i| std::format!("d{i}"))
        .collect::<Vec<_>>()
        .join("/");
    assert_eq!(parsed(&deep).n, MAX_DEPTH);
}

#[test]
fn rejects_documented_shapes() {
    for bad in [
        "",
        "/",
        "//",
        "a//b",
        "a//",
        "/a//",
        ".",
        "..",
        "a/./b",
        "a/../b",
        "../a",
        "a/..",
        "eve@",
        "eve@/x",
        "eve@//",
        "@a",
        "@",
        "a@b@c",
        "a/eve@b",
        "eve@a/b@c",
        "a b",
        "a\tb",
        "a\\b",
        "a\0",
    ] {
        assert!(parse_path(bad).is_none(), "{bad:?} must be rejected");
    }
    assert!(parse_path("a/").is_some());
    assert!(parse_path("eve@x/").is_some());
    let too_deep = (0..=MAX_DEPTH)
        .map(|i| std::format!("d{i}"))
        .collect::<Vec<_>>()
        .join("/");
    assert!(parse_path(&too_deep).is_none());
    let long = "x".repeat(NAME_CAP + 1);
    assert!(parse_path(&long).is_none());
    assert!(parse_path(&std::format!("{long}@a")).is_none());
    assert!(parse_path(&std::format!("a@{long}")).is_none());
}

/// Rebuilds the canonical spelling of a parsed path.
fn render(p: &ParsedPath<'_>) -> String {
    let mut s = String::new();
    if let Some(owner) = p.owner {
        s.push_str(owner);
        s.push('@');
    }
    s.push_str(&p.components().join("/"));
    if p.dir {
        s.push('/');
    }
    s
}

/// Reference acceptor written against the grammar in `docs/GALFS.md`,
/// independently of the parser's control flow.
fn oracle(input: &str) -> bool {
    let body = input.strip_prefix('/').unwrap_or(input);
    let (body, dir) = match body.strip_suffix('/') {
        Some(b) => (b, true),
        None => (body, false),
    };
    if body.is_empty() {
        return false;
    }
    let parts: Vec<&str> = body.split('/').collect();
    if parts.len() > MAX_DEPTH {
        return false;
    }
    let first = parts[0];
    let rest = &parts[1..];
    if rest.iter().any(|c| !component_ok(c)) {
        return false;
    }
    match first.split_once('@') {
        None => component_ok(first),
        Some((owner, leaf)) => {
            if !component_ok(owner) {
                return false;
            }
            if leaf.is_empty() {
                // `owner@/` only: directory, nothing else.
                dir && rest.is_empty()
            } else {
                component_ok(leaf)
            }
        }
    }
}

#[test]
fn exhaustive_small_alphabet_matches_oracle_and_round_trips() {
    // One representative per byte class the grammar distinguishes:
    // alphanumeric, the three punctuation marks a component may hold,
    // the two structural bytes, and one illegal byte.
    const ALPHABET: &[u8] = b"a.-_/@!";
    const MAX_LEN: usize = 7;

    let mut buf = Vec::with_capacity(MAX_LEN);
    let mut accepted = 0usize;
    let mut total = 0usize;
    for len in 0..=MAX_LEN {
        let mut idx = std::vec![0usize; len];
        loop {
            buf.clear();
            buf.extend(idx.iter().map(|&i| ALPHABET[i]));
            let s = core::str::from_utf8(&buf).unwrap();
            total += 1;

            let got = parse_path(s);
            assert_eq!(
                got.is_some(),
                oracle(s),
                "{s:?}: parser and oracle disagree"
            );
            if let Some(p) = got {
                accepted += 1;
                assert!(p.n <= MAX_DEPTH);
                assert!(p.components().iter().all(|c| component_ok(c)));
                if let Some(owner) = p.owner {
                    assert!(component_ok(owner));
                }
                assert!(p.n > 0 || (p.owner.is_some() && p.dir));
                assert_eq!(p.dir, s.ends_with('/') && s != "/");
                // Canonical spelling re-parses to the same structure and
                // equals the input minus its optional leading slash.
                let canon = render(&p);
                assert_eq!(canon, s.strip_prefix('/').unwrap_or(s));
                assert_eq!(parse_path(&canon), Some(p));
            }

            // Next tuple in the odometer.
            let mut k = len;
            loop {
                if k == 0 {
                    break;
                }
                k -= 1;
                idx[k] += 1;
                if idx[k] < ALPHABET.len() {
                    break;
                }
                idx[k] = 0;
            }
            if idx.iter().all(|&i| i == 0) {
                break;
            }
        }
    }
    let expected_total: usize = (0..=MAX_LEN).map(|l| ALPHABET.len().pow(l as u32)).sum();
    assert_eq!(total, expected_total);
    assert!(accepted > 1000, "only {accepted} of {total} accepted");
}

#[test]
fn depth_and_length_boundaries_exhaustively() {
    // Every depth 1..=MAX_DEPTH+1, with and without owner, with and
    // without trailing slash and leading slash.
    for depth in 1..=MAX_DEPTH + 1 {
        for owner in [false, true] {
            for dir in [false, true] {
                for lead in [false, true] {
                    let mut s = String::new();
                    if lead {
                        s.push('/');
                    }
                    if owner {
                        s.push_str("o@");
                    }
                    s.push_str(
                        &(0..depth)
                            .map(|i| std::format!("c{i}"))
                            .collect::<Vec<_>>()
                            .join("/"),
                    );
                    if dir {
                        s.push('/');
                    }
                    let ok = parse_path(&s).is_some();
                    assert_eq!(ok, depth <= MAX_DEPTH, "{s:?}");
                    assert_eq!(ok, oracle(&s), "{s:?}");
                }
            }
        }
    }
    // Component length 1..=NAME_CAP+1 in every position of a 3-deep path
    // (length 0 collapses into slash syntax, covered above).
    for len in 1..=NAME_CAP + 1 {
        let comp = "n".repeat(len);
        for pos in 0..3 {
            let mut parts = std::vec!["a", "b", "c"];
            parts[pos] = &comp;
            let s = parts.join("/");
            let ok = parse_path(&s).is_some();
            assert_eq!(ok, len <= NAME_CAP, "{s:?}");
            assert_eq!(ok, oracle(&s), "{s:?}");
        }
        let s = std::format!("{comp}@x");
        assert_eq!(parse_path(&s).is_some(), len <= NAME_CAP, "{s:?}");
    }
}
