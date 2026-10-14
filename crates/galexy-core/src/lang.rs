//! In-editor language service.
//!
//! Rust, Python, Lisp, JSON, CSV, TOML, and XML.
//!
//! Highlighting, diagnostics, and completion run in the process. The
//! image does not ship rust-analyzer or a Python language server, so
//! the editor is the server: keywords, strings, comments, unmatched
//! delimiters, and a short completion list.

/// Which language a path asks the editor to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    /// No language service.
    Text,
    /// Rust source (`.rs`).
    Rust,
    /// Python source (`.py`).
    Python,
    /// Lisp source (`.lisp`, `.lsp`, `.scm`).
    Lisp,
    /// JSON (`.json`).
    Json,
    /// CSV (`.csv`).
    Csv,
    /// TOML (`.toml`).
    Toml,
    /// XML (`.xml`).
    Xml,
}

impl Lang {
    /// Short name painted on the title and status lines. Empty for text.
    pub const fn name(self) -> &'static [u8] {
        match self {
            Self::Text => b"",
            Self::Rust => b"Rust",
            Self::Python => b"Python",
            Self::Lisp => b"Lisp",
            Self::Json => b"JSON",
            Self::Csv => b"CSV",
            Self::Toml => b"TOML",
            Self::Xml => b"XML",
        }
    }
}

/// How one run of bytes on a line should be colored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Span {
    /// Ordinary text.
    Text,
    /// A keyword or a built-in the completer also offers.
    Keyword,
    /// A string or character literal.
    String,
    /// A comment.
    Comment,
    /// A numeric literal.
    Number,
}

/// Carry between lines: a block comment or a triple-quoted string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HiState {
    mode: u8,
    depth: u8,
}

const GROUND: u8 = 0;
const RUST_BLOCK: u8 = 1;
const PY_TRIPLE_SQ: u8 = 2;
const PY_TRIPLE_DQ: u8 = 3;
const XML_COMMENT: u8 = 4;

impl HiState {
    /// Ground state, nothing open.
    pub const fn new() -> Self {
        Self {
            mode: GROUND,
            depth: 0,
        }
    }
}

impl Default for HiState {
    fn default() -> Self {
        Self::new()
    }
}

/// Picks a language from the path suffix. Anything else is plain text.
pub fn lang_of(path: &[u8]) -> Lang {
    if path.ends_with(b".rs") {
        Lang::Rust
    } else if path.ends_with(b".py") {
        Lang::Python
    } else if path.ends_with(b".lisp") || path.ends_with(b".lsp") || path.ends_with(b".scm") {
        Lang::Lisp
    } else if path.ends_with(b".json") {
        Lang::Json
    } else if path.ends_with(b".csv") {
        Lang::Csv
    } else if path.ends_with(b".toml") {
        Lang::Toml
    } else if path.ends_with(b".xml") {
        Lang::Xml
    } else {
        Lang::Text
    }
}

/// Paints `line` (no newline) and updates `state` for the next line.
pub fn paint_line(lang: Lang, line: &[u8], state: &mut HiState, mut emit: impl FnMut(Span, &[u8])) {
    match lang {
        Lang::Text => {
            if !line.is_empty() {
                emit(Span::Text, line);
            }
        }
        Lang::Rust => paint_rust(line, state, &mut emit),
        Lang::Python => paint_python(line, state, &mut emit),
        Lang::Lisp => paint_words(Lang::Lisp, line, b';', &mut emit),
        Lang::Json => paint_words(Lang::Json, line, 0, &mut emit),
        Lang::Csv => paint_words(Lang::Csv, line, 0, &mut emit),
        Lang::Toml => paint_words(Lang::Toml, line, b'#', &mut emit),
        Lang::Xml => paint_xml(line, state, &mut emit),
    }
}

/// The first problem in `doc`, if the language service has one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Diag {
    /// 0-based line.
    pub line: usize,
    /// Static text. The editor prints it on the status line.
    pub msg: &'static str,
}

/// Scans `doc` for one diagnostic. Plain text has none.
pub fn diagnose(lang: Lang, doc: &[u8]) -> Option<Diag> {
    match lang {
        Lang::Text => None,
        Lang::Rust => diag_rust(doc),
        Lang::Python => diag_python(doc),
        Lang::Lisp => diag_lisp(doc),
        Lang::Json => diag_json(doc),
        Lang::Csv => diag_csv(doc),
        Lang::Toml => diag_toml(doc),
        Lang::Xml => diag_xml(doc),
    }
}

/// One completion. `len` bytes of `bytes` are the word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Compl {
    /// The word, zero-padded.
    pub bytes: [u8; COMPL_LEN],
    /// Significant length.
    pub len: usize,
}

/// Longest word the completer stores.
pub const COMPL_LEN: usize = 24;

impl Compl {
    /// Empty slot.
    pub const fn new() -> Self {
        Self {
            bytes: [0; COMPL_LEN],
            len: 0,
        }
    }

    fn set(&mut self, word: &[u8]) {
        let n = word.len().min(COMPL_LEN);
        self.bytes[..n].copy_from_slice(&word[..n]);
        self.len = n;
    }
}

impl Default for Compl {
    fn default() -> Self {
        Self::new()
    }
}

/// Fills `out` with words that start with `prefix` and are longer than it.
///
/// Keywords come first, then identifiers already in `doc`. Returns how
/// many slots were written.
pub fn complete(lang: Lang, doc: &[u8], prefix: &[u8], out: &mut [Compl]) -> usize {
    if prefix.is_empty() || out.is_empty() {
        return 0;
    }
    let mut n = 0usize;
    if let Some(words) = keywords(lang) {
        for word in words {
            if n == out.len() {
                break;
            }
            if word.starts_with(prefix) && word.len() > prefix.len() {
                push_unique(out, &mut n, word);
            }
        }
    }
    let mut i = 0usize;
    while i < doc.len() && n < out.len() {
        if is_ident_start(doc[i]) {
            let start = i;
            i += 1;
            while i < doc.len() && is_ident_cont(doc[i]) {
                i += 1;
            }
            let word = &doc[start..i];
            if word.starts_with(prefix) && word.len() > prefix.len() && word.len() <= COMPL_LEN {
                push_unique(out, &mut n, word);
            }
        } else {
            i += 1;
        }
    }
    n
}

fn push_unique(out: &mut [Compl], n: &mut usize, word: &[u8]) {
    if *n >= out.len() {
        return;
    }
    for slot in out.iter().take(*n) {
        if &slot.bytes[..slot.len] == word {
            return;
        }
    }
    out[*n].set(word);
    *n += 1;
}

fn keywords(lang: Lang) -> Option<&'static [&'static [u8]]> {
    match lang {
        Lang::Text | Lang::Csv | Lang::Xml => None,
        Lang::Rust => Some(RUST_WORDS),
        Lang::Python => Some(PYTHON_WORDS),
        Lang::Lisp => Some(LISP_WORDS),
        Lang::Json => Some(JSON_WORDS),
        Lang::Toml => Some(TOML_WORDS),
    }
}

const RUST_WORDS: &[&[u8]] = &[
    b"println",
    b"print",
    b"Err",
    b"None",
    b"Ok",
    b"Option",
    b"Result",
    b"Some",
    b"String",
    b"Vec",
    b"as",
    b"async",
    b"await",
    b"break",
    b"const",
    b"continue",
    b"else",
    b"enum",
    b"false",
    b"fn",
    b"for",
    b"if",
    b"impl",
    b"in",
    b"let",
    b"loop",
    b"match",
    b"mod",
    b"mut",
    b"pub",
    b"return",
    b"self",
    b"struct",
    b"trait",
    b"true",
    b"type",
    b"unsafe",
    b"use",
    b"vec",
    b"where",
    b"while",
];

const PYTHON_WORDS: &[&[u8]] = &[
    b"print",
    b"False",
    b"None",
    b"True",
    b"and",
    b"as",
    b"assert",
    b"async",
    b"await",
    b"break",
    b"class",
    b"continue",
    b"def",
    b"del",
    b"elif",
    b"else",
    b"except",
    b"finally",
    b"for",
    b"from",
    b"global",
    b"if",
    b"import",
    b"in",
    b"is",
    b"lambda",
    b"nonlocal",
    b"not",
    b"or",
    b"pass",
    b"raise",
    b"range",
    b"return",
    b"try",
    b"while",
    b"with",
    b"yield",
];

const LISP_WORDS: &[&[u8]] = &[
    b"car",
    b"cdr",
    b"cond",
    b"cons",
    b"define",
    b"defun",
    b"defmacro",
    b"if",
    b"lambda",
    b"let",
    b"list",
    b"nil",
    b"quote",
];

const JSON_WORDS: &[&[u8]] = &[b"false", b"null", b"true"];

const TOML_WORDS: &[&[u8]] = &[b"false", b"true"];

const PY_HEADERS: &[&[u8]] = &[
    b"def", b"class", b"if", b"elif", b"else", b"for", b"while", b"try", b"except", b"finally",
    b"with",
];

fn paint_rust(line: &[u8], state: &mut HiState, emit: &mut impl FnMut(Span, &[u8])) {
    if state.mode == RUST_BLOCK {
        match close_block(line, 0, state.depth) {
            BlockEnd::Open => {
                emit(Span::Comment, line);
                return;
            }
            BlockEnd::Closed { at, depth } => {
                emit(Span::Comment, &line[..at]);
                state.mode = GROUND;
                state.depth = depth;
                paint_rust_ground(&line[at..], state, emit);
                return;
            }
        }
    }
    state.mode = GROUND;
    state.depth = 0;
    paint_rust_ground(line, state, emit);
}

fn paint_rust_ground(line: &[u8], state: &mut HiState, emit: &mut impl FnMut(Span, &[u8])) {
    let bytes = line;
    let mut i = 0usize;
    while i < bytes.len() {
        let rest = &bytes[i..];
        if rest.starts_with(b"//") {
            emit(Span::Comment, &bytes[i..]);
            return;
        }
        if rest.starts_with(b"/*") {
            match close_block(bytes, i, 1) {
                BlockEnd::Open => {
                    emit(Span::Comment, &bytes[i..]);
                    state.mode = RUST_BLOCK;
                    state.depth = block_depth_at_end(&bytes[i..]);
                    return;
                }
                BlockEnd::Closed { at, depth: _ } => {
                    emit(Span::Comment, &bytes[i..at]);
                    i = at;
                    continue;
                }
            }
        }
        let b = bytes[i];
        if b == b'"' || b == b'\'' {
            let end = close_rust_quote(bytes, i, b);
            emit(Span::String, &bytes[i..end]);
            i = end;
            continue;
        }
        if b.is_ascii_digit() {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_cont(bytes[i]) {
                i += 1;
            }
            emit(Span::Number, &bytes[start..i]);
            continue;
        }
        if is_ident_start(b) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_cont(bytes[i]) {
                i += 1;
            }
            let word = &bytes[start..i];
            let kind = if is_keyword(Lang::Rust, word) {
                Span::Keyword
            } else {
                Span::Text
            };
            emit(kind, word);
            continue;
        }
        let start = i;
        i += 1;
        while i < bytes.len() && !rust_boundary(bytes, i) {
            i += 1;
        }
        emit(Span::Text, &bytes[start..i]);
    }
}

fn rust_boundary(bytes: &[u8], i: usize) -> bool {
    let b = bytes[i];
    b == b'"'
        || b == b'\''
        || b.is_ascii_digit()
        || is_ident_start(b)
        || bytes[i..].starts_with(b"//")
        || bytes[i..].starts_with(b"/*")
}

fn close_rust_quote(bytes: &[u8], open: usize, quote: u8) -> usize {
    let mut i = open + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if bytes[i] == quote {
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

enum BlockEnd {
    Open,
    Closed { at: usize, depth: u8 },
}

fn close_block(bytes: &[u8], from: usize, depth: u8) -> BlockEnd {
    let mut i = from;
    let mut depth = depth.max(1);
    if bytes[from..].starts_with(b"/*") {
        i = from + 2;
    }
    while i < bytes.len() {
        if bytes[i..].starts_with(b"/*") {
            depth = depth.saturating_add(1);
            i += 2;
            continue;
        }
        if bytes[i..].starts_with(b"*/") {
            depth = depth.saturating_sub(1);
            i += 2;
            if depth == 0 {
                return BlockEnd::Closed { at: i, depth: 0 };
            }
            continue;
        }
        i += 1;
    }
    BlockEnd::Open
}

fn block_depth_at_end(bytes: &[u8]) -> u8 {
    let mut depth = 1u8;
    let mut i = 2usize;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"/*") {
            depth = depth.saturating_add(1);
            i += 2;
            continue;
        }
        if bytes[i..].starts_with(b"*/") {
            depth = depth.saturating_sub(1);
            i += 2;
            continue;
        }
        i += 1;
    }
    depth.max(1)
}

fn paint_python(line: &[u8], state: &mut HiState, emit: &mut impl FnMut(Span, &[u8])) {
    let mut i = 0usize;
    if state.mode == PY_TRIPLE_SQ || state.mode == PY_TRIPLE_DQ {
        let q = if state.mode == PY_TRIPLE_SQ {
            b'\''
        } else {
            b'"'
        };
        if let Some(end) = find_triple(line, 0, q) {
            emit(Span::String, &line[..end]);
            state.mode = GROUND;
            i = end;
        } else {
            emit(Span::String, line);
            return;
        }
    }
    while i < line.len() {
        let b = line[i];
        if b == b'#' {
            emit(Span::Comment, &line[i..]);
            return;
        }
        if b == b'"' || b == b'\'' {
            if let Some(end) = take_py_string(line, i, state) {
                emit(Span::String, &line[i..end]);
                if state.mode != GROUND {
                    return;
                }
                i = end;
                continue;
            }
        }
        if b.is_ascii_digit() {
            let start = i;
            i += 1;
            while i < line.len() && is_ident_cont(line[i]) {
                i += 1;
            }
            emit(Span::Number, &line[start..i]);
            continue;
        }
        if is_ident_start(b) {
            let start = i;
            i += 1;
            while i < line.len() && is_ident_cont(line[i]) {
                i += 1;
            }
            let word = &line[start..i];
            let kind = if is_keyword(Lang::Python, word) {
                Span::Keyword
            } else {
                Span::Text
            };
            emit(kind, word);
            continue;
        }
        let start = i;
        i += 1;
        while i < line.len() && !py_boundary(line[i]) {
            i += 1;
        }
        emit(Span::Text, &line[start..i]);
    }
}

fn py_boundary(b: u8) -> bool {
    b == b'#' || b == b'"' || b == b'\'' || b.is_ascii_digit() || is_ident_start(b)
}

/// End index of the string starting at `open`, or the line length when
/// a triple quote stays open. Updates `state` when the quote spans lines.
fn take_py_string(line: &[u8], open: usize, state: &mut HiState) -> Option<usize> {
    let q = line[open];
    if line[open..].starts_with(&[q, q, q]) {
        state.mode = if q == b'\'' {
            PY_TRIPLE_SQ
        } else {
            PY_TRIPLE_DQ
        };
        if let Some(end) = find_triple(line, open + 3, q) {
            state.mode = GROUND;
            Some(end)
        } else {
            Some(line.len())
        }
    } else {
        Some(close_rust_quote(line, open, q))
    }
}

fn find_triple(line: &[u8], from: usize, q: u8) -> Option<usize> {
    let mut i = from;
    while i + 2 < line.len() {
        if line[i] == q && line[i + 1] == q && line[i + 2] == q {
            return Some(i + 3);
        }
        i += 1;
    }
    None
}

fn is_keyword(lang: Lang, word: &[u8]) -> bool {
    keywords(lang).is_some_and(|words| words.contains(&word))
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn diag_rust(doc: &[u8]) -> Option<Diag> {
    let mut stack = [(0u8, 0u16); 32];
    let mut sp = 0usize;
    let mut line = 0u16;
    let mut i = 0usize;
    let mut mode = GROUND;
    let mut depth = 0u8;
    while i < doc.len() {
        let b = doc[i];
        if mode == RUST_BLOCK {
            if doc[i..].starts_with(b"/*") {
                depth = depth.saturating_add(1);
                i += 2;
                continue;
            }
            if doc[i..].starts_with(b"*/") {
                depth = depth.saturating_sub(1);
                i += 2;
                if depth == 0 {
                    mode = GROUND;
                }
                continue;
            }
            if b == b'\n' {
                line = line.saturating_add(1);
            }
            i += 1;
            continue;
        }
        if mode == 8 {
            // rust string
            if b == b'\n' {
                return Some(Diag {
                    line: line as usize,
                    msg: "unclosed string",
                });
            }
            if b == b'\\' && i + 1 < doc.len() && doc[i + 1] != b'\n' {
                i += 2;
                continue;
            }
            if b == b'"' {
                mode = GROUND;
            }
            i += 1;
            continue;
        }
        if mode == 9 {
            if b == b'\n' {
                return Some(Diag {
                    line: line as usize,
                    msg: "unclosed string",
                });
            }
            if b == b'\\' && i + 1 < doc.len() && doc[i + 1] != b'\n' {
                i += 2;
                continue;
            }
            if b == b'\'' {
                mode = GROUND;
            }
            i += 1;
            continue;
        }
        if b == b'\n' {
            line = line.saturating_add(1);
            i += 1;
            continue;
        }
        if doc[i..].starts_with(b"//") {
            while i < doc.len() && doc[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if doc[i..].starts_with(b"/*") {
            mode = RUST_BLOCK;
            depth = 1;
            i += 2;
            continue;
        }
        if b == b'"' {
            mode = 8;
            i += 1;
            continue;
        }
        if b == b'\'' {
            mode = 9;
            i += 1;
            continue;
        }
        if let Some(msg) = push_delim(&mut stack, &mut sp, b, line) {
            return Some(Diag {
                line: line as usize,
                msg,
            });
        }
        i += 1;
    }
    if mode == RUST_BLOCK {
        return Some(Diag {
            line: line as usize,
            msg: "unclosed comment",
        });
    }
    if mode == 8 || mode == 9 {
        return Some(Diag {
            line: line as usize,
            msg: "unclosed string",
        });
    }
    open_diag(&stack, sp)
}

fn diag_python(doc: &[u8]) -> Option<Diag> {
    let delim = diag_python_delim(doc);
    let shape = diag_python_shape(doc);
    match (delim, shape) {
        (Some(a), Some(b)) if b.line < a.line => Some(b),
        (Some(a), _) => Some(a),
        (None, other) => other,
    }
}

fn diag_python_delim(doc: &[u8]) -> Option<Diag> {
    let mut stack = [(0u8, 0u16); 32];
    let mut sp = 0usize;
    let mut line = 0u16;
    let mut i = 0usize;
    let mut mode = GROUND;
    while i < doc.len() {
        let b = doc[i];
        if mode == PY_TRIPLE_SQ || mode == PY_TRIPLE_DQ {
            let q = if mode == PY_TRIPLE_SQ { b'\'' } else { b'"' };
            if b == b'\n' {
                line = line.saturating_add(1);
                i += 1;
                continue;
            }
            if doc[i..].starts_with(&[q, q, q]) {
                mode = GROUND;
                i += 3;
                continue;
            }
            i += 1;
            continue;
        }
        if mode == 8 || mode == 9 {
            let q = if mode == 8 { b'\'' } else { b'"' };
            if b == b'\n' {
                return Some(Diag {
                    line: line as usize,
                    msg: "unclosed string",
                });
            }
            if b == b'\\' && i + 1 < doc.len() && doc[i + 1] != b'\n' {
                i += 2;
                continue;
            }
            if b == q {
                mode = GROUND;
            }
            i += 1;
            continue;
        }
        if b == b'\n' {
            line = line.saturating_add(1);
            i += 1;
            continue;
        }
        if b == b'#' {
            while i < doc.len() && doc[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'\'' || b == b'"' {
            if doc[i..].starts_with(&[b, b, b]) {
                mode = if b == b'\'' {
                    PY_TRIPLE_SQ
                } else {
                    PY_TRIPLE_DQ
                };
                i += 3;
            } else {
                mode = if b == b'\'' { 8 } else { 9 };
                i += 1;
            }
            continue;
        }
        if let Some(msg) = push_delim(&mut stack, &mut sp, b, line) {
            return Some(Diag {
                line: line as usize,
                msg,
            });
        }
        i += 1;
    }
    if mode == PY_TRIPLE_SQ || mode == PY_TRIPLE_DQ || mode == 8 || mode == 9 {
        return Some(Diag {
            line: line as usize,
            msg: "unclosed string",
        });
    }
    open_diag(&stack, sp)
}

fn diag_python_shape(doc: &[u8]) -> Option<Diag> {
    let mut state = HiState::new();
    let mut prev_indent = 0usize;
    let mut prev_colon = false;
    for (line_no, line) in split_lines(doc).enumerate() {
        let inside = state.mode == PY_TRIPLE_SQ || state.mode == PY_TRIPLE_DQ;
        paint_python(line, &mut state, &mut |_, _| {});
        if !inside {
            if let Some(msg) = py_line(line, &mut prev_indent, &mut prev_colon) {
                return Some(Diag { line: line_no, msg });
            }
        }
    }
    None
}

fn py_line(line: &[u8], prev_indent: &mut usize, prev_colon: &mut bool) -> Option<&'static str> {
    let mut indent = 0usize;
    while indent < line.len() && line[indent] == b' ' {
        indent += 1;
    }
    let mut code = [0u8; 160];
    let mut n = 0usize;
    let mut i = 0usize;
    while i < line.len() && n < code.len() {
        let b = line[i];
        if b == b'#' {
            break;
        }
        if b == b'\'' || b == b'"' {
            if line[i..].starts_with(&[b, b, b]) {
                if let Some(end) = find_triple(line, i + 3, b) {
                    i = end;
                    continue;
                }
                break;
            }
            i = close_rust_quote(line, i, b);
            continue;
        }
        code[n] = b;
        n += 1;
        i += 1;
    }
    while n > 0 && code[n - 1] == b' ' {
        n -= 1;
    }
    if n == 0 {
        return None;
    }
    if *prev_colon && indent <= *prev_indent {
        return Some("expected an indent");
    }
    let header = first_ident(&code[..n]).is_some_and(|word| PY_HEADERS.contains(&word));
    let colon = code[n - 1] == b':';
    if header && !colon {
        *prev_indent = indent;
        *prev_colon = false;
        return Some("expected ':'");
    }
    *prev_indent = indent;
    *prev_colon = colon;
    None
}

fn first_ident(code: &[u8]) -> Option<&[u8]> {
    let mut i = 0usize;
    while i < code.len() && code[i] == b' ' {
        i += 1;
    }
    if i >= code.len() || !is_ident_start(code[i]) {
        return None;
    }
    let start = i;
    i += 1;
    while i < code.len() && is_ident_cont(code[i]) {
        i += 1;
    }
    Some(&code[start..i])
}

fn split_lines(doc: &[u8]) -> SplitLines<'_> {
    SplitLines { doc, i: 0 }
}

struct SplitLines<'a> {
    doc: &'a [u8],
    i: usize,
}

impl<'a> Iterator for SplitLines<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.i > self.doc.len() {
            return None;
        }
        if self.i == self.doc.len() {
            if self.doc.is_empty() || self.doc.ends_with(b"\n") {
                self.i = self.doc.len() + 1;
                if self.doc.ends_with(b"\n") {
                    return Some(b"");
                }
            }
            self.i = self.doc.len() + 1;
            return None;
        }
        let rest = &self.doc[self.i..];
        let n = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
        let line = &rest[..n];
        self.i += n + 1;
        if n == rest.len() {
            self.i = self.doc.len();
        }
        Some(line)
    }
}

fn push_delim(stack: &mut [(u8, u16)], sp: &mut usize, b: u8, line: u16) -> Option<&'static str> {
    match b {
        b'(' | b'[' | b'{' => {
            if *sp < stack.len() {
                stack[*sp] = (b, line);
                *sp += 1;
            }
            None
        }
        b')' | b']' | b'}' => {
            let want = match b {
                b')' => b'(',
                b']' => b'[',
                _ => b'{',
            };
            if *sp == 0 || stack[*sp - 1].0 != want {
                return Some(unexpected(b));
            }
            *sp -= 1;
            None
        }
        _ => None,
    }
}

fn unexpected(b: u8) -> &'static str {
    match b {
        b')' => "unexpected ')'",
        b']' => "unexpected ']'",
        _ => "unexpected '}'",
    }
}

/// `#`, `;`, or no comment. Quotes, numbers, and that language's words.
fn paint_words(lang: Lang, line: &[u8], comment: u8, emit: &mut impl FnMut(Span, &[u8])) {
    let mut i = 0usize;
    while i < line.len() {
        let b = line[i];
        if comment != 0 && b == comment {
            emit(Span::Comment, &line[i..]);
            return;
        }
        if b == b'"' || (b == b'\'' && lang == Lang::Toml) {
            let end = close_rust_quote(line, i, b);
            emit(Span::String, &line[i..end]);
            i = end;
            continue;
        }
        if number_at(line, i) {
            let start = i;
            if line[i] == b'-' {
                i += 1;
            }
            while i < line.len() && (is_ident_cont(line[i]) || line[i] == b'.') {
                i += 1;
            }
            emit(Span::Number, &line[start..i]);
            continue;
        }
        if is_ident_start(b) {
            let start = i;
            i += 1;
            while i < line.len() && is_ident_cont(line[i]) {
                i += 1;
            }
            let word = &line[start..i];
            let kind = if is_keyword(lang, word) {
                Span::Keyword
            } else {
                Span::Text
            };
            emit(kind, word);
            continue;
        }
        let start = i;
        i += 1;
        while i < line.len() && !word_boundary(lang, line, i, comment) {
            i += 1;
        }
        emit(Span::Text, &line[start..i]);
    }
}

fn number_at(line: &[u8], i: usize) -> bool {
    let b = line[i];
    if b.is_ascii_digit() {
        return true;
    }
    b == b'-' && i + 1 < line.len() && line[i + 1].is_ascii_digit()
}

fn word_boundary(lang: Lang, line: &[u8], i: usize, comment: u8) -> bool {
    let b = line[i];
    (comment != 0 && b == comment)
        || b == b'"'
        || (b == b'\'' && lang == Lang::Toml)
        || number_at(line, i)
        || is_ident_start(b)
}

fn paint_xml(line: &[u8], state: &mut HiState, emit: &mut impl FnMut(Span, &[u8])) {
    let mut i = 0usize;
    if state.mode == XML_COMMENT {
        if let Some(end) = find_slice(line, b"-->") {
            emit(Span::Comment, &line[..end]);
            state.mode = GROUND;
            i = end;
        } else {
            emit(Span::Comment, line);
            return;
        }
    }
    while i < line.len() {
        if line[i..].starts_with(b"<!--") {
            if let Some(end) = find_slice(&line[i + 4..], b"-->") {
                let at = i + 4 + end;
                emit(Span::Comment, &line[i..at]);
                i = at;
                continue;
            }
            emit(Span::Comment, &line[i..]);
            state.mode = XML_COMMENT;
            return;
        }
        let b = line[i];
        if b == b'"' || b == b'\'' {
            let end = close_rust_quote(line, i, b);
            emit(Span::String, &line[i..end]);
            i = end;
            continue;
        }
        if b == b'<' {
            let start = i;
            i += 1;
            if i < line.len() && line[i] == b'/' {
                i += 1;
            }
            while i < line.len() && (is_ident_cont(line[i]) || line[i] == b':' || line[i] == b'-') {
                i += 1;
            }
            emit(Span::Keyword, &line[start..i]);
            continue;
        }
        let start = i;
        i += 1;
        while i < line.len()
            && line[i] != b'<'
            && line[i] != b'"'
            && line[i] != b'\''
            && !line[i..].starts_with(b"<!--")
        {
            i += 1;
        }
        emit(Span::Text, &line[start..i]);
    }
}

fn find_slice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let last = hay.len() - needle.len();
    let mut i = 0usize;
    while i <= last {
        if &hay[i..i + needle.len()] == needle {
            return Some(i + needle.len());
        }
        i += 1;
    }
    None
}

fn diag_lisp(doc: &[u8]) -> Option<Diag> {
    diag_source(doc, b';', true, false, false, false)
}

fn diag_json(doc: &[u8]) -> Option<Diag> {
    diag_source(doc, 0, false, true, true, false)
}

fn diag_toml(doc: &[u8]) -> Option<Diag> {
    diag_source(doc, b'#', false, true, false, true)
}

fn diag_csv(doc: &[u8]) -> Option<Diag> {
    diag_source(doc, 0, false, false, false, false)
}

fn diag_xml(doc: &[u8]) -> Option<Diag> {
    let mut line = 0u16;
    let mut i = 0usize;
    let mut comment = false;
    let mut quote = 0u8;
    let mut angles = 0i32;
    let mut angle_line = 0u16;
    while i < doc.len() {
        let b = doc[i];
        if b == b'\n' {
            if quote != 0 {
                return Some(Diag {
                    line: line as usize,
                    msg: "unclosed string",
                });
            }
            line = line.saturating_add(1);
            i += 1;
            continue;
        }
        if comment {
            if doc[i..].starts_with(b"-->") {
                comment = false;
                i += 3;
            } else {
                i += 1;
            }
            continue;
        }
        if quote != 0 {
            if b == b'\\' && i + 1 < doc.len() && doc[i + 1] != b'\n' {
                i += 2;
                continue;
            }
            if b == quote {
                quote = 0;
            }
            i += 1;
            continue;
        }
        if doc[i..].starts_with(b"<!--") {
            comment = true;
            i += 4;
            continue;
        }
        if b == b'"' || b == b'\'' {
            quote = b;
            i += 1;
            continue;
        }
        if b == b'<' {
            if angles == 0 {
                angle_line = line;
            }
            angles += 1;
        } else if b == b'>' {
            if angles == 0 {
                return Some(Diag {
                    line: line as usize,
                    msg: "unexpected '>'",
                });
            }
            angles -= 1;
        }
        i += 1;
    }
    if comment {
        return Some(Diag {
            line: line as usize,
            msg: "unclosed comment",
        });
    }
    if quote != 0 {
        return Some(Diag {
            line: line as usize,
            msg: "unclosed string",
        });
    }
    if angles > 0 {
        return Some(Diag {
            line: angle_line as usize,
            msg: "unclosed '<'",
        });
    }
    None
}

/// Strings, an optional line comment, and the delimiters the caller asked for.
fn diag_source(
    doc: &[u8],
    comment: u8,
    parens: bool,
    brackets: bool,
    braces: bool,
    single: bool,
) -> Option<Diag> {
    let mut stack = [(0u8, 0u16); 32];
    let mut sp = 0usize;
    let mut line = 0u16;
    let mut i = 0usize;
    let mut quote = 0u8;
    while i < doc.len() {
        let b = doc[i];
        if quote != 0 {
            if b == b'\n' {
                return Some(Diag {
                    line: line as usize,
                    msg: "unclosed string",
                });
            }
            if b == b'\\' && i + 1 < doc.len() && doc[i + 1] != b'\n' {
                i += 2;
                continue;
            }
            if b == quote {
                quote = 0;
            }
            i += 1;
            continue;
        }
        if b == b'\n' {
            line = line.saturating_add(1);
            i += 1;
            continue;
        }
        if comment != 0 && b == comment {
            while i < doc.len() && doc[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'"' || (single && b == b'\'') {
            quote = b;
            i += 1;
            continue;
        }
        let track = (parens && (b == b'(' || b == b')'))
            || (brackets && (b == b'[' || b == b']'))
            || (braces && (b == b'{' || b == b'}'));
        if track {
            if let Some(msg) = push_delim(&mut stack, &mut sp, b, line) {
                return Some(Diag {
                    line: line as usize,
                    msg,
                });
            }
        }
        i += 1;
    }
    if quote != 0 {
        return Some(Diag {
            line: line as usize,
            msg: "unclosed string",
        });
    }
    open_diag(&stack, sp)
}

fn open_diag(stack: &[(u8, u16)], sp: usize) -> Option<Diag> {
    if sp == 0 {
        return None;
    }
    let (kind, line) = stack[sp - 1];
    let msg = match kind {
        b'(' => "unclosed '('",
        b'[' => "unclosed '['",
        _ => "unclosed '{'",
    };
    Some(Diag {
        line: line as usize,
        msg,
    })
}
