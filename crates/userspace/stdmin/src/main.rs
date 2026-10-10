//! stdmin — the smallest `std` program on the galexy target.
//!
//! Plain `fn main()`, no `#![no_main]`, no `entry!`: the Galexy PAL
//! (`std-pal/sys/pal/galexy/startup.rs`) owns `_start`, runs the std
//! runtime, and `println!` reaches the console (or the stdout pipe the
//! shell handed over) through `std::io::Stdout`. Builds a
//! `HashMap<String, Vec<u32>>`, formats it, and prints the line. No
//! `#![feature(restricted_std)]` either: `std/build.rs` lists galexy as a
//! supported OS, so stable crates compile unchanged.

use std::collections::HashMap;

fn main() {
    let mut map: HashMap<String, Vec<u32>> = HashMap::new();
    map.insert("a".to_string(), vec![1, 2]);
    println!("stdmin: a={:?}", map.get("a").unwrap());
}
