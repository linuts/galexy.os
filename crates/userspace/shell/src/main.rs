//! The interactive shell: a ring-3 program.
//!
//! Keys arrive through the keyboard capability. Programs start through the
//! loader capability, and this task stays parked until they exit. The
//! kernel keeps the status bar and the screen.

#![no_std]
#![no_main]

use galexy_abi::SysError;
use galexy_rt::{entry, keyboard_cap, read, spawn, write_console, yield_now};

entry!(main);

const LINE_MAX: usize = 80;

fn main() -> i32 {
    let kbd = keyboard_cap();
    let mut line = [0u8; LINE_MAX];
    let mut len = 0usize;
    prompt();
    loop {
        let mut buf = [0u8; 8];
        let got = read(kbd, &mut buf);
        if !got.ok {
            write_console(b"\nread: keyboard denied\n");
            prompt();
            continue;
        }
        if got.value == 0 {
            yield_now();
            continue;
        }
        let n = got.value as usize;
        for &byte in &buf[..n.min(buf.len())] {
            match byte {
                b'\n' | b'\r' => {
                    write_console(b"\n");
                    dispatch(trim(&line[..len]));
                    len = 0;
                }
                0x08 => {
                    if len > 0 {
                        len -= 1;
                        write_console(&[0x08]);
                    }
                }
                b if (b.is_ascii_graphic() || b == b' ') && len < LINE_MAX => {
                    line[len] = b;
                    len += 1;
                    write_console(&[b]);
                }
                _ => {}
            }
        }
    }
}

fn dispatch(line: &[u8]) {
    if line.is_empty() {
        prompt();
        return;
    }
    if line == b"help" {
        write_console(b"commands: help, about, clear, run <program>\n");
        prompt();
        return;
    }
    if line == b"about" {
        write_console(b"galexy.os - a small Rust OS\n");
        write_console(b"this shell is a ring-3 program\n");
        prompt();
        return;
    }
    if line == b"clear" {
        write_console(&[0x0c]);
        prompt();
        return;
    }
    if let Some(name) = run_arg(line) {
        run(name);
        return;
    }
    write_console(line);
    write_console(b": command not found\n");
    prompt();
}

fn run(name: &[u8]) {
    if name.is_empty() {
        write_console(b"run: no program named (usage: run <program>)\n");
        prompt();
        return;
    }
    let result = spawn(name);
    if !result.ok {
        if result.value == SysError::NotFound as u64 {
            write_console(b"run: no such program '");
            write_console(name);
            write_console(b"'\n");
        } else if result.value == SysError::NoResource as u64 {
            write_console(b"run: a program is already starting\n");
        } else {
            write_console(b"run: failed\n");
        }
    }
    prompt();
}

/// `run` with nothing after it, or the trimmed argument after `run `.
fn run_arg(line: &[u8]) -> Option<&[u8]> {
    if line == b"run" {
        return Some(b"");
    }
    let prefix = b"run ";
    if line.starts_with(prefix) {
        return Some(trim(&line[prefix.len()..]));
    }
    None
}

fn trim(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && bytes[start] == b' ' {
        start += 1;
    }
    while end > start && bytes[end - 1] == b' ' {
        end -= 1;
    }
    &bytes[start..end]
}

fn prompt() {
    write_console(b"galexy> ");
}
