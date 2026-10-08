//! The link: resolve symbols, pull archive members, drop dead sections,
//! lay out three W^X segments at the image base, apply relocations, emit.

use crate::elf::*;
use crate::error::{Error, Result};
use crate::input::{self, Class, Object, Placement, Shndx};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

/// One command-line input: an object or an archive.
#[derive(Debug, Clone)]
pub struct Input<'a> {
    /// Name used in diagnostics.
    pub name: String,
    /// File bytes.
    pub bytes: &'a [u8],
    /// Load every member of an archive, not just the ones referenced.
    pub whole_archive: bool,
}

/// Link options.
#[derive(Debug, Clone)]
pub struct Options {
    /// Virtual address of the first byte of the image (ELF header).
    pub image_base: u64,
    /// Entry symbol.
    pub entry: String,
    /// Drop sections not reachable from the entry.
    pub gc_sections: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            image_base: galexy_abi::USER_IMAGE_BASE,
            entry: "_start".to_string(),
            gc_sections: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SymKey {
    Def(usize, usize),
    Named(usize),
}

#[derive(Debug, Clone, Copy)]
struct Def {
    obj: usize,
    sym: usize,
    weak: bool,
}

const SEG_R: usize = 0;
const SEG_RX: usize = 1;
const SEG_RW: usize = 2;

struct Segment {
    vaddr: u64,
    filesz: u64,
    memsz: u64,
    buf: Vec<u8>,
    flags: u32,
}

struct Link {
    objects: Vec<Object>,
    table: BTreeMap<String, Def>,
    linker_defined: BTreeMap<String, u64>,
    got: BTreeMap<SymKey, u64>,
    got_names: Vec<String>,
    got_addr: u64,
    segments: Vec<Segment>,
    image_base: u64,
}

/// Link `inputs` into a static `ET_EXEC` for the Galexy loader.
pub fn link(inputs: &[Input<'_>], opts: &Options) -> Result<Vec<u8>> {
    if !opts.image_base.is_multiple_of(PAGE) {
        return Err(Error::Args(format!(
            "image base {:#x} is not page-aligned",
            opts.image_base
        )));
    }
    let mut objects = Vec::new();
    for inp in inputs {
        if input::is_archive(inp.bytes) {
            let mut members = input::parse_archive(&inp.name, inp.bytes)?;
            if inp.whole_archive {
                for m in &mut members {
                    m.loaded = true;
                }
            }
            objects.extend(members);
        } else if inp.bytes.starts_with(b"\x7fELF") {
            objects.push(input::parse_object(&inp.name, inp.bytes)?);
        } else {
            return Err(Error::Input {
                input: inp.name.clone(),
                detail: "not an ELF object or an ar archive".to_string(),
            });
        }
    }

    let mut link = Link {
        objects,
        table: BTreeMap::new(),
        linker_defined: BTreeMap::new(),
        got: BTreeMap::new(),
        got_names: Vec::new(),
        got_addr: 0,
        segments: Vec::new(),
        image_base: opts.image_base,
    };
    link.resolve()?;
    link.mark_live(&opts.entry, opts.gc_sections)?;
    link.check_undefined()?;
    link.collect_got()?;
    link.layout()?;
    link.apply_relocations()?;
    let entry = link
        .lookup_global(&opts.entry)
        .ok_or_else(|| Error::MissingEntry(opts.entry.clone()))?;
    let out = link.emit(entry);
    validate(&out, opts.image_base)?;
    Ok(out)
}

impl Link {
    // ---------------- symbol resolution ----------------

    fn resolve(&mut self) -> Result<()> {
        // COMDAT: first claimer keeps its members, later ones drop theirs.
        let mut claimed: BTreeMap<String, usize> = BTreeMap::new();
        let mut comdat_losers: Vec<(usize, Vec<usize>)> = Vec::new();
        for (oi, obj) in self.objects.iter().enumerate() {
            for (sig, members) in &obj.comdats {
                if claimed.contains_key(sig) {
                    comdat_losers.push((oi, members.clone()));
                } else {
                    claimed.insert(sig.clone(), oi);
                }
            }
        }
        for (oi, members) in comdat_losers {
            for m in members {
                if let Some(sec) = self.objects[oi].sections.get_mut(m) {
                    sec.class = Class::Discard;
                    sec.relocs.clear();
                }
            }
        }

        // Archive index: first member defining a global wins.
        let mut archive_index: BTreeMap<String, usize> = BTreeMap::new();
        for (oi, obj) in self.objects.iter().enumerate() {
            if obj.loaded {
                continue;
            }
            for s in &obj.symbols {
                if s.bind != STB_LOCAL
                    && s.shndx != Shndx::Undef
                    && !s.name.is_empty()
                    && !matches!(s.stype, STT_SECTION | STT_FILE)
                {
                    archive_index.entry(s.name.clone()).or_insert(oi);
                }
            }
        }

        // Pass 1: definitions from loaded objects, in order.
        let mut queue: VecDeque<usize> = (0..self.objects.len())
            .filter(|&i| self.objects[i].loaded)
            .collect();
        let mut undefined: BTreeMap<String, ()> = BTreeMap::new();
        loop {
            while let Some(oi) = queue.pop_front() {
                self.define_from(oi, &mut undefined)?;
            }
            // Pass 2: pull members for strong undefined references.
            let mut pulled = false;
            let names: Vec<String> = undefined.keys().cloned().collect();
            for name in names {
                if self.table.contains_key(&name) {
                    undefined.remove(&name);
                    continue;
                }
                if let Some(&mi) = archive_index.get(&name) {
                    if !self.objects[mi].loaded {
                        self.objects[mi].loaded = true;
                        queue.push_back(mi);
                        pulled = true;
                    }
                }
            }
            if !pulled {
                break;
            }
        }
        Ok(())
    }

    fn define_from(&mut self, oi: usize, undefined: &mut BTreeMap<String, ()>) -> Result<()> {
        let nsyms = self.objects[oi].symbols.len();
        for si in 0..nsyms {
            let (name, bind, stype, shndx) = {
                let s = &self.objects[oi].symbols[si];
                (s.name.clone(), s.bind, s.stype, s.shndx)
            };
            if bind == STB_LOCAL || name.is_empty() || matches!(stype, STT_SECTION | STT_FILE) {
                continue;
            }
            if stype == STT_TLS {
                return Err(Error::Unsupported {
                    what: format!("TLS symbol `{name}` (Milestone 70)"),
                    input: self.objects[oi].name.clone(),
                });
            }
            match shndx {
                Shndx::Undef => {
                    if bind != STB_WEAK && !self.table.contains_key(&name) {
                        undefined.insert(name, ());
                    }
                }
                Shndx::Section(s)
                    if self.objects[oi].sections[s].class == Class::Discard
                        && self.objects[oi].sections[s].flags & SHF_ALLOC != 0 =>
                {
                    // COMDAT loser: its definitions do not take part.
                }
                _ => {
                    let weak = bind == STB_WEAK;
                    let new = Def {
                        obj: oi,
                        sym: si,
                        weak,
                    };
                    match self.table.get(&name).copied() {
                        None => {
                            self.table.insert(name, new);
                        }
                        Some(old) if old.weak && !weak => {
                            self.table.insert(name, new);
                        }
                        Some(old) if !old.weak && !weak => {
                            return Err(Error::Duplicate {
                                symbol: name,
                                first: self.objects[old.obj].name.clone(),
                                second: self.objects[oi].name.clone(),
                            });
                        }
                        Some(_) => {}
                    }
                }
            }
        }
        Ok(())
    }

    /// Resolve a relocation's symbol to its defining `(object, symbol)`, or
    /// `None` for an undefined symbol (weak, or an error caught earlier).
    fn resolve_sym(&self, oi: usize, si: usize) -> Option<(usize, usize)> {
        let s = &self.objects[oi].symbols[si];
        if s.shndx != Shndx::Undef {
            return Some((oi, si));
        }
        if s.bind == STB_LOCAL {
            return None;
        }
        self.table.get(&s.name).map(|d| (d.obj, d.sym))
    }

    fn lookup_global(&self, name: &str) -> Option<u64> {
        if let Some(d) = self.table.get(name) {
            return self.sym_addr(d.obj, d.sym).ok();
        }
        self.linker_defined.get(name).copied()
    }

    // ---------------- liveness ----------------

    fn mark_live(&mut self, entry: &str, gc: bool) -> Result<()> {
        if !gc {
            for obj in self.objects.iter_mut().filter(|o| o.loaded) {
                for s in obj.sections.iter_mut() {
                    if s.class != Class::Discard {
                        s.live = true;
                    }
                }
            }
            return Ok(());
        }
        let mut stack: Vec<(usize, usize)> = Vec::new();
        if let Some(d) = self.table.get(entry).copied() {
            if let Shndx::Section(s) = self.objects[d.obj].symbols[d.sym].shndx {
                stack.push((d.obj, s));
            }
        }
        for (oi, obj) in self.objects.iter().enumerate() {
            if !obj.loaded {
                continue;
            }
            for (si, s) in obj.sections.iter().enumerate() {
                if s.class != Class::Discard && s.flags & SHF_GNU_RETAIN != 0 {
                    stack.push((oi, si));
                }
            }
        }
        while let Some((oi, si)) = stack.pop() {
            let sec = &mut self.objects[oi].sections[si];
            if sec.live || sec.class == Class::Discard {
                continue;
            }
            sec.live = true;
            let nrel = sec.relocs.len();
            for ri in 0..nrel {
                let sym = self.objects[oi].sections[si].relocs[ri].sym;
                if let Some((doi, dsi)) = self.resolve_sym(oi, sym) {
                    if let Shndx::Section(target) = self.objects[doi].symbols[dsi].shndx {
                        stack.push((doi, target));
                    }
                }
            }
        }
        Ok(())
    }

    fn check_undefined(&mut self) -> Result<()> {
        // Linker-provided symbols exist only if nothing else defines them;
        // their values are filled in after layout.
        for name in [
            "__executable_start",
            "etext",
            "_etext",
            "__etext",
            "_edata",
            "edata",
            "__bss_start",
            "_end",
            "end",
            "_GLOBAL_OFFSET_TABLE_",
        ] {
            if !self.table.contains_key(name) {
                self.linker_defined.insert(name.to_string(), 0);
            }
        }
        for (oi, obj) in self.objects.iter().enumerate() {
            if !obj.loaded {
                continue;
            }
            for (si, sec) in obj.sections.iter().enumerate() {
                if !sec.live {
                    continue;
                }
                for r in &sec.relocs {
                    let s = &obj.symbols[r.sym];
                    if s.shndx != Shndx::Undef || s.bind == STB_WEAK {
                        continue;
                    }
                    if s.bind == STB_LOCAL {
                        return Err(Error::Input {
                            input: obj.name.clone(),
                            detail: format!(
                                "section {}: relocation against undefined local symbol",
                                sec.name
                            ),
                        });
                    }
                    if !self.table.contains_key(&s.name)
                        && !self.linker_defined.contains_key(&s.name)
                    {
                        return Err(Error::Undefined {
                            symbol: s.name.clone(),
                            referenced_by: format!(
                                "{}({})",
                                obj.name,
                                self.objects[oi].section_name(si)
                            ),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    // ---------------- GOT ----------------

    fn sym_key(&self, oi: usize, si: usize) -> SymKey {
        match self.resolve_sym(oi, si) {
            Some((o, s)) => SymKey::Def(o, s),
            None => SymKey::Named(self.intern_name(&self.objects[oi].symbols[si].name)),
        }
    }

    fn intern_name(&self, name: &str) -> usize {
        self.got_names
            .iter()
            .position(|n| n == name)
            .unwrap_or(usize::MAX)
    }

    fn collect_got(&mut self) -> Result<()> {
        let mut next = 0u64;
        for oi in 0..self.objects.len() {
            if !self.objects[oi].loaded {
                continue;
            }
            for si in 0..self.objects[oi].sections.len() {
                if !self.objects[oi].sections[si].live {
                    continue;
                }
                for ri in 0..self.objects[oi].sections[si].relocs.len() {
                    let r = self.objects[oi].sections[si].relocs[ri].clone();
                    if !needs_got(r.rtype) {
                        continue;
                    }
                    let name = self.objects[oi].symbols[r.sym].name.clone();
                    if self.resolve_sym(oi, r.sym).is_none() && !self.got_names.contains(&name) {
                        self.got_names.push(name);
                    }
                    let key = self.sym_key(oi, r.sym);
                    if let alloc::collections::btree_map::Entry::Vacant(e) = self.got.entry(key) {
                        e.insert(next);
                        next += 8;
                    }
                }
            }
        }
        Ok(())
    }

    // ---------------- layout ----------------

    fn layout(&mut self) -> Result<()> {
        let base = self.image_base;
        let mut segs = vec![
            Segment {
                vaddr: base,
                filesz: 0,
                memsz: 0,
                buf: Vec::new(),
                flags: PF_R,
            },
            Segment {
                vaddr: 0,
                filesz: 0,
                memsz: 0,
                buf: Vec::new(),
                flags: PF_R | PF_X,
            },
            Segment {
                vaddr: 0,
                filesz: 0,
                memsz: 0,
                buf: Vec::new(),
                flags: PF_R | PF_W,
            },
        ];

        // R: headers, rodata, GOT.
        let mut cur = (EHDR_SIZE + 3 * PHDR_SIZE) as u64;
        self.place_class(Class::Rodata, SEG_R, &mut cur);
        if !self.got.is_empty() {
            cur = align_up(cur, 8);
        }
        let got_off = cur;
        cur += 8 * self.got.len() as u64;
        segs[SEG_R].filesz = cur;
        segs[SEG_R].memsz = cur;
        self.got_addr = base + got_off;

        // RX: text.
        let text_va = align_up(base + segs[SEG_R].memsz, PAGE);
        segs[SEG_RX].vaddr = text_va;
        let mut cur = 0u64;
        self.place_class(Class::Text, SEG_RX, &mut cur);
        segs[SEG_RX].filesz = cur;
        segs[SEG_RX].memsz = cur;

        // RW: data then bss, then COMMON.
        let data_va = align_up(text_va + segs[SEG_RX].memsz, PAGE);
        segs[SEG_RW].vaddr = data_va;
        let mut cur = 0u64;
        self.place_class(Class::Data, SEG_RW, &mut cur);
        segs[SEG_RW].filesz = cur;
        self.place_class(Class::Bss, SEG_RW, &mut cur);
        for obj in self.objects.iter_mut().filter(|o| o.loaded) {
            for s in obj.symbols.iter_mut() {
                if s.shndx == Shndx::Common && s.bind != STB_LOCAL {
                    let align = s.value.max(1);
                    if !align.is_power_of_two() {
                        return Err(Error::Input {
                            input: obj.name.clone(),
                            detail: format!(
                                "COMMON symbol `{}` alignment {align} not a power of two",
                                s.name
                            ),
                        });
                    }
                    cur = align_up(cur, align);
                    s.common_addr = Some(data_va + cur);
                    cur += s.size;
                }
            }
        }
        segs[SEG_RW].memsz = cur;

        let text_end = text_va + segs[SEG_RX].memsz;
        let data_end = data_va + segs[SEG_RW].filesz;
        let bss_end = data_va + segs[SEG_RW].memsz;
        for (name, v) in self.linker_defined.iter_mut() {
            *v = match name.as_str() {
                "__executable_start" => base,
                "etext" | "_etext" | "__etext" => text_end,
                "_edata" | "edata" => data_end,
                "__bss_start" => data_end,
                "_end" | "end" => bss_end,
                "_GLOBAL_OFFSET_TABLE_" => self.got_addr,
                _ => 0,
            };
        }

        // Fill segment buffers with section bytes.
        for seg in segs.iter_mut() {
            seg.buf = vec![0u8; seg.filesz as usize];
        }
        for obj in self.objects.iter().filter(|o| o.loaded) {
            for sec in obj.sections.iter() {
                if let Some(p) = sec.out {
                    if !sec.data.is_empty() {
                        let off = p.offset as usize;
                        segs[p.seg].buf[off..off + sec.data.len()].copy_from_slice(&sec.data);
                    }
                }
            }
        }
        self.segments = segs;
        Ok(())
    }

    fn place_class(&mut self, class: Class, seg: usize, cur: &mut u64) {
        for obj in self.objects.iter_mut().filter(|o| o.loaded) {
            for sec in obj.sections.iter_mut() {
                if sec.live && sec.class == class {
                    *cur = align_up(*cur, sec.align);
                    sec.out = Some(Placement { seg, offset: *cur });
                    *cur += sec.size;
                }
            }
        }
    }

    // ---------------- addresses ----------------

    fn section_addr(&self, oi: usize, si: usize) -> Result<u64> {
        let sec = &self.objects[oi].sections[si];
        match sec.out {
            Some(p) => Ok(self.segments[p.seg].vaddr + p.offset),
            None => Err(Error::Input {
                input: self.objects[oi].name.clone(),
                detail: format!("section {} is referenced but was discarded", sec.name),
            }),
        }
    }

    /// Address of a defined symbol.
    fn sym_addr(&self, oi: usize, si: usize) -> Result<u64> {
        let s = &self.objects[oi].symbols[si];
        match s.shndx {
            Shndx::Abs => Ok(s.value),
            Shndx::Common => s.common_addr.ok_or_else(|| Error::Input {
                input: self.objects[oi].name.clone(),
                detail: format!("COMMON symbol `{}` was not allocated", s.name),
            }),
            Shndx::Section(sec) => Ok(self.section_addr(oi, sec)? + s.value),
            Shndx::Undef => Err(Error::Input {
                input: self.objects[oi].name.clone(),
                detail: format!("symbol `{}` is undefined", s.name),
            }),
        }
    }

    // ---------------- relocations ----------------

    fn apply_relocations(&mut self) -> Result<()> {
        // GOT contents first.
        let got_entries: Vec<(SymKey, u64)> = self.got.iter().map(|(k, v)| (*k, *v)).collect();
        for (key, slot) in got_entries {
            let value = match key {
                SymKey::Def(o, s) => self.sym_addr(o, s)?,
                SymKey::Named(_) => 0,
            };
            let off = (self.got_addr - self.segments[SEG_R].vaddr + slot) as usize;
            write_u64(&mut self.segments[SEG_R].buf, off, value);
        }

        for oi in 0..self.objects.len() {
            if !self.objects[oi].loaded {
                continue;
            }
            for si in 0..self.objects[oi].sections.len() {
                let (live, place, nrel) = {
                    let s = &self.objects[oi].sections[si];
                    (s.live, s.out, s.relocs.len())
                };
                let Some(place) = place else { continue };
                if !live {
                    continue;
                }
                let sec_va = self.segments[place.seg].vaddr + place.offset;
                for ri in 0..nrel {
                    let r = self.objects[oi].sections[si].relocs[ri].clone();
                    if r.rtype == R_X86_64_NONE {
                        continue;
                    }
                    let where_ = format!(
                        "{}({})",
                        self.objects[oi].name, self.objects[oi].sections[si].name
                    );
                    let sym_name = self.objects[oi].symbols[r.sym].name.clone();
                    let target = self.resolve_sym(oi, r.sym);
                    let p = sec_va + r.offset;
                    let a = r.addend as i128;
                    let (s, z) = match target {
                        Some((o, s)) => (self.sym_addr(o, s)?, self.objects[o].symbols[s].size),
                        None => match self.linker_defined.get(&sym_name) {
                            Some(v) => (*v, 0),
                            // Undefined weak: PC-relative forms resolve to the
                            // location itself so the field cannot overflow.
                            None if is_pc_relative(r.rtype) => (p, 0),
                            None => (0, 0),
                        },
                    };
                    let field_len = match field_size(r.rtype) {
                        Some(n) => n,
                        None => {
                            return Err(Error::Unsupported {
                                what: format!("relocation type {} against `{sym_name}`", r.rtype),
                                input: where_,
                            });
                        }
                    };
                    let got_slot = if needs_got(r.rtype) {
                        let key = self.sym_key(oi, r.sym);
                        Some(*self.got.get(&key).ok_or_else(|| Error::Input {
                            input: where_.clone(),
                            detail: format!("no GOT slot for `{sym_name}`"),
                        })?)
                    } else {
                        None
                    };
                    let got_addr = self.got_addr;
                    let field_off = (place.offset + r.offset) as usize;
                    let buf = &mut self.segments[place.seg].buf;
                    if field_off + field_len > buf.len() {
                        return Err(Error::Input {
                            input: where_,
                            detail: format!("relocation at {:#x} runs past the section", r.offset),
                        });
                    }
                    let overflow = Error::Overflow {
                        rtype: r.rtype,
                        symbol: sym_name.clone(),
                        input: where_.clone(),
                    };
                    let s128 = s as i128;
                    let p128 = p as i128;
                    let value: i128 = match r.rtype {
                        R_X86_64_64 | R_X86_64_32 | R_X86_64_32S => s128 + a,
                        R_X86_64_PC64 | R_X86_64_PC32 | R_X86_64_PLT32 => s128 + a - p128,
                        R_X86_64_GOTPCREL
                        | R_X86_64_GOTPCRELX
                        | R_X86_64_REX_GOTPCRELX
                        | R_X86_64_CODE_4_GOTPCRELX => {
                            (got_addr + got_slot.unwrap_or(0)) as i128 + a - p128
                        }
                        R_X86_64_GOTPC32 => got_addr as i128 + a - p128,
                        R_X86_64_SIZE32 | R_X86_64_SIZE64 => z as i128 + a,
                        _ => unreachable!("field_size filtered the type"),
                    };
                    match r.rtype {
                        R_X86_64_64 | R_X86_64_PC64 | R_X86_64_SIZE64 => {
                            write_u64(buf, field_off, value as u64);
                        }
                        R_X86_64_32 | R_X86_64_SIZE32 => {
                            if value < 0 || value > u32::MAX as i128 {
                                return Err(overflow);
                            }
                            write_u32(buf, field_off, value as u32);
                        }
                        _ => {
                            if value < i32::MIN as i128 || value > i32::MAX as i128 {
                                return Err(overflow);
                            }
                            write_u32(buf, field_off, value as i32 as u32);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    // ---------------- emit ----------------

    fn emit(&self, entry: u64) -> Vec<u8> {
        let base = self.image_base;
        let loads: Vec<&Segment> = self.segments.iter().filter(|s| s.memsz > 0).collect();
        let file_len = loads
            .iter()
            .map(|s| (s.vaddr - base + s.filesz) as usize)
            .max()
            .unwrap_or(EHDR_SIZE + 3 * PHDR_SIZE);
        let mut out = vec![0u8; file_len];
        for seg in &loads {
            let off = (seg.vaddr - base) as usize;
            out[off..off + seg.buf.len()].copy_from_slice(&seg.buf);
        }
        // Header after the R segment copy: the R buffer's first bytes are
        // the reserved header area and are zero.
        out[0..4].copy_from_slice(b"\x7fELF");
        out[4] = 2;
        out[5] = 1;
        out[6] = 1;
        out[7] = 0;
        write_u16(&mut out, 16, ET_EXEC);
        write_u16(&mut out, 18, EM_X86_64);
        write_u32(&mut out, 20, 1);
        write_u64(&mut out, 24, entry);
        write_u64(&mut out, 32, EHDR_SIZE as u64);
        write_u64(&mut out, 40, 0);
        write_u32(&mut out, 48, 0);
        write_u16(&mut out, 52, EHDR_SIZE as u16);
        write_u16(&mut out, 54, PHDR_SIZE as u16);
        write_u16(&mut out, 56, loads.len() as u16);
        write_u16(&mut out, 58, 0);
        write_u16(&mut out, 60, 0);
        write_u16(&mut out, 62, 0);
        for (i, seg) in loads.iter().enumerate() {
            let at = EHDR_SIZE + i * PHDR_SIZE;
            write_u32(&mut out, at, PT_LOAD);
            write_u32(&mut out, at + 4, seg.flags);
            // A bss-only segment has no bytes in the file; keep its offset
            // inside the file so loaders that bounds-check are happy.
            let offset = (seg.vaddr - base).min(file_len as u64);
            write_u64(&mut out, at + 8, offset);
            write_u64(&mut out, at + 16, seg.vaddr);
            write_u64(&mut out, at + 24, seg.vaddr);
            write_u64(&mut out, at + 32, seg.filesz);
            write_u64(&mut out, at + 40, seg.memsz);
            write_u64(&mut out, at + 48, PAGE);
        }
        out
    }
}

fn needs_got(rtype: u32) -> bool {
    matches!(
        rtype,
        R_X86_64_GOTPCREL | R_X86_64_GOTPCRELX | R_X86_64_REX_GOTPCRELX | R_X86_64_CODE_4_GOTPCRELX
    )
}

fn is_pc_relative(rtype: u32) -> bool {
    matches!(rtype, R_X86_64_PC32 | R_X86_64_PLT32 | R_X86_64_PC64)
}

/// Width of the relocated field, or `None` for a type outside v0.
fn field_size(rtype: u32) -> Option<usize> {
    match rtype {
        R_X86_64_64 | R_X86_64_PC64 | R_X86_64_SIZE64 => Some(8),
        R_X86_64_PC32
        | R_X86_64_PLT32
        | R_X86_64_32
        | R_X86_64_32S
        | R_X86_64_GOTPCREL
        | R_X86_64_GOTPCRELX
        | R_X86_64_REX_GOTPCRELX
        | R_X86_64_CODE_4_GOTPCRELX
        | R_X86_64_GOTPC32
        | R_X86_64_SIZE32 => Some(4),
        _ => None,
    }
}
