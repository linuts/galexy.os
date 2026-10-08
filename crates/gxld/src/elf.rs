//! ELF64 constants the linker reads and writes, and the output check that
//! mirrors what `sched/loader.rs` enforces.

use crate::error::{Error, Result};
use alloc::format;
use alloc::string::ToString;

pub(crate) const PAGE: u64 = 0x1000;
pub(crate) const EHDR_SIZE: usize = 64;
pub(crate) const PHDR_SIZE: usize = 56;

pub(crate) const ET_REL: u16 = 1;
pub(crate) const ET_EXEC: u16 = 2;
pub(crate) const EM_X86_64: u16 = 0x3E;

pub(crate) const PT_LOAD: u32 = 1;
pub(crate) const PF_X: u32 = 1;
pub(crate) const PF_W: u32 = 2;
pub(crate) const PF_R: u32 = 4;

pub(crate) const SHT_PROGBITS: u32 = 1;
pub(crate) const SHT_NOBITS: u32 = 8;
pub(crate) const SHT_REL: u32 = 9;
pub(crate) const SHT_INIT_ARRAY: u32 = 14;
pub(crate) const SHT_FINI_ARRAY: u32 = 15;
pub(crate) const SHT_PREINIT_ARRAY: u32 = 16;
pub(crate) const SHT_GROUP: u32 = 17;

pub(crate) const SHF_WRITE: u64 = 1 << 0;
pub(crate) const SHF_ALLOC: u64 = 1 << 1;
pub(crate) const SHF_EXECINSTR: u64 = 1 << 2;
pub(crate) const SHF_TLS: u64 = 1 << 10;
pub(crate) const SHF_GNU_RETAIN: u64 = 1 << 21;

pub(crate) const GRP_COMDAT: u32 = 1;

pub(crate) const SHN_UNDEF: u16 = 0;
pub(crate) const SHN_ABS: u16 = 0xfff1;
pub(crate) const SHN_COMMON: u16 = 0xfff2;
pub(crate) const SHN_XINDEX: u16 = 0xffff;

pub(crate) const STB_LOCAL: u8 = 0;
#[cfg(test)]
pub(crate) const STB_GLOBAL: u8 = 1;
pub(crate) const STB_WEAK: u8 = 2;

pub(crate) const STT_SECTION: u8 = 3;
pub(crate) const STT_FILE: u8 = 4;
pub(crate) const STT_TLS: u8 = 6;

pub(crate) const R_X86_64_NONE: u32 = 0;
pub(crate) const R_X86_64_64: u32 = 1;
pub(crate) const R_X86_64_PC32: u32 = 2;
pub(crate) const R_X86_64_PLT32: u32 = 4;
pub(crate) const R_X86_64_GOTPCREL: u32 = 9;
pub(crate) const R_X86_64_32: u32 = 10;
pub(crate) const R_X86_64_32S: u32 = 11;
pub(crate) const R_X86_64_PC64: u32 = 24;
pub(crate) const R_X86_64_GOTPC32: u32 = 26;
pub(crate) const R_X86_64_SIZE32: u32 = 32;
pub(crate) const R_X86_64_SIZE64: u32 = 33;
pub(crate) const R_X86_64_GOTPCRELX: u32 = 41;
pub(crate) const R_X86_64_REX_GOTPCRELX: u32 = 42;
pub(crate) const R_X86_64_CODE_4_GOTPCRELX: u32 = 43;

/// Host-side checks mirroring the loader's hard requirements: ELF64 LE,
/// `ET_EXEC`, entry inside the image window, every `PT_LOAD` W^X and at or
/// above `image_base`.
pub fn validate(bytes: &[u8], image_base: u64) -> Result<()> {
    let bad = |detail: &str| Error::Input {
        input: "output".to_string(),
        detail: detail.to_string(),
    };
    if bytes.len() < EHDR_SIZE + PHDR_SIZE {
        return Err(bad("ELF too small"));
    }
    if &bytes[0..4] != b"\x7fELF" {
        return Err(bad("bad ELF magic"));
    }
    if bytes[4] != 2 || bytes[5] != 1 {
        return Err(bad("need ELF64 little-endian"));
    }
    let etype = read_u16(bytes, 16);
    if etype != ET_EXEC {
        return Err(bad(&format!("need ET_EXEC, got {etype}")));
    }
    let entry = read_u64(bytes, 24);
    if entry < image_base || entry >= image_base + 512 * 1024 * 1024 {
        return Err(bad(&format!("entry {entry:#x} outside the image window")));
    }
    let phoff = read_u64(bytes, 32) as usize;
    let phentsize = read_u16(bytes, 54) as usize;
    let phnum = read_u16(bytes, 56) as usize;
    if phentsize != PHDR_SIZE {
        return Err(bad("unexpected e_phentsize"));
    }
    let mut saw_load = false;
    let mut last_end = 0u64;
    for i in 0..phnum {
        let off = phoff + i * phentsize;
        if off + PHDR_SIZE > bytes.len() {
            return Err(bad("phdr truncated"));
        }
        if read_u32(bytes, off) != PT_LOAD {
            continue;
        }
        saw_load = true;
        let flags = read_u32(bytes, off + 4);
        if flags & PF_W != 0 && flags & PF_X != 0 {
            return Err(bad("W|X PT_LOAD forbidden (W^X)"));
        }
        let offset = read_u64(bytes, off + 8);
        let vaddr = read_u64(bytes, off + 16);
        let filesz = read_u64(bytes, off + 32);
        let memsz = read_u64(bytes, off + 40);
        if vaddr < image_base {
            return Err(bad(&format!("PT_LOAD vaddr {vaddr:#x} below image base")));
        }
        if !vaddr.is_multiple_of(PAGE) || vaddr < last_end {
            return Err(bad("PT_LOAD segments must be page-aligned and disjoint"));
        }
        if filesz > memsz || offset.saturating_add(filesz) > bytes.len() as u64 {
            return Err(bad("PT_LOAD sizes exceed the file"));
        }
        last_end = align_up(vaddr + memsz, PAGE);
    }
    if !saw_load {
        return Err(bad("no PT_LOAD segments"));
    }
    Ok(())
}

pub(crate) fn align_up(v: u64, a: u64) -> u64 {
    if a <= 1 {
        v
    } else {
        v.div_ceil(a) * a
    }
}

pub(crate) fn write_u16(out: &mut [u8], at: usize, v: u16) {
    out[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
pub(crate) fn write_u32(out: &mut [u8], at: usize, v: u32) {
    out[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
pub(crate) fn write_u64(out: &mut [u8], at: usize, v: u64) {
    out[at..at + 8].copy_from_slice(&v.to_le_bytes());
}
pub(crate) fn read_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
pub(crate) fn read_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
pub(crate) fn read_u64(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}
