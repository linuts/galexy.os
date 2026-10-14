use super::{complete, diagnose, lang_of, paint_line, Compl, HiState, Lang, Span};

fn kinds(lang: Lang, lines: &[&[u8]]) -> [[Span; 48]; 4] {
    let mut state = HiState::new();
    let mut out = [[Span::Text; 48]; 4];
    for (row, line) in lines.iter().enumerate() {
        let mut n = 0usize;
        paint_line(lang, line, &mut state, |span, bytes| {
            for _ in bytes {
                if n < out[row].len() {
                    out[row][n] = span;
                    n += 1;
                }
            }
        });
    }
    out
}

#[test]
fn path_picks_the_language() {
    assert_eq!(lang_of(b"note"), Lang::Text);
    assert_eq!(lang_of(b"src/main.rs"), Lang::Rust);
    assert_eq!(lang_of(b"hello.py"), Lang::Python);
    assert_eq!(lang_of(b"hello.pyx"), Lang::Text);
}

#[test]
fn rust_keywords_strings_and_comments() {
    let painted = kinds(Lang::Rust, &[b"fn x // hi", b"a /*", b"go */ z"]);
    assert_eq!(painted[0][0], Span::Keyword);
    assert_eq!(painted[0][1], Span::Keyword);
    assert_eq!(painted[0][2], Span::Text);
    assert_eq!(painted[0][3], Span::Text);
    assert_eq!(painted[0][5], Span::Comment);
    assert_eq!(painted[1][0], Span::Text);
    assert_eq!(painted[1][2], Span::Comment);
    assert_eq!(painted[2][0], Span::Comment);
    assert_eq!(painted[2][5], Span::Text);
}

#[test]
fn python_hash_and_keyword() {
    let painted = kinds(Lang::Python, &[b"def f(): # no"]);
    assert_eq!(painted[0][0], Span::Keyword);
    assert_eq!(painted[0][3], Span::Text);
    assert_eq!(painted[0][9], Span::Comment);
}

#[test]
fn rust_unclosed_brace_and_string() {
    let open = diagnose(Lang::Rust, b"fn main() {\n").unwrap();
    assert_eq!(open.line, 0);
    assert_eq!(open.msg, "unclosed '{'");
    let string = diagnose(Lang::Rust, b"let s = \"hi\n").unwrap();
    assert_eq!(string.msg, "unclosed string");
    assert!(diagnose(Lang::Rust, b"fn main() {}\n").is_none());
    assert_eq!(diagnose(Lang::Rust, b"}\n").unwrap().msg, "unexpected '}'");
}

#[test]
fn python_colon_and_indent() {
    assert_eq!(
        diagnose(Lang::Python, b"def f()\n").unwrap().msg,
        "expected ':'"
    );
    assert_eq!(
        diagnose(Lang::Python, b"def f():\npass\n").unwrap().msg,
        "expected an indent"
    );
    assert!(diagnose(Lang::Python, b"def f():\n    pass\n").is_none());
    assert!(diagnose(Lang::Python, b"def f():\n\n    pass\n").is_none());
}

#[test]
fn complete_println_and_a_word_in_the_file() {
    let mut out = [Compl::new(); 4];
    let n = complete(Lang::Rust, b"pri", b"pri", &mut out);
    assert!(n >= 1);
    assert_eq!(&out[0].bytes[..out[0].len], b"println");
    let n = complete(Lang::Text, b"hello helper", b"he", &mut out);
    assert!(n >= 2);
    assert_eq!(&out[0].bytes[..out[0].len], b"hello");
    assert_eq!(&out[1].bytes[..out[1].len], b"helper");
    assert_eq!(complete(Lang::Rust, b"fn", b"", &mut out), 0);
}
