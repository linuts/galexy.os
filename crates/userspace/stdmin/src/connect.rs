//! `connect` — `std::net::TcpStream::connect` on the login image.
//!
//! A granted name asks the seat, then dials through `net`. The value
//! is a lane: this program reads it to EOF. `--bind <address>` binds
//! the stored local address. `--listen <port>` accepts on the stored
//! port. `--accept <address:port>` binds, then accepts. `--send <name>`
//! sends one datagram to the stored peer. `--recv <name>` receives one
//! datagram from that peer. `--http <name>` writes one request and
//! reads the response on that lane. A timeout and a socket option stay
//! `unsupported`.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::process::ExitCode;
use std::time::Duration;

/// Public pin of the in-tree listener certificate. The program writes
/// it to a file and does not print it.
const PIN_HEX: &str = "a758b0f4e66caafe2efc3136f19126281107cd81384526e92e9cf153fda1e38f";

fn main() -> ExitCode {
    let arg = std::env::args().nth(1).unwrap_or_default();
    if arg.is_empty() {
        println!("connect: usage: connect <name>");
        return ExitCode::from(1);
    }
    if arg == "--putkey" {
        if std::fs::write("/k", PIN_HEX).is_err()
            || std::fs::write("/bad", "11".repeat(32)).is_err()
        {
            println!("connect: write failed");
            return ExitCode::from(1);
        }
        println!("wrote");
        return ExitCode::SUCCESS;
    }
    if arg == "--resolve" {
        let name = std::env::args().nth(2).unwrap_or_default();
        match TcpStream::lookup(&name) {
            Ok(text) => {
                print!("{text}");
                if !text.ends_with('\n') {
                    println!();
                }
                return ExitCode::SUCCESS;
            }
            Err(err) => return report(Some(err)),
        }
    }
    if arg == "--bind" {
        let spec = std::env::args()
            .nth(2)
            .unwrap_or_else(|| "127.0.0.1:1".into());
        let spec = if spec.contains(':') {
            spec
        } else {
            format!("{spec}:0")
        };
        return report(TcpListener::bind(spec).err());
    }
    if arg == "--listen" {
        let port = std::env::args().nth(2).unwrap_or_default();
        return match TcpListener::listen_port(&port) {
            Ok(None) => {
                println!("attached");
                ExitCode::SUCCESS
            }
            Ok(Some(mut stream)) => {
                let mut body = Vec::new();
                if stream.read_to_end(&mut body).is_err() {
                    println!("connect: read failed");
                    return ExitCode::from(1);
                }
                let text = String::from_utf8_lossy(&body);
                if text.ends_with('\n') {
                    print!("{text}");
                } else {
                    println!("{text}");
                }
                ExitCode::SUCCESS
            }
            Err(err) if err.kind() == ErrorKind::AddrInUse => {
                println!("held");
                ExitCode::SUCCESS
            }
            Err(err) => report(Some(err)),
        };
    }
    if arg == "--send" {
        let name = std::env::args().nth(2).unwrap_or_default();
        return match UdpSocket::send_card(&name, b"card-datagram") {
            Ok(()) => {
                println!("sent");
                ExitCode::SUCCESS
            }
            Err(err) => report(Some(err)),
        };
    }
    if arg == "--recv" {
        let name = std::env::args().nth(2).unwrap_or_default();
        return match UdpSocket::recv_card(&name) {
            Ok(body) => {
                let text = String::from_utf8_lossy(&body);
                if text.ends_with('\n') {
                    print!("{text}");
                } else {
                    println!("{text}");
                }
                ExitCode::SUCCESS
            }
            Err(err) => report(Some(err)),
        };
    }
    if arg == "--accept" {
        let spec = std::env::args().nth(2).unwrap_or_default();
        let listener = match TcpListener::bind(&spec) {
            Ok(listener) => listener,
            Err(err) => return report(Some(err)),
        };
        let (mut stream, _) = match listener.accept() {
            Ok(pair) => pair,
            Err(err) => return report(Some(err)),
        };
        let mut body = Vec::new();
        if stream.read_to_end(&mut body).is_err() {
            println!("connect: read failed");
            return ExitCode::from(1);
        }
        let text = String::from_utf8_lossy(&body);
        if text.ends_with('\n') {
            print!("{text}");
        } else {
            println!("{text}");
        }
        return ExitCode::SUCCESS;
    }
    if arg == "--udp" {
        return report(UdpSocket::bind("127.0.0.1:1").err());
    }
    if arg == "--timeout" {
        let Ok(addr) = "127.0.0.1:9".parse() else {
            println!("unsupported");
            return ExitCode::SUCCESS;
        };
        return report(TcpStream::connect_timeout(&addr, Duration::from_secs(1)).err());
    }
    if arg == "--http" {
        let name = std::env::args().nth(2).unwrap_or_default();
        let mut stream = match TcpStream::connect(&name) {
            Ok(stream) => stream,
            Err(err) => return report(Some(err)),
        };
        let req = b"GET / HTTP/1.0\r\n\r\n";
        let mut off = 0usize;
        while off < req.len() {
            match stream.write(&req[off..]) {
                Ok(0) | Err(_) => {
                    println!("connect: write failed");
                    return ExitCode::from(1);
                }
                Ok(n) => off += n,
            }
        }
        let mut body = Vec::new();
        loop {
            let mut buf = [0u8; 256];
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    body.extend_from_slice(&buf[..n]);
                    if http_complete(&body) {
                        break;
                    }
                }
                Err(_) => {
                    println!("connect: read failed");
                    return ExitCode::from(1);
                }
            }
        }
        let text = String::from_utf8_lossy(&body);
        if text.ends_with('\n') {
            print!("{text}");
        } else {
            println!("{text}");
        }
        return ExitCode::SUCCESS;
    }
    let mut stream = match TcpStream::connect(&arg) {
        Ok(stream) => stream,
        Err(err) => return report(Some(err)),
    };
    // A socket option is not a dial. Success here would be a second stack.
    if stream.set_nodelay(true).is_ok() {
        println!("nodelay");
    }
    let mut body = Vec::new();
    if stream.read_to_end(&mut body).is_err() {
        println!("connect: read failed");
        return ExitCode::from(1);
    }
    let text = String::from_utf8_lossy(&body);
    if text.ends_with('\n') {
        print!("{text}");
    } else {
        println!("{text}");
    }
    ExitCode::SUCCESS
}

/// Headers plus a `Content-Length` body. The lane stays open, so the
/// read stops when that body is here.
fn http_complete(buf: &[u8]) -> bool {
    let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let header = &buf[..at];
    let body = &buf[at + 4..];
    let key = b"Content-Length:";
    let Some(pos) = header
        .windows(key.len())
        .position(|w| w.eq_ignore_ascii_case(key))
    else {
        return false;
    };
    let rest = header[pos + key.len()..].trim_ascii_start();
    let digits = rest
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .copied()
        .collect::<Vec<_>>();
    let Ok(text) = std::str::from_utf8(&digits) else {
        return false;
    };
    let Ok(len) = text.parse::<usize>() else {
        return false;
    };
    body.len() >= len
}

fn report(err: Option<std::io::Error>) -> ExitCode {
    match err {
        None => println!("connected"),
        Some(err) if err.kind() == ErrorKind::Unsupported => println!("unsupported"),
        Some(err) if err.kind() == ErrorKind::PermissionDenied => {
            println!("access denied");
            println!("still up");
        }
        Some(err) => println!("connect: {err}"),
    }
    ExitCode::from(1)
}
