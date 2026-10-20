//! One cooked line from `std::io::stdin`, then that line on stdout.
//!
//! The shell runs this as `testing/stdline` with an argument so the
//! prompt waits and the program holds the seat keyboard. A pipe would
//! be raw bytes; this path is the console.

fn main() {
    use std::io::{self, Write};
    let mut line = String::new();
    print!("line? ");
    let _ = io::stdout().flush();
    match io::stdin().read_line(&mut line) {
        Ok(0) => println!("eof"),
        Ok(_) => print!("got {line}"),
        Err(e) => println!("err {e}"),
    }
}
