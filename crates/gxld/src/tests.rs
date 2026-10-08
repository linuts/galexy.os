//! Host tests over hand-built relocatable objects: no toolchain needed.

use crate::elf::*;
use crate::{link, Error, Input, Options};
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

const BASE: u64 = galexy_abi::USER_IMAGE_BASE;

struct Sec {
    name: &'static str,
    sh_type: u32,
    flags: u64,
    align: u64,
    data: Vec<u8>,
    /// NOBITS size.
    size: u64,
    relocs: Vec<(u64, u32, &'static str, i64)>,
}

struct Sym {
    name: &'static str,
    bind: u8,
    stype: u8,
    /// Section name, "" for undefined, "*ABS*" for absolute, "*COM*" for common.
    sec: &'static str,
    value: u64,
    size: u64,
}

/// Minimal ELF64 `ET_REL` writer.
struct Obj {
    secs: Vec<Sec>,
    syms: Vec<Sym>,
}

impl Obj {
    fn new() -> Self {
        Obj {
            secs: Vec::new(),
            syms: Vec::new(),
        }
    }
    fn text(mut self, data: &[u8]) -> Self {
        self.secs.push(Sec {
            name: ".text",
            sh_type: SHT_PROGBITS,
            flags: SHF_ALLOC | SHF_EXECINSTR,
            align: 16,
            data: data.to_vec(),
            size: 0,
            relocs: Vec::new(),
        });
        self
    }
    fn section(mut self, name: &'static str, sh_type: u32, flags: u64, data: &[u8]) -> Self {
        self.secs.push(Sec {
            name,
            sh_type,
            flags,
            align: 8,
            data: data.to_vec(),
            size: if sh_type == SHT_NOBITS {
                data.len() as u64
            } else {
                0
            },
            relocs: Vec::new(),
        });
        self
    }
    fn reloc(mut self, sec: &str, offset: u64, rtype: u32, sym: &'static str, addend: i64) -> Self {
        let s = self.secs.iter_mut().find(|s| s.name == sec).unwrap();
        s.relocs.push((offset, rtype, sym, addend));
        self
    }
    fn sym(mut self, name: &'static str, bind: u8, sec: &'static str, value: u64) -> Self {
        self.syms.push(Sym {
            name,
            bind,
            stype: 0,
            sec,
            value,
            size: 0,
        });
        self
    }
    fn sym_sized(
        mut self,
        name: &'static str,
        bind: u8,
        sec: &'static str,
        value: u64,
        size: u64,
    ) -> Self {
        self.syms.push(Sym {
            name,
            bind,
            stype: 1,
            sec,
            value,
            size,
        });
        self
    }

    fn build(&self) -> Vec<u8> {
        // Section indices: 0 null, 1..=n user, then symtab, strtab, shstrtab,
        // then one .rela per user section with relocations.
        let n = self.secs.len();
        let symtab_idx = n + 1;
        let strtab_idx = n + 2;
        let shstrtab_idx = n + 3;
        let mut shstr: Vec<u8> = vec![0];
        let mut strtab: Vec<u8> = vec![0];
        let add_str = |tab: &mut Vec<u8>, s: &str| -> u32 {
            let off = tab.len() as u32;
            tab.extend_from_slice(s.as_bytes());
            tab.push(0);
            off
        };
        // Symbols: null, then one STT_SECTION per user section, then named.
        let mut symtab: Vec<u8> = vec![0; 24];
        let sym_index = |name: &str, syms: &[Sym]| -> u32 {
            if let Some(i) = self.secs.iter().position(|s| s.name == name) {
                return (1 + i) as u32;
            }
            let i = syms
                .iter()
                .position(|s| s.name == name)
                .expect("symbol exists");
            (1 + n + i) as u32
        };
        for (i, _) in self.secs.iter().enumerate() {
            let mut e = [0u8; 24];
            e[4] = (STB_LOCAL << 4) | STT_SECTION;
            e[6..8].copy_from_slice(&((1 + i) as u16).to_le_bytes());
            symtab.extend_from_slice(&e);
        }
        for s in &self.syms {
            let mut e = [0u8; 24];
            let name_off = add_str(&mut strtab, s.name);
            e[0..4].copy_from_slice(&name_off.to_le_bytes());
            e[4] = (s.bind << 4) | s.stype;
            let shndx: u16 = match s.sec {
                "" => SHN_UNDEF,
                "*ABS*" => SHN_ABS,
                "*COM*" => SHN_COMMON,
                name => (1 + self.secs.iter().position(|x| x.name == name).unwrap()) as u16,
            };
            e[6..8].copy_from_slice(&shndx.to_le_bytes());
            e[8..16].copy_from_slice(&s.value.to_le_bytes());
            e[16..24].copy_from_slice(&s.size.to_le_bytes());
            symtab.extend_from_slice(&e);
        }
        let mut rela_blobs: Vec<(usize, Vec<u8>, u32)> = Vec::new();
        for (i, s) in self.secs.iter().enumerate() {
            if s.relocs.is_empty() {
                continue;
            }
            let mut blob = Vec::new();
            for (off, rtype, sym, addend) in &s.relocs {
                blob.extend_from_slice(&off.to_le_bytes());
                let info = ((sym_index(sym, &self.syms) as u64) << 32) | *rtype as u64;
                blob.extend_from_slice(&info.to_le_bytes());
                blob.extend_from_slice(&addend.to_le_bytes());
            }
            let name = String::from(".rela") + s.name;
            let name_off = add_str(&mut shstr, &name);
            rela_blobs.push((i, blob, name_off));
        }
        let sec_name_offs: Vec<u32> = self
            .secs
            .iter()
            .map(|s| add_str(&mut shstr, s.name))
            .collect();
        let symtab_name = add_str(&mut shstr, ".symtab");
        let strtab_name = add_str(&mut shstr, ".strtab");
        let shstrtab_name = add_str(&mut shstr, ".shstrtab");

        // Lay out data.
        let mut out = vec![0u8; 64];
        let mut headers: Vec<[u64; 10]> = vec![[0; 10]]; // name,type,flags,addr,off,size,link,info,align,entsize
        let place = |out: &mut Vec<u8>, data: &[u8], align: usize| -> u64 {
            while !out.len().is_multiple_of(align) {
                out.push(0);
            }
            let off = out.len() as u64;
            out.extend_from_slice(data);
            off
        };
        for (i, s) in self.secs.iter().enumerate() {
            let off = if s.sh_type == SHT_NOBITS {
                out.len() as u64
            } else {
                place(&mut out, &s.data, s.align as usize)
            };
            let size = if s.sh_type == SHT_NOBITS {
                s.size
            } else {
                s.data.len() as u64
            };
            headers.push([
                sec_name_offs[i] as u64,
                s.sh_type as u64,
                s.flags,
                0,
                off,
                size,
                0,
                0,
                s.align,
                0,
            ]);
        }
        let off = place(&mut out, &symtab, 8);
        let first_global = 1
            + n
            + self
                .syms
                .iter()
                .position(|s| s.bind != STB_LOCAL)
                .unwrap_or(self.syms.len());
        headers.push([
            symtab_name as u64,
            2,
            0,
            0,
            off,
            symtab.len() as u64,
            strtab_idx as u64,
            first_global as u64,
            8,
            24,
        ]);
        let off = place(&mut out, &strtab, 1);
        headers.push([
            strtab_name as u64,
            3,
            0,
            0,
            off,
            strtab.len() as u64,
            0,
            0,
            1,
            0,
        ]);
        let off = place(&mut out, &shstr, 1);
        headers.push([
            shstrtab_name as u64,
            3,
            0,
            0,
            off,
            shstr.len() as u64,
            0,
            0,
            1,
            0,
        ]);
        for (target, blob, name_off) in &rela_blobs {
            let off = place(&mut out, blob, 8);
            headers.push([
                *name_off as u64,
                4,
                0,
                0,
                off,
                blob.len() as u64,
                symtab_idx as u64,
                (1 + target) as u64,
                8,
                24,
            ]);
        }
        let shoff = place(&mut out, &[], 8);
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
        // ELF header.
        out[0..4].copy_from_slice(b"\x7fELF");
        out[4] = 2;
        out[5] = 1;
        out[6] = 1;
        write_u16(&mut out, 16, ET_REL);
        write_u16(&mut out, 18, EM_X86_64);
        write_u32(&mut out, 20, 1);
        write_u64(&mut out, 40, shoff);
        write_u16(&mut out, 52, 64);
        write_u16(&mut out, 58, 64);
        write_u16(&mut out, 60, headers.len() as u16);
        write_u16(&mut out, 62, shstrtab_idx as u16);
        out
    }
}

/// Minimal `ar` archive (GNU format, short names).
fn archive(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = b"!<arch>\n".to_vec();
    for (name, data) in members {
        let mut hdr = [b' '; 60];
        let n = alloc::format!("{name}/");
        hdr[..n.len()].copy_from_slice(n.as_bytes());
        hdr[16..28].copy_from_slice(b"0           ");
        hdr[28..34].copy_from_slice(b"0     ");
        hdr[34..40].copy_from_slice(b"0     ");
        hdr[40..48].copy_from_slice(b"100644  ");
        let size = alloc::format!("{:<10}", data.len());
        hdr[48..58].copy_from_slice(size.as_bytes());
        hdr[58..60].copy_from_slice(b"`\n");
        out.extend_from_slice(&hdr);
        out.extend_from_slice(data);
        if data.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

fn inputs<'a>(files: &'a [(&'a str, Vec<u8>)]) -> Vec<Input<'a>> {
    files
        .iter()
        .map(|(n, b)| Input {
            name: n.to_string(),
            bytes: b,
            whole_archive: false,
        })
        .collect()
}

fn phdrs(img: &[u8]) -> Vec<(u32, u64, u64, u64, u64)> {
    let n = read_u16(img, 56) as usize;
    (0..n)
        .map(|i| {
            let at = 64 + i * 56;
            (
                read_u32(img, at + 4),
                read_u64(img, at + 8),
                read_u64(img, at + 16),
                read_u64(img, at + 32),
                read_u64(img, at + 40),
            )
        })
        .collect()
}

fn hello_obj() -> Vec<u8> {
    // _start: lea rsi,[rip+msg]  (reloc PC32 at 3, addend -4)
    //         mov rax,[rip+GOT(counter)]  (GOTPCREL at 10, addend -4)
    //         ret
    let text = [
        0x48, 0x8d, 0x35, 0, 0, 0, 0, // lea rsi, [rip+disp32]
        0x48, 0x8b, 0x05, 0, 0, 0, 0, // mov rax, [rip+disp32]
        0xc3,
    ];
    Obj::new()
        .text(&text)
        .section(".rodata.str", SHT_PROGBITS, SHF_ALLOC, b"hello\n")
        .section(
            ".data",
            SHT_PROGBITS,
            SHF_ALLOC | SHF_WRITE,
            &[1, 2, 3, 4, 5, 6, 7, 8],
        )
        .section(
            ".bss",
            SHT_NOBITS,
            SHF_ALLOC | SHF_WRITE | SHF_GNU_RETAIN,
            &[0; 32],
        )
        .reloc(".text", 3, R_X86_64_PC32, "msg", -4)
        .reloc(".text", 10, R_X86_64_GOTPCREL, "counter", -4)
        .sym("msg", STB_LOCAL, ".rodata.str", 0)
        .sym("_start", STB_GLOBAL, ".text", 0)
        .sym("counter", STB_GLOBAL, ".data", 0)
        .sym("zeroed", STB_GLOBAL, ".bss", 0)
        .build()
}

#[test]
fn links_hello_shape() {
    let files = [("hello.o", hello_obj())];
    let img = link(&inputs(&files), &Options::default()).unwrap();
    crate::validate(&img, BASE).unwrap();
    let ph = phdrs(&img);
    assert_eq!(ph.len(), 3);
    assert_eq!(ph[0].0, PF_R);
    assert_eq!(ph[1].0, PF_R | PF_X);
    assert_eq!(ph[2].0, PF_R | PF_W);
    assert_eq!(ph[0].2, BASE);
    assert_eq!(ph[1].2, BASE + PAGE);
    assert_eq!(ph[2].2, BASE + 2 * PAGE);
    // bss is memsz beyond filesz.
    assert_eq!(ph[2].3, 8);
    assert_eq!(ph[2].4, 8 + 32);
    let entry = read_u64(&img, 24);
    assert_eq!(entry, BASE + PAGE);
    // PC32 to msg: rodata lands right after the headers (64 + 3*56 = 232).
    let msg_va = BASE + 232;
    let disp = read_u32(&img, (PAGE as usize) + 3) as i32 as i64;
    assert_eq!(entry as i64 + 7 + disp, msg_va as i64);
    assert_eq!(&img[232..238], b"hello\n");
    // GOTPCREL: the GOT slot holds counter's address, the displacement reaches the slot.
    let got_va = BASE + align_up(238, 8);
    let disp = read_u32(&img, (PAGE as usize) + 10) as i32 as i64;
    assert_eq!(entry as i64 + 14 + disp, got_va as i64);
    let slot = read_u64(&img, (got_va - BASE) as usize);
    assert_eq!(slot, BASE + 2 * PAGE);
}

#[test]
fn archive_member_pulled_on_demand() {
    let main = Obj::new()
        .text(&[0xe8, 0, 0, 0, 0, 0xc3])
        .reloc(".text", 1, R_X86_64_PLT32, "helper", -4)
        .sym("_start", STB_GLOBAL, ".text", 0)
        .sym("helper", STB_GLOBAL, "", 0)
        .build();
    let helper = Obj::new()
        .text(&[0x90, 0x90, 0xc3])
        .sym("helper", STB_GLOBAL, ".text", 0)
        .build();
    let unused = Obj::new()
        .text(&[0xcc; 64])
        .sym("unused", STB_GLOBAL, ".text", 0)
        .build();
    let ar = archive(&[
        ("unused.o", &unused),
        ("helper.o", &helper),
        ("lib.rmeta", b"not elf"),
    ]);
    let files = [("main.o", main), ("libx.rlib", ar)];
    let img = link(&inputs(&files), &Options::default()).unwrap();
    let ph = phdrs(&img);
    // text = main (6) aligned 16 + helper (3): the 64 bytes of 0xcc never arrive.
    assert_eq!(ph[1].3, 16 + 3);
    let disp = read_u32(&img, PAGE as usize + 1) as i32 as i64;
    assert_eq!((BASE + PAGE) as i64 + 5 + disp, (BASE + PAGE + 16) as i64);
}

#[test]
fn whole_archive_loads_everything() {
    let main = Obj::new()
        .text(&[0xc3])
        .sym("_start", STB_GLOBAL, ".text", 0)
        .build();
    let unused = Obj::new()
        .section(
            ".data.keep",
            SHT_PROGBITS,
            SHF_ALLOC | SHF_WRITE | SHF_GNU_RETAIN,
            &[7; 8],
        )
        .sym("unused", STB_GLOBAL, ".data.keep", 0)
        .build();
    let ar = archive(&[("unused.o", &unused)]);
    let files = [("main.o", main), ("libx.a", ar)];
    let mut ins = inputs(&files);
    ins[1].whole_archive = true;
    let img = link(&ins, &Options::default()).unwrap();
    assert_eq!(phdrs(&img)[2].3, 8);
}

#[test]
fn gc_drops_unreferenced_sections() {
    let obj = Obj::new()
        .text(&[0xc3])
        .section(".rodata.dead", SHT_PROGBITS, SHF_ALLOC, &[0xAB; 100])
        .sym("_start", STB_GLOBAL, ".text", 0)
        .sym("dead", STB_GLOBAL, ".rodata.dead", 0)
        .build();
    let files = [("a.o", obj)];
    let img = link(&inputs(&files), &Options::default()).unwrap();
    assert_eq!(phdrs(&img)[0].3, 232, "rodata segment is just the headers");
    let opts = Options {
        gc_sections: false,
        ..Options::default()
    };
    let img = link(&inputs(&files), &opts).unwrap();
    assert_eq!(phdrs(&img)[0].3, 232 + 100);
}

#[test]
fn undefined_symbol_is_an_error_only_when_live() {
    let obj = Obj::new()
        .text(&[0xe8, 0, 0, 0, 0, 0xc3])
        .section(
            ".text.dead",
            SHT_PROGBITS,
            SHF_ALLOC | SHF_EXECINSTR,
            &[0xe8, 0, 0, 0, 0],
        )
        .reloc(".text.dead", 1, R_X86_64_PLT32, "ghost", -4)
        .reloc(".text", 1, R_X86_64_PLT32, "missing", -4)
        .sym("_start", STB_GLOBAL, ".text", 0)
        .sym("ghost", STB_GLOBAL, "", 0)
        .sym("missing", STB_GLOBAL, "", 0)
        .build();
    let files = [("a.o", obj)];
    match link(&inputs(&files), &Options::default()) {
        Err(Error::Undefined {
            symbol,
            referenced_by,
        }) => {
            assert_eq!(symbol, "missing");
            assert_eq!(referenced_by, "a.o(.text)");
        }
        other => panic!("expected Undefined, got {other:?}"),
    }
}

#[test]
fn weak_undefined_resolves_without_error() {
    let obj = Obj::new()
        .text(&[0xe8, 0, 0, 0, 0, 0xc3])
        .reloc(".text", 1, R_X86_64_PLT32, "maybe", -4)
        .sym("_start", STB_GLOBAL, ".text", 0)
        .sym("maybe", STB_WEAK, "", 0)
        .build();
    let files = [("a.o", obj)];
    let img = link(&inputs(&files), &Options::default()).unwrap();
    // Resolved to the location itself: displacement is just the addend.
    assert_eq!(read_u32(&img, PAGE as usize + 1) as i32, -4);
}

#[test]
fn duplicate_strong_is_an_error_weak_is_overridden() {
    let a = Obj::new()
        .text(&[0xc3])
        .sym("_start", STB_GLOBAL, ".text", 0)
        .sym("f", STB_GLOBAL, ".text", 0)
        .build();
    let b = Obj::new()
        .text(&[0x90, 0xc3])
        .sym("f", STB_GLOBAL, ".text", 0)
        .build();
    let files = [("a.o", a.clone()), ("b.o", b)];
    assert!(matches!(
        link(&inputs(&files), &Options::default()),
        Err(Error::Duplicate { .. })
    ));
    let w = Obj::new()
        .text(&[0x90, 0xc3])
        .sym("f", STB_WEAK, ".text", 0)
        .build();
    let files = [("w.o", w), ("a.o", a)];
    link(&inputs(&files), &Options::default()).unwrap();
}

#[test]
fn entry_required() {
    let obj = Obj::new()
        .text(&[0xc3])
        .sym("main", STB_GLOBAL, ".text", 0)
        .build();
    let files = [("a.o", obj)];
    assert!(matches!(
        link(&inputs(&files), &Options::default()),
        Err(Error::MissingEntry(_))
    ));
}

#[test]
fn absolute_32bit_relocation_overflows_above_4g() {
    let obj = Obj::new()
        .text(&[0xb8, 0, 0, 0, 0, 0xc3])
        .reloc(".text", 1, R_X86_64_32, "_start", 0)
        .sym("_start", STB_GLOBAL, ".text", 0)
        .build();
    let files = [("a.o", obj)];
    assert!(matches!(
        link(&inputs(&files), &Options::default()),
        Err(Error::Overflow {
            rtype: R_X86_64_32,
            ..
        })
    ));
}

#[test]
fn unsupported_constructs_are_errors_not_panics() {
    let tls = Obj::new()
        .text(&[0xc3])
        .section(
            ".tbss",
            SHT_NOBITS,
            SHF_ALLOC | SHF_WRITE | SHF_TLS,
            &[0; 8],
        )
        .sym("_start", STB_GLOBAL, ".text", 0)
        .build();
    let files = [("a.o", tls)];
    assert!(matches!(
        link(&inputs(&files), &Options::default()),
        Err(Error::Unsupported { .. })
    ));
    let odd = Obj::new()
        .text(&[0, 0, 0, 0, 0, 0, 0, 0, 0xc3])
        .reloc(".text", 0, 99, "_start", 0)
        .sym("_start", STB_GLOBAL, ".text", 0)
        .build();
    let files = [("a.o", odd)];
    assert!(matches!(
        link(&inputs(&files), &Options::default()),
        Err(Error::Unsupported { .. })
    ));
}

#[test]
fn hostile_inputs_return_errors() {
    let good = hello_obj();
    for cut in [0usize, 3, 16, 63, 64, 200, good.len() - 1] {
        let files = [("cut.o", good[..cut].to_vec())];
        let r = link(&inputs(&files), &Options::default());
        assert!(r.is_err(), "truncated at {cut} must fail");
    }
    let files = [("junk", b"definitely not an object".to_vec())];
    assert!(matches!(
        link(&inputs(&files), &Options::default()),
        Err(Error::Input { .. })
    ));
    let out_of_section = Obj::new()
        .text(&[0xc3])
        .reloc(".text", 40, R_X86_64_PC32, "_start", 0)
        .sym("_start", STB_GLOBAL, ".text", 0)
        .build();
    let files = [("a.o", out_of_section)];
    assert!(matches!(
        link(&inputs(&files), &Options::default()),
        Err(Error::Input { .. })
    ));
    let files = [("bad.a", archive(&[("x.o", b"\x7fELF garbage")]))];
    assert!(link(&inputs(&files), &Options::default()).is_err());
}

#[test]
fn common_symbols_land_in_bss() {
    let obj = Obj::new()
        .text(&[0x48, 0x8b, 0x05, 0, 0, 0, 0, 0xc3])
        .reloc(".text", 3, R_X86_64_GOTPCREL, "shared", -4)
        .sym("_start", STB_GLOBAL, ".text", 0)
        .sym_sized("shared", STB_GLOBAL, "*COM*", 16, 64)
        .build();
    let files = [("a.o", obj)];
    let img = link(&inputs(&files), &Options::default()).unwrap();
    let ph = phdrs(&img);
    assert_eq!(ph[2].3, 0);
    assert_eq!(ph[2].4, 64);
    let got = read_u64(&img, 232);
    assert_eq!(got, BASE + 2 * PAGE);
}

#[test]
fn linker_defined_symbols() {
    let obj = Obj::new()
        .section(
            ".data",
            SHT_PROGBITS,
            SHF_ALLOC | SHF_WRITE | SHF_GNU_RETAIN,
            &[0; 16],
        )
        .text(&[0xc3])
        .reloc(".data", 0, R_X86_64_64, "_end", 0)
        .reloc(".data", 8, R_X86_64_64, "__executable_start", 0)
        .sym("_start", STB_GLOBAL, ".text", 0)
        .sym("_end", STB_GLOBAL, "", 0)
        .sym("__executable_start", STB_GLOBAL, "", 0)
        .build();
    let files = [("a.o", obj)];
    let img = link(&inputs(&files), &Options::default()).unwrap();
    let data_off = 2 * PAGE as usize;
    assert_eq!(read_u64(&img, data_off), BASE + 2 * PAGE + 16);
    assert_eq!(read_u64(&img, data_off + 8), BASE);
}

#[test]
fn validate_rejects_wx() {
    let files = [("hello.o", hello_obj())];
    let mut img = link(&inputs(&files), &Options::default()).unwrap();
    write_u32(&mut img, 64 + 56 + 4, PF_R | PF_W | PF_X);
    assert!(crate::validate(&img, BASE).is_err());
}
