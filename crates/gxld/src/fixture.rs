//! Two relocatable objects the ring-3 linker test hands over Caps.
//!
//! `a.o` is `_start`: it calls `answer`, then `exit`s with that value.
//! `b.o` defines `answer` as `mov eax, 0x71; ret`. Linked, the program
//! exits `0x71`.

use crate::elf::*;
use alloc::vec;
use alloc::vec::Vec;

struct Sym {
    name: &'static str,
    sec: &'static str,
    value: u64,
}

struct Obj {
    data: Vec<u8>,
    relocs: Vec<(u64, u32, &'static str, i64)>,
    syms: Vec<Sym>,
}

impl Obj {
    fn new(text: &[u8]) -> Self {
        Obj {
            data: text.to_vec(),
            relocs: Vec::new(),
            syms: Vec::new(),
        }
    }

    fn reloc(mut self, offset: u64, kind: u32, sym: &'static str, addend: i64) -> Self {
        self.relocs.push((offset, kind, sym, addend));
        self
    }

    fn sym(mut self, name: &'static str, sec: &'static str, value: u64) -> Self {
        self.syms.push(Sym { name, sec, value });
        self
    }

    fn build(&self) -> Vec<u8> {
        // 0 null, 1 .text, 2 .symtab, 3 .strtab, 4 .shstrtab, 5 .rela.text?
        let rela = !self.relocs.is_empty();
        let symtab_idx = 2u32;
        let strtab_idx = 3u32;
        let shstrtab_idx = 4u32;
        let mut shstr = vec![0u8];
        let mut strtab = vec![0u8];
        let add = |tab: &mut Vec<u8>, s: &str| -> u32 {
            let off = tab.len() as u32;
            tab.extend_from_slice(s.as_bytes());
            tab.push(0);
            off
        };
        let text_name = add(&mut shstr, ".text");
        let symtab_name = add(&mut shstr, ".symtab");
        let strtab_name = add(&mut shstr, ".strtab");
        let shstr_name = add(&mut shstr, ".shstrtab");
        let rela_name = if rela {
            add(&mut shstr, ".rela.text")
        } else {
            0
        };

        // null + STT_SECTION for .text + named symbols
        let mut symtab = vec![0u8; 24];
        let mut sec_sym = [0u8; 24];
        sec_sym[4] = (STB_LOCAL << 4) | STT_SECTION;
        sec_sym[6..8].copy_from_slice(&1u16.to_le_bytes());
        symtab.extend_from_slice(&sec_sym);
        let sym_index = |name: &str, syms: &[Sym]| -> u32 {
            if name == ".text" {
                return 1;
            }
            let i = syms.iter().position(|s| s.name == name).expect("symbol");
            (2 + i) as u32
        };
        for s in &self.syms {
            let mut e = [0u8; 24];
            let name_off = add(&mut strtab, s.name);
            e[0..4].copy_from_slice(&name_off.to_le_bytes());
            e[4] = STB_GLOBAL << 4;
            let shndx: u16 = if s.sec.is_empty() { SHN_UNDEF } else { 1 };
            e[6..8].copy_from_slice(&shndx.to_le_bytes());
            e[8..16].copy_from_slice(&s.value.to_le_bytes());
            symtab.extend_from_slice(&e);
        }
        let mut rela_blob = Vec::new();
        for (off, kind, sym, addend) in &self.relocs {
            rela_blob.extend_from_slice(&off.to_le_bytes());
            let info = ((sym_index(sym, &self.syms) as u64) << 32) | *kind as u64;
            rela_blob.extend_from_slice(&info.to_le_bytes());
            rela_blob.extend_from_slice(&addend.to_le_bytes());
        }

        let mut out = vec![0u8; 64];
        let place = |out: &mut Vec<u8>, data: &[u8], align: usize| -> u64 {
            while !out.len().is_multiple_of(align) {
                out.push(0);
            }
            let off = out.len() as u64;
            out.extend_from_slice(data);
            off
        };
        let text_off = place(&mut out, &self.data, 16);
        let sym_off = place(&mut out, &symtab, 8);
        let str_off = place(&mut out, &strtab, 1);
        let shs_off = place(&mut out, &shstr, 1);
        let rela_off = if rela {
            place(&mut out, &rela_blob, 8)
        } else {
            0
        };

        let mut headers: Vec<[u64; 10]> = vec![[0; 10]];
        headers.push([
            text_name as u64,
            SHT_PROGBITS as u64,
            SHF_ALLOC | SHF_EXECINSTR,
            0,
            text_off,
            self.data.len() as u64,
            0,
            0,
            16,
            0,
        ]);
        let first_global = 2;
        headers.push([
            symtab_name as u64,
            2,
            0,
            0,
            sym_off,
            symtab.len() as u64,
            strtab_idx as u64,
            first_global,
            8,
            24,
        ]);
        headers.push([
            strtab_name as u64,
            3,
            0,
            0,
            str_off,
            strtab.len() as u64,
            0,
            0,
            1,
            0,
        ]);
        headers.push([
            shstr_name as u64,
            3,
            0,
            0,
            shs_off,
            shstr.len() as u64,
            0,
            0,
            1,
            0,
        ]);
        if rela {
            headers.push([
                rela_name as u64,
                4,
                0,
                0,
                rela_off,
                rela_blob.len() as u64,
                symtab_idx as u64,
                1,
                8,
                24,
            ]);
        }
        while !out.len().is_multiple_of(8) {
            out.push(0);
        }
        let shoff = out.len() as u64;
        for h in &headers {
            let mut e = [0u8; 64];
            e[0..4].copy_from_slice(&(h[0] as u32).to_le_bytes());
            e[4..8].copy_from_slice(&(h[1] as u32).to_le_bytes());
            e[8..16].copy_from_slice(&h[2].to_le_bytes());
            e[16..24].copy_from_slice(&h[3].to_le_bytes());
            e[24..32].copy_from_slice(&h[4].to_le_bytes());
            e[32..40].copy_from_slice(&h[5].to_le_bytes());
            e[40..44].copy_from_slice(&(h[6] as u32).to_le_bytes());
            e[44..48].copy_from_slice(&(h[7] as u32).to_le_bytes());
            e[48..56].copy_from_slice(&h[8].to_le_bytes());
            e[56..64].copy_from_slice(&h[9].to_le_bytes());
            out.extend_from_slice(&e);
        }
        out[0..4].copy_from_slice(b"\x7fELF");
        out[4] = 2;
        out[5] = 1;
        out[6] = 1;
        out[16..18].copy_from_slice(&ET_REL.to_le_bytes());
        out[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
        out[20..24].copy_from_slice(&1u32.to_le_bytes());
        out[40..48].copy_from_slice(&shoff.to_le_bytes());
        out[52..54].copy_from_slice(&64u16.to_le_bytes());
        out[58..60].copy_from_slice(&64u16.to_le_bytes());
        out[60..62].copy_from_slice(&(headers.len() as u16).to_le_bytes());
        out[62..64].copy_from_slice(&(shstrtab_idx as u16).to_le_bytes());
        out
    }
}

/// `(a.o, b.o)` — `_start` calls `answer` and exits with its return value.
pub fn sample_objects() -> (Vec<u8>, Vec<u8>) {
    let start = Obj::new(&[
        0xe8, 0x00, 0x00, 0x00, 0x00, // call answer
        0x89, 0xc7, // mov edi, eax
        0x31, 0xc0, // xor eax, eax (Syscall::Exit)
        0x0f, 0x05, // syscall
    ])
    .reloc(1, R_X86_64_PLT32, "answer", -4)
    .sym("_start", ".text", 0)
    .sym("answer", "", 0)
    .build();
    let answer = Obj::new(&[
        0xb8, 0x71, 0x00, 0x00, 0x00, // mov eax, 0x71
        0xc3, // ret
    ])
    .sym("answer", ".text", 0)
    .build();
    (start, answer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{link, Input, Options};
    use alloc::string::ToString;

    #[test]
    fn sample_objects_link() {
        let (a, b) = sample_objects();
        let inputs = [
            Input {
                name: "a.o".to_string(),
                bytes: &a,
                whole_archive: false,
            },
            Input {
                name: "b.o".to_string(),
                bytes: &b,
                whole_archive: false,
            },
        ];
        let img = link(&inputs, &Options::default()).expect("link");
        assert!(img.starts_with(b"\x7fELF"));
        crate::validate(&img, galexy_abi::USER_IMAGE_BASE).expect("loader would accept it");
    }
}
