//! ELF64 relocatable (`ET_REL`) emitter: what `gxc` hands to `gxld`.
//!
//! Sections: `.text` (global `_start` at 0), `.rodata`, `.rela.text` with
//! one `R_X86_64_64` per string reference (symbol = `.rodata` section
//! symbol, addend = offset), `.symtab`, `.strtab`, `.shstrtab`.

use crate::codegen::ObjectCode;

const SHT_PROGBITS: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;
const SHT_RELA: u32 = 4;
const SHF_ALLOC: u64 = 1 << 1;
const SHF_EXECINSTR: u64 = 1 << 2;
const STB_LOCAL: u8 = 0;
const STB_GLOBAL: u8 = 1;
const STT_SECTION: u8 = 3;
const STT_FUNC: u8 = 2;
const R_X86_64_64: u32 = 1;

// Section indices: 0 null, 1 .text, 2 .rodata, 3 .rela.text, 4 .symtab,
// 5 .strtab, 6 .shstrtab.
const SEC_TEXT: u16 = 1;
const SEC_RODATA: u16 = 2;
const SEC_SYMTAB: u16 = 4;
const SEC_STRTAB: u16 = 5;
const SEC_SHSTRTAB: u16 = 6;
const SEC_COUNT: u16 = 7;

// Symbol indices: 0 null, 1 .text section, 2 .rodata section, 3 _start.
const SYM_RODATA: u32 = 2;
const SYM_START: u32 = 3;

/// Build the relocatable object for `code`.
pub fn emit_object(code: &ObjectCode) -> Vec<u8> {
    let mut strtab = vec![0u8];
    let start_name = strtab.len() as u32;
    strtab.extend_from_slice(b"_start\0");

    let mut shstr = vec![0u8];
    let mut shname = |name: &str| -> u32 {
        let off = shstr.len() as u32;
        shstr.extend_from_slice(name.as_bytes());
        shstr.push(0);
        off
    };
    let n_text = shname(".text");
    let n_rodata = shname(".rodata");
    let n_rela = shname(".rela.text");
    let n_symtab = shname(".symtab");
    let n_strtab = shname(".strtab");
    let n_shstrtab = shname(".shstrtab");

    let mut symtab = vec![0u8; 24];
    symtab.extend_from_slice(&sym(0, STB_LOCAL, STT_SECTION, SEC_TEXT, 0, 0));
    symtab.extend_from_slice(&sym(0, STB_LOCAL, STT_SECTION, SEC_RODATA, 0, 0));
    symtab.extend_from_slice(&sym(
        start_name,
        STB_GLOBAL,
        STT_FUNC,
        SEC_TEXT,
        0,
        code.text.len() as u64,
    ));

    let mut rela = Vec::with_capacity(code.relocs.len() * 24);
    for &(text_off, rodata_off) in &code.relocs {
        rela.extend_from_slice(&(text_off as u64).to_le_bytes());
        rela.extend_from_slice(&(((SYM_RODATA as u64) << 32) | R_X86_64_64 as u64).to_le_bytes());
        rela.extend_from_slice(&(rodata_off as i64).to_le_bytes());
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
    let text_off = place(&mut out, &code.text, 16);
    let rodata_off = place(&mut out, &code.rodata, 1);
    let rela_off = place(&mut out, &rela, 8);
    let symtab_off = place(&mut out, &symtab, 8);
    let strtab_off = place(&mut out, &strtab, 1);
    let shstr_off = place(&mut out, &shstr, 1);
    let shoff = place(&mut out, &[], 8);

    let headers: [[u64; 10]; SEC_COUNT as usize] = [
        [0; 10],
        [
            n_text as u64,
            SHT_PROGBITS as u64,
            SHF_ALLOC | SHF_EXECINSTR,
            0,
            text_off,
            code.text.len() as u64,
            0,
            0,
            16,
            0,
        ],
        [
            n_rodata as u64,
            SHT_PROGBITS as u64,
            SHF_ALLOC,
            0,
            rodata_off,
            code.rodata.len() as u64,
            0,
            0,
            1,
            0,
        ],
        [
            n_rela as u64,
            SHT_RELA as u64,
            0,
            0,
            rela_off,
            rela.len() as u64,
            SEC_SYMTAB as u64,
            SEC_TEXT as u64,
            8,
            24,
        ],
        [
            n_symtab as u64,
            SHT_SYMTAB as u64,
            0,
            0,
            symtab_off,
            symtab.len() as u64,
            SEC_STRTAB as u64,
            SYM_START as u64,
            8,
            24,
        ],
        [
            n_strtab as u64,
            SHT_STRTAB as u64,
            0,
            0,
            strtab_off,
            strtab.len() as u64,
            0,
            0,
            1,
            0,
        ],
        [
            n_shstrtab as u64,
            SHT_STRTAB as u64,
            0,
            0,
            shstr_off,
            shstr.len() as u64,
            0,
            0,
            1,
            0,
        ],
    ];
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
    out[4] = 2; // ELFCLASS64
    out[5] = 1; // ELFDATA2LSB
    out[6] = 1; // EV_CURRENT
    out[16..18].copy_from_slice(&1u16.to_le_bytes()); // ET_REL
    out[18..20].copy_from_slice(&0x3Eu16.to_le_bytes()); // EM_X86_64
    out[20..24].copy_from_slice(&1u32.to_le_bytes());
    out[40..48].copy_from_slice(&shoff.to_le_bytes());
    out[52..54].copy_from_slice(&64u16.to_le_bytes());
    out[58..60].copy_from_slice(&64u16.to_le_bytes());
    out[60..62].copy_from_slice(&SEC_COUNT.to_le_bytes());
    out[62..64].copy_from_slice(&SEC_SHSTRTAB.to_le_bytes());
    out
}

fn sym(name: u32, bind: u8, stype: u8, shndx: u16, value: u64, size: u64) -> [u8; 24] {
    let mut e = [0u8; 24];
    e[0..4].copy_from_slice(&name.to_le_bytes());
    e[4] = (bind << 4) | stype;
    e[6..8].copy_from_slice(&shndx.to_le_bytes());
    e[8..16].copy_from_slice(&value.to_le_bytes());
    e[16..24].copy_from_slice(&size.to_le_bytes());
    e
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::codegen;
    use crate::compile_check;

    #[test]
    fn object_is_et_rel_with_text_rodata_rela() {
        let prog = compile_check(include_str!("../examples/hello.gxr")).unwrap();
        let obj = emit_object(&codegen(&prog));
        assert!(obj.starts_with(b"\x7fELF"));
        assert_eq!(u16::from_le_bytes([obj[16], obj[17]]), 1, "ET_REL");
        assert_eq!(u16::from_le_bytes([obj[60], obj[61]]), SEC_COUNT);
    }
}
