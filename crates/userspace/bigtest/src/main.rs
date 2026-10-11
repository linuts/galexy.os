//! Allocates 300 MiB, writes a 20 MiB galfs file, reads it back, checks a hash.
//!
//! `bigtest: passed` is the line `test-big` and the boot suite look for.

use std::fs::File;
use std::io::{Read, Write};

const HEAP: usize = 300 * 1024 * 1024;
const FILE_BYTES: usize = 20 * 1024 * 1024;
const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

fn fnv_byte(mut hash: u64, byte: u8) -> u64 {
    hash ^= u64::from(byte);
    hash.wrapping_mul(FNV_PRIME)
}

fn main() {
    println!("bigtest: start");
    let mut heap = vec![0u8; HEAP];
    let mut touched = 0usize;
    let mut page = 0usize;
    while page < heap.len() {
        heap[page] = 0x71;
        touched += 1;
        page += 4096;
    }
    if heap.len() != HEAP || touched != HEAP / 4096 || heap[HEAP - 4096] != 0x71 {
        println!("bigtest: FAIL heap");
        std::process::exit(1);
    }
    println!("bigtest: heap {HEAP} bytes");

    let mut expect = FNV_OFFSET;
    {
        let mut file = File::create("big.bin").unwrap_or_else(|e| {
            println!("bigtest: FAIL create {e}");
            std::process::exit(1);
        });
        let mut chunk = [0u8; 1024];
        let mut left = FILE_BYTES;
        let mut i = 0usize;
        while left > 0 {
            let n = left.min(chunk.len());
            for byte in &mut chunk[..n] {
                *byte = (i as u8).wrapping_mul(31).wrapping_add(7);
                expect = fnv_byte(expect, *byte);
                i += 1;
            }
            if file.write_all(&chunk[..n]).is_err() {
                println!("bigtest: FAIL write");
                std::process::exit(1);
            }
            left -= n;
        }
        if file.sync_all().is_err() {
            println!("bigtest: FAIL sync");
            std::process::exit(1);
        }
    }

    let mut got = FNV_OFFSET;
    let mut read_n = 0usize;
    {
        let mut file = File::open("big.bin").unwrap_or_else(|e| {
            println!("bigtest: FAIL open {e}");
            std::process::exit(1);
        });
        let mut buf = [0u8; 1024];
        loop {
            let n = match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    println!("bigtest: FAIL read {e}");
                    std::process::exit(1);
                }
            };
            for byte in &buf[..n] {
                got = fnv_byte(got, *byte);
            }
            read_n += n;
        }
    }
    if read_n != FILE_BYTES || got != expect {
        println!("bigtest: FAIL hash read={read_n} got={got:#x} expect={expect:#x}");
        std::process::exit(1);
    }
    println!("bigtest: passed");
}
