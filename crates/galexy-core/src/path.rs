//! galfs path grammar: pure parsing, no filesystem.
//!
//! The kernel (`sched::galfs`) wraps [`parse_path`] and maps `None` to
//! `SysError::BadValue`; the grammar itself lives here so the host can
//! enumerate it exhaustively (`path_test.rs`).
//!
//! Grammar (see `docs/GALFS.md`):
//!
//! ```text
//! path      := "/"? first ("/" component)* "/"?
//! first     := component | owner "@" component | owner "@"   (only as "owner@/")
//! component := [A-Za-z0-9._-]{1,64} minus "." and ".."
//! owner     := component
//! ```
//!
//! A trailing `/` marks a directory. `owner@/` names that owner's root
//! object and carries no components. At most [`MAX_DEPTH`] components.

/// Longest path, in components.
pub const MAX_DEPTH: usize = 8;

/// Longest single component (object name), in bytes.
pub const NAME_CAP: usize = 64;

/// True when `name` is a legal single component: 1..=64 bytes of
/// `[A-Za-z0-9._-]`, and not `.` or `..`.
pub fn component_ok(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= NAME_CAP
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

/// A path after the optional leading `/` and `owner@` on the first component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedPath<'a> {
    /// `owner` of `owner@...`, when present.
    pub owner: Option<&'a str>,
    /// Components in order; only `comps[..n]` are meaningful.
    pub comps: [&'a str; MAX_DEPTH],
    /// Number of components (0 only for `owner@/`).
    pub n: usize,
    /// True when the path ended in `/` (names a directory).
    pub dir: bool,
}

impl<'a> ParsedPath<'a> {
    /// The meaningful components.
    pub fn components(&self) -> &[&'a str] {
        &self.comps[..self.n]
    }
}

/// Splits `name`. A trailing slash marks a directory. The first component
/// may be `owner@leaf`. `owner@/` (empty leaf, directory) names that
/// actor's root object — the login/grant path. `.` and `..` are rejected.
/// Returns `None` for anything outside the grammar.
pub fn parse_path(name: &str) -> Option<ParsedPath<'_>> {
    let name = name.strip_prefix('/').unwrap_or(name);
    let (body, dir) = if let Some(stripped) = name.strip_suffix('/') {
        if stripped.is_empty() || stripped.ends_with('/') {
            return None;
        }
        (stripped, true)
    } else {
        (name, false)
    };
    if body.is_empty() {
        return None;
    }
    let mut comps = [""; MAX_DEPTH];
    let mut n = 0usize;
    let mut owner = None;
    for (i, comp) in body.split('/').enumerate() {
        if comp.is_empty() || n >= MAX_DEPTH {
            return None;
        }
        if i == 0 {
            if let Some((own, leaf)) = comp.split_once('@') {
                if own.is_empty() || leaf.contains('@') || !component_ok(own) {
                    return None;
                }
                // `eve@/` → actor root (no components under the root).
                if leaf.is_empty() {
                    if !dir || body.contains('/') {
                        return None;
                    }
                    owner = Some(own);
                    break;
                }
                if !component_ok(leaf) {
                    return None;
                }
                owner = Some(own);
                comps[n] = leaf;
                n += 1;
                continue;
            }
        } else if comp.contains('@') {
            return None;
        }
        if !component_ok(comp) {
            return None;
        }
        comps[n] = comp;
        n += 1;
    }
    if n == 0 && owner.is_none() {
        return None;
    }
    Some(ParsedPath {
        owner,
        comps,
        n,
        dir,
    })
}
