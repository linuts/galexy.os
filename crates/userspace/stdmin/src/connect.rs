//! `connect` — `std::net::TcpStream::connect` on the login image.
//!
//! A granted name asks the seat, then dials through `net`. The value
//! is a lane: this program reads it to EOF. `--bind <address>` binds
//! the stored local address. UDP and a timeout stay `unsupported`.

use std::io::{ErrorKind, Read};
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
