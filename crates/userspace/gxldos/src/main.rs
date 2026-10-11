//! Ring-3 `gxld`. Inputs and the output arrive as Caps at spawn.
//! The program does not open a path.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use galexy_abi::{Cap, CapRights, FILE_CAP_BASE};
use galexy_rt::{entry, read, write, write_console};
use gxld::{link, Input, Options};

entry!(main);

fn main() -> i32 {
    let mut blobs: Vec<Vec<u8>> = Vec::new();
    let mut names: Vec<alloc::string::String> = Vec::new();
    let mut output = None;
    for slot in 0..64u64 {
        let index = FILE_CAP_BASE + slot;
        let read_cap = Cap::new(index, CapRights::READ);
        let write_cap = Cap::new(index, CapRights::WRITE);
        let can_read = read(read_cap, &mut []).ok;
        let can_write = write(write_cap, &[]).ok;
        if can_read && !can_write {
            blobs.push(read_all(read_cap));
            names.push(alloc::format!("in{slot}.o"));
        } else if can_write && !can_read {
            output = Some(write_cap);
        }
    }
    let Some(out) = output else {
        write_console(b"gxldos: no output cap\n");
        return 2;
    };
    if blobs.len() < 2 {
        write_console(b"gxldos: need two inputs\n");
        return 2;
    }
    let inputs: Vec<Input<'_>> = blobs
        .iter()
        .zip(names.iter())
        .map(|(bytes, name)| Input {
            name: name.clone(),
            bytes,
            whole_archive: false,
        })
        .collect();
    let img = match link(&inputs, &Options::default()) {
        Ok(img) => img,
        Err(_) => {
            write_console(b"gxldos: link failed\n");
            return 3;
        }
    };
    let mut off = 0usize;
    while off < img.len() {
        let n = (img.len() - off).min(1024);
        let wrote = write(out, &img[off..off + n]);
        if !wrote.ok || wrote.value == 0 {
            write_console(b"gxldos: short write\n");
            return 4;
        }
        off += wrote.value as usize;
    }
    write_console(b"gxldos: linked\n");
    0
}

fn read_all(cap: Cap) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let got = read(cap, &mut buf);
        if !got.ok || got.value == 0 {
            break;
        }
        out.extend_from_slice(&buf[..got.value as usize]);
    }
    out
}
