//! Minimal static ELF64 emitter for Galexy user programs.
//!
//! Layout (page-aligned segments, W^X):
//! ```text
//! file 0x0000  ELF header + 2 program headers
//! file 0x1000  rodata  → VA IMAGE_BASE+0x1000  (PF_R)
//! file 0x2000  text    → VA IMAGE_BASE+0x2000  (PF_R|PF_X), e_entry
//! ```

use crate::codegen::{self, ObjectCode};
use crate::error::{Error, Result};
use galexy_abi::USER_IMAGE_BASE;

const PAGE: usize = 0x1000;
const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;
const PHDR_COUNT: usize = 2;

const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

/// Build a static ET_EXEC ELF image for `code`.
pub fn emit_elf(code: &ObjectCode) -> Result<Vec<u8>> {
    let rodata_file_off = PAGE;
    let text_file_off = PAGE * 2;
    let rodata_va = USER_IMAGE_BASE + rodata_file_off as u64;
    let text_va = USER_IMAGE_BASE + text_file_off as u64;

    let text = codegen::finish_text(code, rodata_va);
    if text.is_empty() {
        return Err(Error::msg("internal: empty text"));
    }

    let ro_filesz = code.rodata.len().max(1); // keep a non-empty R segment
    let mut ro_payload = code.rodata.clone();
    if ro_payload.is_empty() {
        ro_payload.push(0);
    }

    let text_filesz = text.len();
    let file_len = text_file_off + text_filesz;
    let mut out = vec![0u8; file_len];

    write_ehdr(&mut out, text_va, PHDR_COUNT as u16);
    // Phdr 0: rodata
    write_phdr(
        &mut out,
        0,
        PT_LOAD,
        PF_R,
        rodata_file_off as u64,
        rodata_va,
        ro_filesz as u64,
        ro_filesz as u64,
        PAGE as u64,
    );
    // Phdr 1: text
    write_phdr(
        &mut out,
        1,
        PT_LOAD,
        PF_R | PF_X,
        text_file_off as u64,
        text_va,
        text_filesz as u64,
        text_filesz as u64,
        PAGE as u64,
    );

    out[rodata_file_off..rodata_file_off + ro_payload.len()].copy_from_slice(&ro_payload);
    out[text_file_off..text_file_off + text.len()].copy_from_slice(&text);

    validate_elf(&out)?;
    Ok(out)
}

fn write_ehdr(out: &mut [u8], entry: u64, phnum: u16) {
    out[0..4].copy_from_slice(b"\x7fELF");
    out[4] = 2; // ELFCLASS64
    out[5] = 1; // ELFDATA2LSB
    out[6] = 1; // EV_CURRENT
    out[7] = 0; // ELFOSABI_NONE
                // pad 8..16 zero
    write_u16(out, 16, 2); // ET_EXEC
    write_u16(out, 18, 0x3E); // EM_X86_64
    write_u32(out, 20, 1); // EV_CURRENT
    write_u64(out, 24, entry);
    write_u64(out, 32, EHDR_SIZE as u64); // e_phoff
    write_u64(out, 40, 0); // e_shoff
    write_u32(out, 48, 0); // e_flags
    write_u16(out, 52, EHDR_SIZE as u16);
    write_u16(out, 54, PHDR_SIZE as u16);
    write_u16(out, 56, phnum);
    write_u16(out, 58, 0); // e_shentsize
    write_u16(out, 60, 0); // e_shnum
    write_u16(out, 62, 0); // e_shstrndx
}

fn write_phdr(
    out: &mut [u8],
    index: usize,
    p_type: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
    align: u64,
) {
    let base = EHDR_SIZE + index * PHDR_SIZE;
    write_u32(out, base, p_type);
    write_u32(out, base + 4, flags);
    write_u64(out, base + 8, offset);
    write_u64(out, base + 16, vaddr);
    write_u64(out, base + 24, vaddr); // paddr
    write_u64(out, base + 32, filesz);
    write_u64(out, base + 40, memsz);
    write_u64(out, base + 48, align);
}

/// Host-side checks mirroring the loader's hard requirements.
pub fn validate_elf(bytes: &[u8]) -> Result<()> {
    if bytes.len() < EHDR_SIZE + PHDR_SIZE {
        return Err(Error::msg("ELF too small"));
    }
    if &bytes[0..4] != b"\x7fELF" {
        return Err(Error::msg("bad ELF magic"));
    }
    if bytes[4] != 2 || bytes[5] != 1 {
        return Err(Error::msg("need ELF64 little-endian"));
    }
    let etype = read_u16(bytes, 16);
    if etype != 2 {
        return Err(Error::msg(format!("need ET_EXEC, got {etype}")));
    }
    let entry = read_u64(bytes, 24);
    if entry < USER_IMAGE_BASE || entry >= USER_IMAGE_BASE + 512 * 1024 * 1024 {
        return Err(Error::msg(format!(
            "entry {entry:#x} outside USER_IMAGE_BASE region"
        )));
    }
    let phoff = read_u64(bytes, 32) as usize;
    let phentsize = read_u16(bytes, 54) as usize;
    let phnum = read_u16(bytes, 56) as usize;
    if phentsize != PHDR_SIZE {
        return Err(Error::msg("unexpected e_phentsize"));
    }
    let mut saw_load = false;
    for i in 0..phnum {
        let off = phoff + i * phentsize;
        if off + PHDR_SIZE > bytes.len() {
            return Err(Error::msg("phdr truncated"));
        }
        let p_type = read_u32(bytes, off);
        if p_type != PT_LOAD {
            continue;
        }
        saw_load = true;
        let flags = read_u32(bytes, off + 4);
        let writable = flags & PF_W != 0;
        let exec = flags & PF_X != 0;
        if writable && exec {
            return Err(Error::msg("W|X PT_LOAD forbidden (W^X)"));
        }
        let vaddr = read_u64(bytes, off + 16);
        if vaddr < USER_IMAGE_BASE {
            return Err(Error::msg(format!("PT_LOAD vaddr {vaddr:#x} below image base")));
        }
    }
    if !saw_load {
        return Err(Error::msg("no PT_LOAD segments"));
    }
    Ok(())
}

fn write_u16(out: &mut [u8], at: usize, v: u16) {
    out[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
fn write_u32(out: &mut [u8], at: usize, v: u32) {
    out[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn write_u64(out: &mut [u8], at: usize, v: u64) {
    out[at..at + 8].copy_from_slice(&v.to_le_bytes());
}
fn read_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn read_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn read_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::codegen;
    use crate::compile_check;

    #[test]
    fn emits_valid_hello_elf() {
        let prog = compile_check(include_str!("../examples/hello.gxr")).unwrap();
        let obj = codegen(&prog);
        let elf = emit_elf(&obj).unwrap();
        validate_elf(&elf).unwrap();
        assert!(elf.starts_with(b"\x7fELF"));
        let entry = read_u64(&elf, 24);
        assert_eq!(entry, USER_IMAGE_BASE + 0x2000);
        // Two LOADs: R and RX
        let flags0 = read_u32(&elf, EHDR_SIZE + 4);
        let flags1 = read_u32(&elf, EHDR_SIZE + PHDR_SIZE + 4);
        assert_eq!(flags0, PF_R);
        assert_eq!(flags1, PF_R | PF_X);
    }
}
