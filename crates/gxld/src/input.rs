//! Input parsing: ELF64 x86_64 relocatables and `ar` archives of them,
//! read with the `object` crate and copied into an owned, lifetime-free
//! form the rest of the linker works on.

use crate::elf::*;
use crate::error::{Error, Result};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use object::elf::{FileHeader64, Rela64, SectionHeader64, Sym64};
use object::read::archive::ArchiveFile;
use object::read::elf::{ElfFile, FileHeader, Rela, SectionHeader, Sym};
use object::read::{SectionIndex, SymbolIndex};
use object::LittleEndian;

type Elf = FileHeader64<LittleEndian>;
type File<'a> = ElfFile<'a, Elf, &'a [u8]>;

/// Where a section's bytes end up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// `R` segment.
    Rodata,
    /// `RX` segment.
    Text,
    /// `RW` segment, file-backed.
    Data,
    /// `RW` segment, zero-filled.
    Bss,
    /// Not in the image (debug, notes, groups, COMDAT losers, …).
    Discard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Placement {
    pub seg: usize,
    pub offset: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct Reloc {
    pub offset: u64,
    pub rtype: u32,
    pub sym: usize,
    pub addend: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct Section {
    pub name: String,
    pub flags: u64,
    pub align: u64,
    pub size: u64,
    pub data: Vec<u8>,
    pub relocs: Vec<Reloc>,
    pub class: Class,
    pub live: bool,
    pub out: Option<Placement>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shndx {
    Undef,
    Abs,
    Common,
    Section(usize),
}

#[derive(Debug, Clone)]
pub(crate) struct Symbol {
    pub name: String,
    pub bind: u8,
    pub stype: u8,
    pub shndx: Shndx,
    pub value: u64,
    pub size: u64,
    /// Output address once a `COMMON` symbol has been allocated.
    pub common_addr: Option<u64>,
}

#[derive(Debug, Clone)]
pub(crate) struct Object {
    pub name: String,
    pub sections: Vec<Section>,
    pub symbols: Vec<Symbol>,
    /// COMDAT group signatures this object carries (first claimer wins).
    pub comdats: Vec<(String, Vec<usize>)>,
    /// Explicit objects and pulled archive members take part in the link.
    pub loaded: bool,
}

impl Object {
    pub(crate) fn section_name(&self, idx: usize) -> &str {
        self.sections
            .get(idx)
            .map(|s| s.name.as_str())
            .unwrap_or("?")
    }
}

/// Does `bytes` look like an `ar` archive?
pub(crate) fn is_archive(bytes: &[u8]) -> bool {
    bytes.starts_with(b"!<arch>\n") || bytes.starts_with(b"!<thin>\n")
}

/// Parse every ELF member of an archive. Non-ELF members (`lib.rmeta`) are
/// skipped. Members come back unloaded.
pub(crate) fn parse_archive(name: &str, bytes: &[u8]) -> Result<Vec<Object>> {
    let archive = ArchiveFile::parse(bytes).map_err(|e| Error::Input {
        input: name.to_string(),
        detail: format!("archive: {e}"),
    })?;
    let mut members = Vec::new();
    for member in archive.members() {
        let member = member.map_err(|e| Error::Input {
            input: name.to_string(),
            detail: format!("archive member: {e}"),
        })?;
        let member_name = String::from_utf8_lossy(member.name()).into_owned();
        let data = member.data(bytes).map_err(|e| Error::Input {
            input: format!("{name}({member_name})"),
            detail: format!("archive member data: {e}"),
        })?;
        if !data.starts_with(b"\x7fELF") {
            continue;
        }
        let mut obj = parse_object(&format!("{name}({member_name})"), data)?;
        obj.loaded = false;
        members.push(obj);
    }
    Ok(members)
}

/// Parse one relocatable object. The result is `loaded`.
pub(crate) fn parse_object(name: &str, bytes: &[u8]) -> Result<Object> {
    let input = |detail: String| Error::Input {
        input: name.to_string(),
        detail,
    };
    let file: File<'_> = File::parse(bytes).map_err(|e| input(format!("ELF: {e}")))?;
    let endian = file.endian();
    let header = file.elf_header();
    let e_type = header.e_type(endian).0;
    if e_type != ET_REL {
        return Err(input(format!(
            "need ET_REL (relocatable), got e_type {e_type}"
        )));
    }
    let e_machine = header.e_machine(endian).0;
    if e_machine != EM_X86_64 {
        return Err(input(format!("need EM_X86_64, got e_machine {e_machine}")));
    }
    let sections = file.elf_section_table();
    let symtab = file.elf_symbol_table();
    let rel_map = sections
        .relocation_sections(endian, symtab.section())
        .map_err(|e| input(format!("relocation sections: {e}")))?;

    // Symbols first: COMDAT signatures and relocations refer to them.
    let mut symbols = Vec::with_capacity(symtab.len());
    for (i, sym) in symtab.iter().enumerate() {
        let sym: &Sym64<LittleEndian> = sym;
        let name = symtab
            .symbol_name(endian, sym)
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .unwrap_or_default();
        let raw = sym.st_shndx(endian).0;
        let shndx = match raw {
            SHN_UNDEF => Shndx::Undef,
            SHN_ABS => Shndx::Abs,
            SHN_COMMON => Shndx::Common,
            SHN_XINDEX => match symtab.symbol_section(endian, sym, SymbolIndex(i)) {
                Ok(Some(SectionIndex(s))) => Shndx::Section(s),
                _ => return Err(input(format!("symbol {i}: bad extended section index"))),
            },
            s if s >= 0xff00 => {
                return Err(input(format!("symbol {i}: reserved section index {s:#x}")))
            }
            s => Shndx::Section(s as usize),
        };
        symbols.push(Symbol {
            name,
            bind: sym.st_bind().0,
            stype: sym.st_type().0,
            shndx,
            value: sym.st_value(endian),
            size: sym.st_size(endian),
            common_addr: None,
        });
    }

    let mut out_sections = Vec::with_capacity(sections.len());
    let mut comdats = Vec::new();
    for (SectionIndex(idx), sh) in sections.enumerate() {
        let sh: &SectionHeader64<LittleEndian> = sh;
        let sec_name = sections
            .section_name(endian, sh)
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .unwrap_or_default();
        let sh_type = sh.sh_type(endian).0;
        let flags = sh.sh_flags(endian).0;
        let size = sh.sh_size(endian);
        let align = sh.sh_addralign(endian).max(1);
        if !align.is_power_of_two() {
            return Err(input(format!(
                "section {sec_name}: alignment {align} not a power of two"
            )));
        }

        if sh_type == SHT_GROUP {
            let (gflags, members) = sh
                .group(endian, bytes)
                .map_err(|e| input(format!("section {sec_name}: group: {e}")))?
                .ok_or_else(|| input(format!("section {sec_name}: not a group")))?;
            if gflags.0 & GRP_COMDAT != 0 {
                let sig_index = sh.sh_info(endian) as usize;
                let sig = symbols
                    .get(sig_index)
                    .map(|s| s.name.clone())
                    .ok_or_else(|| {
                        input(format!(
                            "section {sec_name}: COMDAT signature symbol {sig_index} missing"
                        ))
                    })?;
                let members: Vec<usize> = members.iter().map(|m| m.get(endian) as usize).collect();
                comdats.push((sig, members));
            }
        }

        let data = if sh_type == SHT_NOBITS {
            Vec::new()
        } else {
            sh.data(endian, bytes)
                .map_err(|e| input(format!("section {sec_name}: data: {e}")))?
                .to_vec()
        };

        let class = classify(name, &sec_name, sh_type, flags)?;

        // Relocations against this section (possibly several `.rela` tables).
        let mut relocs = Vec::new();
        if class != Class::Discard {
            let mut cursor = SectionIndex(idx);
            while let Some(rel_idx) = rel_map.get(cursor) {
                let rel_sh = sections
                    .section(rel_idx)
                    .map_err(|e| input(format!("section {sec_name}: relocation table: {e}")))?;
                if rel_sh.sh_type(endian).0 == SHT_REL {
                    return Err(Error::Unsupported {
                        what: format!("REL (implicit-addend) relocations against {sec_name}"),
                        input: name.to_string(),
                    });
                }
                let (entries, _link) = rel_sh
                    .rela(endian, bytes)
                    .map_err(|e| input(format!("section {sec_name}: rela: {e}")))?
                    .ok_or_else(|| {
                        input(format!("section {sec_name}: relocation table is not RELA"))
                    })?;
                for r in entries {
                    let r: &Rela64<LittleEndian> = r;
                    let offset = r.r_offset(endian);
                    let rtype = r.r_type(endian, false).0;
                    let sym = r.r_sym(endian, false) as usize;
                    if sym >= symbols.len() {
                        return Err(input(format!(
                            "section {sec_name}: relocation symbol index {sym} out of range"
                        )));
                    }
                    if offset >= size && rtype != R_X86_64_NONE {
                        return Err(input(format!(
                            "section {sec_name}: relocation offset {offset:#x} beyond size {size:#x}"
                        )));
                    }
                    relocs.push(Reloc {
                        offset,
                        rtype,
                        sym,
                        addend: r.r_addend(endian),
                    });
                }
                cursor = rel_idx;
            }
        }

        out_sections.push(Section {
            name: sec_name,
            flags,
            align,
            size,
            data,
            relocs,
            class,
            live: false,
            out: None,
        });
    }

    // Symbols must point at real sections.
    for (i, s) in symbols.iter().enumerate() {
        if let Shndx::Section(idx) = s.shndx {
            if idx >= out_sections.len() {
                return Err(input(format!(
                    "symbol {i} `{}`: section index {idx} out of range",
                    s.name
                )));
            }
            if s.stype != STT_SECTION && s.stype != STT_FILE && s.value > out_sections[idx].size {
                return Err(input(format!(
                    "symbol `{}`: value beyond its section",
                    s.name
                )));
            }
        }
    }

    Ok(Object {
        name: name.to_string(),
        sections: out_sections,
        symbols,
        comdats,
        loaded: true,
    })
}

fn classify(input: &str, name: &str, sh_type: u32, flags: u64) -> Result<Class> {
    if flags & SHF_ALLOC == 0 {
        return Ok(Class::Discard);
    }
    if flags & SHF_TLS != 0 {
        return Err(Error::Unsupported {
            what: format!("TLS section {name} (Milestone 70)"),
            input: input.to_string(),
        });
    }
    if matches!(sh_type, SHT_INIT_ARRAY | SHT_FINI_ARRAY | SHT_PREINIT_ARRAY) {
        return Err(Error::Unsupported {
            what: format!("{name}: static constructors are not run on Galexy"),
            input: input.to_string(),
        });
    }
    let discard_prefixes = [
        ".eh_frame",
        ".gcc_except_table",
        ".note",
        ".comment",
        ".llvm_addrsig",
        ".debug",
    ];
    if discard_prefixes.iter().any(|p| name.starts_with(p)) {
        return Ok(Class::Discard);
    }
    if sh_type != SHT_PROGBITS && sh_type != SHT_NOBITS {
        return Ok(Class::Discard);
    }
    Ok(if flags & SHF_EXECINSTR != 0 {
        Class::Text
    } else if flags & SHF_WRITE != 0 {
        if sh_type == SHT_NOBITS {
            Class::Bss
        } else {
            Class::Data
        }
    } else {
        Class::Rodata
    })
}
