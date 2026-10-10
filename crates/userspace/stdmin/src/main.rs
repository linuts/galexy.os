//! stdmin — `std` on the galexy target, without a Rust fork.
//!
//! Builds a `HashMap<String, Vec<u32>>`, formats it, and writes the line
//! through the console capability. `std::fs`, threads, and `println!`
//! are the Milestone 70 PAL; this program does not call them.

#![feature(restricted_std)]
#![no_main]

use std::collections::HashMap;

use galexy_rt::{entry, write_console};

entry!(main);

fn main() -> i32 {
    let mut map: HashMap<String, Vec<u32>> = HashMap::new();
    map.insert("a".to_string(), vec![1, 2]);
    let line = format!("stdmin: a={:?}\n", map.get("a").unwrap());
    write_console(line.as_bytes());
    0
}
