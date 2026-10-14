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
    assert_eq!(lang_of(b"a.lisp"), Lang::Lisp);
    assert_eq!(lang_of(b"a.lsp"), Lang::Lisp);
    assert_eq!(lang_of(b"a.scm"), Lang::Lisp);
    assert_eq!(lang_of(b"a.json"), Lang::Json);
    assert_eq!(lang_of(b"a.csv"), Lang::Csv);
    assert_eq!(lang_of(b"a.toml"), Lang::Toml);
    assert_eq!(lang_of(b"a.xml"), Lang::Xml);
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
fn json_toml_lisp_csv_and_xml() {
    let json = kinds(Lang::Json, &[b"{\"ok\": true}"]);
    assert_eq!(json[0][0], Span::Text);
    assert_eq!(json[0][2], Span::String);
    assert_eq!(json[0][7], Span::Keyword);
    let toml = kinds(Lang::Toml, &[b"name = \"x\" # note"]);
    assert_eq!(toml[0][0], Span::Text);
    assert_eq!(toml[0][7], Span::String);
    assert_eq!(toml[0][11], Span::Comment);
    let lisp = kinds(Lang::Lisp, &[b"(defun x) ; go"]);
    assert_eq!(lisp[0][1], Span::Keyword);
    assert_eq!(lisp[0][11], Span::Comment);
    let csv = kinds(Lang::Csv, &[b"\"a\",12"]);
    assert_eq!(csv[0][0], Span::String);
    assert_eq!(csv[0][3], Span::Text);
    assert_eq!(csv[0][4], Span::Number);
    let xml = kinds(Lang::Xml, &[b"<item>x</item>"]);
    assert_eq!(xml[0][0], Span::Keyword);
    assert_eq!(xml[0][1], Span::Keyword);
    assert!(diagnose(Lang::Lisp, b"(defun x\n")
        .unwrap()
        .msg
        .contains("unclosed"));
    assert_eq!(
        diagnose(Lang::Json, b"{\"a\": \"hi\n").unwrap().msg,
        "unclosed string"
    );
    assert!(diagnose(Lang::Json, b"{\"a\": true}\n").is_none());
    assert_eq!(
        diagnose(Lang::Csv, b"\"ab\n").unwrap().msg,
        "unclosed string"
    );
    assert_eq!(
        diagnose(Lang::Xml, b"<!-- open\n").unwrap().msg,
        "unclosed comment"
    );
    assert_eq!(
        diagnose(Lang::Toml, b"[name\n").unwrap().msg,
        "unclosed '['"
    );
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
