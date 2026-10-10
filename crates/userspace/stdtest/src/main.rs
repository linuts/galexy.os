//! stdtest — `std` conformance on `x86_64-unknown-galexy` (Milestone 70).
//!
//! A plain `fn main()` program, no `#![feature]`, no `galexy-rt`: it
//! talks to the OS only through `std`, so everything here is the PAL in
//! `std-pal/`. Threads and `Mutex`/`Condvar`/`thread_local!`, `std::fs`
//! on galfs and the ramdisk, `std::process::Command` with a captured
//! stdout pipe, `Instant`/`SystemTime`, `env`, `HashMap`, and the
//! unmodified `toml` crate from crates.io. Each check prints nothing on
//! success; the first failure prints `stdtest: FAIL <name>` and exits 1.
//! The last line is `stdtest: passed (<n> checks)` — the `test-std`
//! kernel and the boot suite look for it.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, thread};

use serde::{Deserialize, Serialize};

static CHECKS: AtomicUsize = AtomicUsize::new(0);
static TLS_DROPS: AtomicUsize = AtomicUsize::new(0);

fn check(name: &str, ok: bool) {
    if !ok {
        println!("stdtest: FAIL {name}");
        std::process::exit(1);
    }
    CHECKS.fetch_add(1, Ordering::Relaxed);
}

fn check_eq<T: PartialEq + std::fmt::Debug>(name: &str, got: T, want: T) {
    if got != want {
        println!("stdtest: FAIL {name}: got {got:?}, want {want:?}");
        std::process::exit(1);
    }
    CHECKS.fetch_add(1, Ordering::Relaxed);
}

struct DropCounter;

impl Drop for DropCounter {
    fn drop(&mut self) {
        TLS_DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

thread_local! {
    // Not `const`: a lazily initialised slot is the path that registers
    // a destructor through the PAL's `register_dtor`.
    #[allow(clippy::missing_const_for_thread_local)]
    static SLOT: std::cell::RefCell<(u32, Option<DropCounter>)> =
        std::cell::RefCell::new((0, None));
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Config {
    name: String,
    port: u16,
    tags: Vec<String>,
    limits: Limits,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Limits {
    threads: u32,
    ratio: f64,
}

const DOC: &str = r#"
name = "galexy"
port = 8080
tags = ["cap", "std", "toml"]

[limits]
threads = 16
ratio = 0.75
"#;

fn env_checks() {
    check_eq("consts::OS", env::consts::OS, "galexy");
    check_eq("consts::FAMILY", env::consts::FAMILY, "");
    let args: Vec<String> = env::args().collect();
    check_eq("args[0]", args.first().map(String::as_str), Some("stdtest"));
    check_eq("args.len", args.len(), 1);
    check("vars empty", env::vars().next().is_none());
    check("var missing", env::var("HOME").is_err());
    check_eq("current_dir", env::current_dir().ok(), Some("/".into()));
}

fn thread_checks() {
    let shared = Arc::new(Mutex::new(Vec::<u32>::new()));
    let mut handles = Vec::new();
    for k in 0..4u32 {
        let shared = Arc::clone(&shared);
        handles.push(thread::spawn(move || {
            SLOT.with(|s| *s.borrow_mut() = (k, Some(DropCounter)));
            for i in 0..100u32 {
                shared.lock().unwrap().push(k * 1000 + i);
                if i % 25 == 0 {
                    thread::yield_now();
                }
            }
            SLOT.with(|s| s.borrow().0)
        }));
    }
    let mut sum = 0u32;
    for h in handles {
        sum += h.join().unwrap();
    }
    check_eq("join values", sum, 6);
    let v = shared.lock().unwrap();
    check_eq("mutex pushes", v.len(), 400);
    let mut sorted = v.clone();
    sorted.sort_unstable();
    sorted.dedup();
    check_eq("mutex no lost pushes", sorted.len(), 400);
    drop(v);
    check_eq("tls destructors", TLS_DROPS.load(Ordering::SeqCst), 4);

    // Condvar handshake, both directions.
    let pair = Arc::new((Mutex::new(false), Condvar::new()));
    let pair2 = Arc::clone(&pair);
    let t = thread::spawn(move || {
        let (lock, cv) = &*pair2;
        thread::sleep(Duration::from_millis(5));
        *lock.lock().unwrap() = true;
        cv.notify_one();
    });
    {
        let (lock, cv) = &*pair;
        let guard = cv
            .wait_while(lock.lock().unwrap(), |ready| !*ready)
            .unwrap();
        check("condvar notified", *guard);
    }
    t.join().unwrap();
    {
        let (lock, cv) = &*pair;
        let (_g, res) = cv
            .wait_timeout(lock.lock().unwrap(), Duration::from_millis(10))
            .unwrap();
        check("condvar timeout", res.timed_out());
    }

    // Scoped threads borrow the stack.
    let data: Vec<u64> = (1..=1000).collect();
    let (left, right) = data.split_at(500);
    let total = thread::scope(|s| {
        let a = s.spawn(|| left.iter().sum::<u64>());
        let b = s.spawn(|| right.iter().sum::<u64>());
        a.join().unwrap() + b.join().unwrap()
    });
    check_eq("scoped sum", total, 500_500);

    // Named thread, builder stack size, and park/unpark.
    let main_thread = thread::current();
    let h = thread::Builder::new()
        .name("worker".into())
        .stack_size(32 * 1024)
        .spawn(move || {
            let me = thread::current().name().map(str::to_owned);
            main_thread.unpark();
            me
        })
        .unwrap();
    thread::park_timeout(Duration::from_millis(500));
    check_eq("thread name", h.join().unwrap().as_deref(), Some("worker"));
    check("main thread name", thread::current().name() == Some("main"));
    check(
        "parallelism unknown",
        thread::available_parallelism().is_err(),
    );
}

fn time_checks() {
    let t0 = Instant::now();
    thread::sleep(Duration::from_millis(20));
    let dt = t0.elapsed();
    check("sleep >= 20ms", dt >= Duration::from_millis(20));
    check("sleep < 2s", dt < Duration::from_secs(2));
    let t1 = Instant::now();
    check("instant monotonic", t1 >= t0);
    let now = SystemTime::now();
    check(
        "system time after epoch",
        now.duration_since(UNIX_EPOCH).is_ok(),
    );
    check("system time ordered", SystemTime::now() >= now);
}

fn fs_checks() {
    let banner = fs::read_to_string("banner.txt").unwrap();
    check_eq(
        "ramdisk read",
        banner.as_str(),
        "galexy ramdisk plumbing works",
    );
    check(
        "ramdisk read-only",
        OpenOptions::new().write(true).open("banner.txt").is_err(),
    );

    let _ = fs::remove_dir_all("stdbox");
    let _ = fs::remove_file("stdtest.txt");

    fs::write("stdtest.txt", "hello\n").unwrap();
    {
        let mut f = OpenOptions::new().append(true).open("stdtest.txt").unwrap();
        f.write_all(b"world\n").unwrap();
    }
    check_eq(
        "write+append",
        fs::read_to_string("stdtest.txt").unwrap().as_str(),
        "hello\nworld\n",
    );
    check_eq(
        "metadata len",
        fs::metadata("stdtest.txt").unwrap().len(),
        12,
    );
    check(
        "metadata is_file",
        fs::metadata("stdtest.txt").unwrap().is_file(),
    );
    {
        // Positional write (Milestone 70): seek, overwrite in place.
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .open("stdtest.txt")
            .unwrap();
        f.seek(SeekFrom::Start(3)).unwrap();
        f.write_all(b"LO").unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        check_eq("seek+write", s.as_str(), "helLO\nworld\n");
        check_eq("stream_position", f.stream_position().unwrap(), 12);
        f.set_len(5).unwrap();
        check_eq("set_len", f.metadata().unwrap().len(), 5);
    }
    {
        let mut f = fs::File::create("stdtest.txt").unwrap();
        f.write_all(b"fresh").unwrap();
        f.sync_all().unwrap();
    }
    check_eq(
        "create truncates",
        fs::read_to_string("stdtest.txt").unwrap().as_str(),
        "fresh",
    );
    check(
        "create_new exists",
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open("stdtest.txt")
            .map_err(|e| e.kind())
            .err()
            == Some(ErrorKind::AlreadyExists),
    );

    let names: Vec<String> = fs::read_dir("/")
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    check(
        "read_dir root has file",
        names.iter().any(|n| n == "stdtest.txt"),
    );
    check(
        "read_dir root has ramdisk",
        names.iter().any(|n| n == "banner.txt"),
    );

    fs::create_dir("stdbox").unwrap();
    check(
        "create_dir is_dir",
        fs::metadata("stdbox").unwrap().is_dir(),
    );
    fs::write("stdbox/inner.toml", DOC).unwrap();
    fs::copy("stdbox/inner.toml", "stdbox/copy.toml").unwrap();
    check_eq(
        "copy",
        fs::read_to_string("stdbox/copy.toml").unwrap().as_str(),
        DOC,
    );
    fs::rename("stdbox/copy.toml", "stdbox/moved.toml").unwrap();
    check("rename old gone", !fs::exists("stdbox/copy.toml").unwrap());
    let mut inner: Vec<String> = fs::read_dir("stdbox")
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    inner.sort();
    check_eq(
        "read_dir subdir",
        inner,
        vec!["inner.toml".to_string(), "moved.toml".to_string()],
    );
    check(
        "remove_dir non-empty fails",
        fs::remove_dir("stdbox").is_err(),
    );

    check_eq(
        "missing file kind",
        fs::read("nope.txt").map_err(|e| e.kind()).err(),
        Some(ErrorKind::NotFound),
    );
    check_eq(
        "missing dir kind",
        fs::read_dir("nodir").map_err(|e| e.kind()).err(),
        Some(ErrorKind::NotFound),
    );
    check("symlink unsupported", fs::read_link("stdtest.txt").is_err());

    fs::remove_dir_all("stdbox").unwrap();
    fs::remove_file("stdtest.txt").unwrap();
    check("remove_dir_all", !fs::exists("stdbox").unwrap());
    check("remove_file", !fs::exists("stdtest.txt").unwrap());
}

fn process_checks() {
    // `echo` mode 3 writes its text plus a newline to the pipe on the
    // first file slot; `output()` is what installs that pipe.
    let out = Command::new("echo")
        .arg("\x03\x00hi there")
        .output()
        .unwrap();
    check("echo status", out.status.success());
    check_eq("echo code", out.status.code(), Some(0));
    check_eq(
        "echo stdout",
        String::from_utf8(out.stdout).unwrap().as_str(),
        "hi there\n",
    );
    check("echo stderr empty", out.stderr.is_empty());

    // Inherited stdout goes to the console; wait returns the code.
    let status = Command::new("hello").status().unwrap();
    check("hello status", status.success());

    // Explicit pipe through spawn + read.
    let mut child = Command::new("echo")
        .arg("\x03\x00piped")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    check("child id", child.id() > 0);
    let mut text = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    let status = child.wait().unwrap();
    check_eq("spawn piped", text.as_str(), "piped\n");
    check("spawn status", status.success());

    // A program that exits non-zero: `echo` with a bad mode.
    let status = Command::new("echo").arg("\x09\x00x").status().unwrap();
    check_eq("nonzero code", status.code(), Some(1));

    // Missing program.
    check_eq(
        "missing program",
        Command::new("no-such-program")
            .spawn()
            .map_err(|e| e.kind())
            .err(),
        Some(ErrorKind::NotFound),
    );

    // Kill a lingering child.
    let mut child = Command::new("linger")
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    check("try_wait running", child.try_wait().unwrap().is_none());
    child.kill().unwrap();
    let status = child.wait().unwrap();
    check_eq("killed code", status.code(), Some(137));
}

fn collections_checks() {
    let mut map: HashMap<String, u64> = HashMap::new();
    for i in 0..300u64 {
        map.insert(format!("key{i}"), i * i);
    }
    check_eq("hashmap len", map.len(), 300);
    check_eq("hashmap get", map.get("key17").copied(), Some(289));
    check("hashmap miss", !map.contains_key("key300"));
    let mut words = vec!["pear", "apple", "fig"];
    words.sort();
    check_eq("sort", words, vec!["apple", "fig", "pear"]);
    let s = format!("{:>8.3}|{:#x}|{:?}", 1.23456_f64, 255u32, ("a", 1));
    check_eq("format", s.as_str(), "   1.235|0xff|(\"a\", 1)");
    let parsed: Result<u32, _> = "12x".parse();
    check("parse error", parsed.is_err());
    let boxed: Box<dyn std::error::Error> = Box::new(io::Error::other("boom"));
    check_eq("dyn error", boxed.to_string().as_str(), "boom");
}

fn toml_checks() {
    let cfg: Config = toml::from_str(DOC).unwrap();
    check_eq("toml name", cfg.name.as_str(), "galexy");
    check_eq("toml port", cfg.port, 8080);
    check_eq("toml tags", cfg.tags.len(), 3);
    check_eq("toml nested", cfg.limits.threads, 16);
    check("toml float", (cfg.limits.ratio - 0.75).abs() < 1e-9);

    let text = toml::to_string(&cfg).unwrap();
    check("toml serialize", text.contains("port = 8080"));
    let back: Config = toml::from_str(&text).unwrap();
    check("toml roundtrip", back == cfg);

    fs::write("stdtest.toml", DOC).unwrap();
    let from_file: Config = toml::from_str(&fs::read_to_string("stdtest.toml").unwrap()).unwrap();
    check("toml from file", from_file == cfg);
    fs::remove_file("stdtest.toml").unwrap();

    let bad: Result<Config, _> = toml::from_str("name = \"x\"\nport = \"not a number\"");
    check("toml error", bad.is_err());

    let table: toml::Table = DOC.parse().unwrap();
    check_eq(
        "toml table",
        table["limits"]["threads"].as_integer(),
        Some(16),
    );
}

fn io_checks() {
    print!("stdtest: partial ");
    io::stdout().flush().unwrap();
    println!("line");
    eprintln!("stdtest: stderr ok");
    let mut sink = io::sink();
    check_eq("sink", sink.write(b"abc").unwrap(), 3);
    let mut cursor = io::Cursor::new(b"abc\ndef\n".to_vec());
    let mut first = String::new();
    io::BufRead::read_line(&mut cursor, &mut first).unwrap();
    check_eq("cursor read_line", first.as_str(), "abc\n");
}

fn main() {
    println!("stdtest: start");
    env_checks();
    thread_checks();
    time_checks();
    fs_checks();
    process_checks();
    collections_checks();
    toml_checks();
    io_checks();
    println!(
        "stdtest: passed ({} checks)",
        CHECKS.load(Ordering::Relaxed)
    );
}
