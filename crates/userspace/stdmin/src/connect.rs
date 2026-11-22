//! `connect` — `std::net::TcpStream::connect` on the login image.
//!
//! A granted name asks the seat, then dials through `net`. The value
//! is a lane: this program reads it to EOF. An address, a bind, or a
//! UDP socket is `unsupported` and does not open TCP.

use std::io::{ErrorKind, Read};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::process::ExitCode;
use std::time::Duration;

fn main() -> ExitCode {
    let arg = std::env::args().nth(1).unwrap_or_default();
    if arg.is_empty() {
        println!("connect: usage: connect <name>");
        return ExitCode::from(1);
    }
    if arg == "--bind" {
        return report(TcpListener::bind("127.0.0.1:1").err());
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
