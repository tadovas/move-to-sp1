//! A small static linker: RISC-V ELF64 relocatable objects -> ELF64 executable for SP1.
//!
//! Scope is deliberately narrow — exactly what LLVM emits for `riscv64` with
//! `-relax`, medany code model, no PIC, no TLS:
//!   * sections are grouped into three PT_LOAD segments: R+X (text), R (rodata), R+W (data+bss)
//!   * global symbols are resolved across all input objects; `_end` is linker-defined
//!   * relocations: 32/64, BRANCH, JAL, CALL(_PLT), PCREL_HI20/LO12_{I,S}, HI20/LO12_{I,S},
//!     ADD/SUB 8..64; RELAX/ALIGN are ignored (relaxation is disabled at codegen time)
//!
//! SP1 loader constraints honoured: ET_EXEC + EM_RISCV, every segment at or above
//! STACK_TOP (0x7800_0000), exactly one executable segment, no W+X segment.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, anyhow, bail, ensure};
use object::{
    Object, ObjectSection, ObjectSymbol, RelocationFlags, RelocationTarget, SectionIndex,
    SectionKind, SymbolIndex, SymbolSection, elf,
};

/// Base address of the image. Must be >= SP1's STACK_TOP.
pub const BASE: u64 = 0x7800_0000;
const PAGE: u64 = 0x1000;

pub struct InputObject<'a> {
    pub name: String,
    pub data: &'a [u8],
}

/// Pick the archive members needed to satisfy undefined symbols (classic static-archive
/// semantics: a member is pulled in only if it defines a currently-undefined symbol;
/// repeat until nothing changes). Returns the selected members as extra objects.
pub fn select_archive_members<'a>(
    objects: &[InputObject],
    archives: &[(&str, &'a [u8])],
    roots: &[&str],
) -> Result<Vec<InputObject<'a>>> {
    struct Member<'a> {
        name: String,
        data: &'a [u8],
        defines: HashSet<String>,
        undefines: HashSet<String>,
    }
    fn symbols_of(data: &[u8]) -> Result<(HashSet<String>, HashSet<String>)> {
        let f = object::File::parse(data)?;
        let (mut def, mut undef) = (HashSet::new(), HashSet::new());
        for s in f.symbols() {
            if s.is_local() {
                continue;
            }
            let n = s.name()?.to_string();
            if s.is_undefined() {
                undef.insert(n);
            } else {
                def.insert(n);
            }
        }
        Ok((def, undef))
    }

    let mut defined: HashSet<String> = HashSet::new();
    let mut undefined: HashSet<String> = roots.iter().map(|s| s.to_string()).collect();
    for o in objects {
        let (d, u) = symbols_of(o.data).with_context(|| format!("parsing {}", o.name))?;
        defined.extend(d);
        undefined.extend(u);
    }
    undefined.retain(|s| !defined.contains(s));

    let mut members = vec![];
    for (aname, adata) in archives {
        let ar = object::read::archive::ArchiveFile::parse(*adata)
            .with_context(|| format!("parsing archive {aname}"))?;
        for m in ar.members() {
            let m = m?;
            let data = m.data(*adata)?;
            let name = format!("{aname}({})", String::from_utf8_lossy(m.name()));
            if object::FileKind::parse(data).is_err() {
                continue; // symbol index, lib.rmeta, ...
            }
            let Ok((defines, undefines)) = symbols_of(data) else {
                continue;
            };
            members.push(Member {
                name,
                data,
                defines,
                undefines,
            });
        }
    }

    let mut picked = vec![false; members.len()];
    loop {
        let mut changed = false;
        for (i, m) in members.iter().enumerate() {
            if picked[i] || !m.defines.iter().any(|d| undefined.contains(d)) {
                continue;
            }
            picked[i] = true;
            changed = true;
            defined.extend(m.defines.iter().cloned());
            undefined.extend(
                m.undefines
                    .iter()
                    .filter(|u| !defined.contains(*u))
                    .cloned(),
            );
            undefined.retain(|s| !defined.contains(s));
        }
        if !changed {
            break;
        }
    }
    Ok(members
        .into_iter()
        .zip(picked)
        .filter(|(_, p)| *p)
        .map(|(m, _)| InputObject {
            name: m.name,
            data: m.data,
        })
        .collect())
}

/// Extra non-loadable section to embed (e.g. the program ABI description).
pub struct NoteSection {
    pub name: String,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Seg {
    Text,
    Ro,
    Rw,
}

struct Placed {
    obj: usize,
    index: SectionIndex,
    seg: Seg,
    addr: u64,
    size: u64,
    /// None for NOBITS (bss).
    bytes: Option<Vec<u8>>,
}

pub fn link(inputs: &[InputObject], notes: &[NoteSection], entry_symbol: &str) -> Result<Vec<u8>> {
    let files: Vec<object::File> = inputs
        .iter()
        .map(|i| object::File::parse(i.data).with_context(|| format!("parsing {}", i.name)))
        .collect::<Result<_>>()?;
    for (i, f) in files.iter().enumerate() {
        ensure!(
            f.architecture() == object::Architecture::Riscv64,
            "{} is not a riscv64 object",
            inputs[i].name
        );
    }

    // ---- 1. collect and classify allocatable sections
    let mut placed: Vec<Placed> = vec![];
    for (oi, f) in files.iter().enumerate() {
        for s in f.sections() {
            let alloc = match s.flags() {
                object::SectionFlags::Elf { sh_flags } => sh_flags & elf::SHF_ALLOC as u64 != 0,
                _ => false,
            };
            if !alloc || s.size() == 0 {
                continue;
            }
            let name = s.name().unwrap_or("");
            if name.starts_with(".eh_frame") {
                continue;
            }
            let seg = match s.kind() {
                SectionKind::Text => Seg::Text,
                SectionKind::ReadOnlyData
                | SectionKind::ReadOnlyString
                | SectionKind::ReadOnlyDataWithRel => Seg::Ro,
                SectionKind::Data | SectionKind::UninitializedData => Seg::Rw,
                SectionKind::Tls | SectionKind::UninitializedTls => {
                    bail!("{}: TLS section {name} not supported", inputs[oi].name)
                }
                _ => continue,
            };
            let bytes = if s.kind() == SectionKind::UninitializedData {
                None
            } else {
                Some(s.data()?.to_vec())
            };
            placed.push(Placed {
                obj: oi,
                index: s.index(),
                seg,
                addr: 0,
                size: s.size(),
                bytes,
            });
        }
    }

    // ---- 2. layout: text, rodata, data, then bss at the end of the RW segment
    let align_of = |p: &Placed| -> u64 {
        files[p.obj]
            .section_by_index(p.index)
            .map(|s| s.align().max(1))
            .unwrap_or(1)
    };
    let mut cursor = BASE;
    let mut seg_ranges: Vec<(Seg, u64, u64, u64)> = vec![]; // (seg, start, file_end, mem_end)
    for seg in [Seg::Text, Seg::Ro, Seg::Rw] {
        cursor = align_up(cursor, PAGE);
        let start = cursor;
        // progbits first, nobits last so the segment has a contiguous file image
        for nobits in [false, true] {
            for p in placed
                .iter_mut()
                .filter(|p| p.seg == seg && p.bytes.is_none() == nobits)
            {
                let a = files[p.obj]
                    .section_by_index(p.index)
                    .map(|s| s.align().max(1))
                    .unwrap_or(1);
                cursor = align_up(cursor, a.max(if seg == Seg::Text { 4 } else { 1 }));
                p.addr = cursor;
                cursor += p.size;
            }
        }
        let _ = align_of;
        let file_end = placed
            .iter()
            .filter(|p| p.seg == seg && p.bytes.is_some())
            .map(|p| p.addr + p.size)
            .max()
            .unwrap_or(start);
        if cursor > start {
            seg_ranges.push((seg, start, file_end, cursor));
        }
    }
    let end_sym = align_up(cursor, 16);

    let section_addr: HashMap<(usize, SectionIndex), usize> = placed
        .iter()
        .enumerate()
        .map(|(i, p)| ((p.obj, p.index), i))
        .collect();

    // ---- 3. symbols
    let mut globals: HashMap<String, u64> = HashMap::new();
    globals.insert("_end".into(), end_sym);
    // Conventional RISC-V gp value (data start + 0x800). sp1-zkvm's `_start` loads it; with
    // relaxation disabled nothing actually addresses through gp.
    let data_start = seg_ranges
        .iter()
        .find(|r| r.0 == Seg::Rw)
        .map(|r| r.1)
        .unwrap_or(end_sym);
    globals.insert("__global_pointer$".into(), data_start + 0x800);
    let mut weak: HashMap<String, u64> = HashMap::new();
    for (oi, f) in files.iter().enumerate() {
        for sym in f.symbols() {
            if sym.is_local() || sym.is_undefined() {
                continue;
            }
            let Some(addr) = defined_addr(oi, &sym, &placed, &section_addr) else {
                continue;
            };
            let name = sym.name()?.to_string();
            if sym.is_weak() {
                weak.entry(name).or_insert(addr);
            } else if globals.insert(name.clone(), addr).is_some() {
                bail!("duplicate symbol `{name}` (in {})", inputs[oi].name);
            }
        }
    }
    for (k, v) in weak {
        globals.entry(k).or_insert(v);
    }

    let resolve = |oi: usize, idx: SymbolIndex| -> Result<u64> {
        let sym = files[oi].symbol_by_index(idx)?;
        if sym.is_undefined() || !sym.is_local() {
            let name = sym.name()?;
            if let Some(a) = globals.get(name) {
                return Ok(*a);
            }
            if sym.is_undefined() {
                bail!(
                    "undefined symbol `{name}` (referenced from {})",
                    inputs[oi].name
                );
            }
        }
        defined_addr(oi, &sym, &placed, &section_addr).ok_or_else(|| {
            anyhow!(
                "symbol `{}` in {} lives in a discarded section",
                sym.name().unwrap_or("?"),
                inputs[oi].name
            )
        })
    };

    // ---- 4. relocations
    struct Fixup {
        place: usize,
        offset: u64,
        r_type: u32,
        value_s: u64,
        addend: i64,
    }
    let mut fixups = vec![];
    let mut errors = vec![];
    for (pi, p) in placed.iter().enumerate() {
        let sec = files[p.obj].section_by_index(p.index)?;
        for (offset, rel) in sec.relocations() {
            let RelocationFlags::Elf { r_type } = rel.flags() else {
                bail!("non-ELF relocation")
            };
            if r_type == elf::R_RISCV_RELAX || r_type == elf::R_RISCV_ALIGN {
                continue;
            }
            let s = match rel.target() {
                RelocationTarget::Symbol(idx) => match resolve(p.obj, idx) {
                    Ok(v) => v,
                    Err(e) => {
                        errors.push(e.to_string());
                        continue;
                    }
                },
                RelocationTarget::Section(idx) => match section_addr.get(&(p.obj, idx)) {
                    Some(i) => placed[*i].addr,
                    None => bail!("relocation against discarded section"),
                },
                _ => bail!("unsupported relocation target"),
            };
            fixups.push(Fixup {
                place: pi,
                offset,
                r_type,
                value_s: s,
                addend: rel.addend(),
            });
        }
    }
    if !errors.is_empty() {
        errors.sort();
        errors.dedup();
        bail!("link failed:\n  {}", errors.join("\n  "));
    }

    // PCREL_LO12 relocations point at the AUIPC carrying the matching PCREL_HI20; record
    // the full pc-relative value computed at each HI20 site first.
    let mut hi20_at: HashMap<u64, i64> = HashMap::new();
    for f in &fixups {
        if f.r_type == elf::R_RISCV_PCREL_HI20 {
            let pc = placed[f.place].addr + f.offset;
            hi20_at.insert(pc, (f.value_s as i64 + f.addend).wrapping_sub(pc as i64));
        }
    }

    for f in &fixups {
        let pl = &mut placed[f.place];
        let pc = pl.addr + f.offset;
        let sa = (f.value_s as i64).wrapping_add(f.addend);
        let rel = sa.wrapping_sub(pc as i64);
        let bytes = pl
            .bytes
            .as_mut()
            .ok_or_else(|| anyhow!("relocation in NOBITS section"))?;
        let o = f.offset as usize;
        match f.r_type {
            elf::R_RISCV_64 => put64(bytes, o, sa as u64),
            elf::R_RISCV_32 => put32(bytes, o, sa as u32),
            elf::R_RISCV_ADD8 => bytes[o] = bytes[o].wrapping_add(sa as u8),
            elf::R_RISCV_SUB8 => bytes[o] = bytes[o].wrapping_sub(sa as u8),
            elf::R_RISCV_ADD16 => {
                let v = get16(bytes, o).wrapping_add(sa as u16);
                put16(bytes, o, v)
            }
            elf::R_RISCV_SUB16 => {
                let v = get16(bytes, o).wrapping_sub(sa as u16);
                put16(bytes, o, v)
            }
            elf::R_RISCV_ADD32 => {
                let v = get32(bytes, o).wrapping_add(sa as u32);
                put32(bytes, o, v)
            }
            elf::R_RISCV_SUB32 => {
                let v = get32(bytes, o).wrapping_sub(sa as u32);
                put32(bytes, o, v)
            }
            elf::R_RISCV_ADD64 => {
                let v = get64(bytes, o).wrapping_add(sa as u64);
                put64(bytes, o, v)
            }
            elf::R_RISCV_SUB64 => {
                let v = get64(bytes, o).wrapping_sub(sa as u64);
                put64(bytes, o, v)
            }
            elf::R_RISCV_32_PCREL => put32(bytes, o, rel as u32),
            elf::R_RISCV_BRANCH => {
                ensure!((-4096..4096).contains(&rel), "branch out of range");
                {
                    let v = patch_b(get32(bytes, o), rel);
                    put32(bytes, o, v)
                }
            }
            elf::R_RISCV_JAL => {
                ensure!((-(1 << 20)..(1 << 20)).contains(&rel), "jal out of range");
                {
                    let v = patch_j(get32(bytes, o), rel);
                    put32(bytes, o, v)
                }
            }
            elf::R_RISCV_CALL | elf::R_RISCV_CALL_PLT => {
                check_i32(rel)?;
                {
                    let v = patch_u(get32(bytes, o), rel);
                    put32(bytes, o, v)
                }
                {
                    let v = patch_i(get32(bytes, o + 4), rel);
                    put32(bytes, o + 4, v)
                }
            }
            elf::R_RISCV_PCREL_HI20 => {
                check_i32(rel)?;
                {
                    let v = patch_u(get32(bytes, o), rel);
                    put32(bytes, o, v)
                }
            }
            elf::R_RISCV_PCREL_LO12_I | elf::R_RISCV_PCREL_LO12_S => {
                let v = *hi20_at
                    .get(&(f.value_s as u64))
                    .ok_or_else(|| anyhow!("PCREL_LO12 without matching PCREL_HI20"))?;
                let insn = get32(bytes, o);
                let insn = if f.r_type == elf::R_RISCV_PCREL_LO12_I {
                    patch_i(insn, v)
                } else {
                    patch_s(insn, v)
                };
                put32(bytes, o, insn);
            }
            elf::R_RISCV_HI20 => {
                check_i32(sa)?;
                {
                    let v = patch_u(get32(bytes, o), sa);
                    put32(bytes, o, v)
                }
            }
            elf::R_RISCV_LO12_I => {
                let v = patch_i(get32(bytes, o), sa);
                put32(bytes, o, v)
            }
            elf::R_RISCV_LO12_S => {
                let v = patch_s(get32(bytes, o), sa);
                put32(bytes, o, v)
            }
            other => bail!("unsupported RISC-V relocation type {other}"),
        }
    }

    let entry = *globals
        .get(entry_symbol)
        .ok_or_else(|| anyhow!("entry symbol `{entry_symbol}` not defined"))?;

    // ---- 5. write the executable
    let symbols: Vec<(String, u64)> = {
        let mut v: Vec<_> = globals.into_iter().collect();
        v.sort_by_key(|(_, a)| *a);
        v
    };
    Ok(write_elf(&placed, &seg_ranges, entry, notes, &symbols))
}

fn defined_addr(
    oi: usize,
    sym: &object::Symbol,
    placed: &[Placed],
    section_addr: &HashMap<(usize, SectionIndex), usize>,
) -> Option<u64> {
    match sym.section() {
        SymbolSection::Section(idx) => section_addr
            .get(&(oi, idx))
            .map(|i| placed[*i].addr + sym.address()),
        SymbolSection::Absolute => Some(sym.address()),
        _ => None,
    }
}

fn check_i32(v: i64) -> Result<()> {
    ensure!(
        (i32::MIN as i64..=i32::MAX as i64).contains(&(v + 0x800)),
        "pc-relative offset out of range"
    );
    Ok(())
}

fn align_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

// ---- instruction encoders

fn patch_u(insn: u32, v: i64) -> u32 {
    let hi = ((v + 0x800) >> 12) as u32 & 0xfffff;
    (insn & 0xfff) | (hi << 12)
}
fn patch_i(insn: u32, v: i64) -> u32 {
    let lo = (v as u32) & 0xfff;
    (insn & 0x000f_ffff) | (lo << 20)
}
fn patch_s(insn: u32, v: i64) -> u32 {
    let imm = (v as u32) & 0xfff;
    (insn & 0x01ff_f07f) | ((imm >> 5) << 25) | ((imm & 0x1f) << 7)
}
fn patch_b(insn: u32, v: i64) -> u32 {
    let imm = v as u32;
    let b12 = (imm >> 12) & 1;
    let b11 = (imm >> 11) & 1;
    let b10_5 = (imm >> 5) & 0x3f;
    let b4_1 = (imm >> 1) & 0xf;
    (insn & 0x01ff_f07f) | (b12 << 31) | (b10_5 << 25) | (b4_1 << 8) | (b11 << 7)
}
fn patch_j(insn: u32, v: i64) -> u32 {
    let imm = v as u32;
    let b20 = (imm >> 20) & 1;
    let b19_12 = (imm >> 12) & 0xff;
    let b11 = (imm >> 11) & 1;
    let b10_1 = (imm >> 1) & 0x3ff;
    (insn & 0xfff) | (b20 << 31) | (b10_1 << 21) | (b11 << 20) | (b19_12 << 12)
}

fn get16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
fn get32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn get64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes())
}
fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes())
}
fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes())
}

// ---- ELF writer

fn write_elf(
    placed: &[Placed],
    segs: &[(Seg, u64, u64, u64)],
    entry: u64,
    notes: &[NoteSection],
    symbols: &[(String, u64)],
) -> Vec<u8> {
    const EHDR: usize = 64;
    const PHDR: usize = 56;
    const SHDR: usize = 64;

    let mut out = vec![0u8; EHDR + PHDR * segs.len()];
    let mut phdrs = vec![];
    // (name, type, flags, addr, offset, size, link, info, align, entsize)
    let mut shdrs: Vec<(String, u32, u64, u64, u64, u64, u32, u32, u64, u64)> = vec![];

    for (seg, start, file_end, mem_end) in segs {
        // keep file offset congruent to vaddr modulo page size
        let off = align_up(out.len() as u64, PAGE);
        out.resize(off as usize, 0);
        let filesz = file_end - start;
        out.resize((off + filesz) as usize, 0);
        for p in placed.iter().filter(|p| p.seg == *seg) {
            if let Some(b) = &p.bytes {
                let at = (off + (p.addr - start)) as usize;
                out[at..at + b.len()].copy_from_slice(b);
            }
        }
        let flags = match seg {
            Seg::Text => elf::PF_R | elf::PF_X,
            Seg::Ro => elf::PF_R,
            Seg::Rw => elf::PF_R | elf::PF_W,
        };
        phdrs.push((flags, off, *start, filesz, mem_end - start));
        let (name, sflags) = match seg {
            Seg::Text => (".text", elf::SHF_ALLOC | elf::SHF_EXECINSTR),
            Seg::Ro => (".rodata", elf::SHF_ALLOC),
            Seg::Rw => (".data", elf::SHF_ALLOC | elf::SHF_WRITE),
        };
        if filesz > 0 {
            shdrs.push((
                name.into(),
                elf::SHT_PROGBITS,
                sflags as u64,
                *start,
                off,
                filesz,
                0,
                0,
                8,
                0,
            ));
        }
        if mem_end > file_end {
            shdrs.push((
                ".bss".into(),
                elf::SHT_NOBITS,
                (elf::SHF_ALLOC | elf::SHF_WRITE) as u64,
                *file_end,
                off + filesz,
                mem_end - file_end,
                0,
                0,
                8,
                0,
            ));
        }
    }

    for n in notes {
        let off = out.len() as u64;
        out.extend_from_slice(&n.data);
        shdrs.push((
            n.name.clone(),
            elf::SHT_PROGBITS,
            0,
            0,
            off,
            n.data.len() as u64,
            0,
            0,
            1,
            0,
        ));
    }

    // symbol table (for disassemblers / SP1 profiling), all symbols global
    let mut strtab = vec![0u8];
    let mut symtab = vec![0u8; 24];
    for (name, addr) in symbols {
        let name_off = strtab.len() as u32;
        strtab.extend_from_slice(name.as_bytes());
        strtab.push(0);
        let mut e = [0u8; 24];
        e[0..4].copy_from_slice(&name_off.to_le_bytes());
        e[4] = (elf::STB_GLOBAL << 4) | elf::STT_NOTYPE;
        e[6..8].copy_from_slice(&(elf::SHN_ABS).to_le_bytes());
        e[8..16].copy_from_slice(&addr.to_le_bytes());
        symtab.extend_from_slice(&e);
    }
    let symtab_off = align_up(out.len() as u64, 8);
    out.resize(symtab_off as usize, 0);
    out.extend_from_slice(&symtab);
    let strtab_off = out.len() as u64;
    out.extend_from_slice(&strtab);
    let symtab_index = shdrs.len() as u32 + 1;
    shdrs.push((
        ".symtab".into(),
        elf::SHT_SYMTAB,
        0,
        0,
        symtab_off,
        symtab.len() as u64,
        symtab_index + 1,
        1,
        8,
        24,
    ));
    shdrs.push((
        ".strtab".into(),
        elf::SHT_STRTAB,
        0,
        0,
        strtab_off,
        strtab.len() as u64,
        0,
        0,
        1,
        0,
    ));

    // section name string table
    let mut shstr = vec![0u8];
    let mut name_offs = vec![];
    for s in shdrs.iter().map(|s| s.0.as_str()).chain([".shstrtab"]) {
        name_offs.push(shstr.len() as u32);
        shstr.extend_from_slice(s.as_bytes());
        shstr.push(0);
    }
    let shstr_off = out.len() as u64;
    out.extend_from_slice(&shstr);
    let shstrndx = shdrs.len() as u16 + 1;
    shdrs.push((
        ".shstrtab".into(),
        elf::SHT_STRTAB,
        0,
        0,
        shstr_off,
        shstr.len() as u64,
        0,
        0,
        1,
        0,
    ));

    let shoff = align_up(out.len() as u64, 8);
    out.resize(shoff as usize, 0);
    out.extend_from_slice(&[0u8; SHDR]); // null section
    for (i, (_, ty, flags, addr, off, size, link, info, align, entsize)) in shdrs.iter().enumerate()
    {
        let mut h = [0u8; SHDR];
        h[0..4].copy_from_slice(&name_offs[i].to_le_bytes());
        h[4..8].copy_from_slice(&ty.to_le_bytes());
        h[8..16].copy_from_slice(&flags.to_le_bytes());
        h[16..24].copy_from_slice(&addr.to_le_bytes());
        h[24..32].copy_from_slice(&off.to_le_bytes());
        h[32..40].copy_from_slice(&size.to_le_bytes());
        h[40..44].copy_from_slice(&link.to_le_bytes());
        h[44..48].copy_from_slice(&info.to_le_bytes());
        h[48..56].copy_from_slice(&align.to_le_bytes());
        h[56..64].copy_from_slice(&entsize.to_le_bytes());
        out.extend_from_slice(&h);
    }

    // ELF header
    let mut eh = [0u8; EHDR];
    eh[0..4].copy_from_slice(&elf::ELFMAG);
    eh[4] = elf::ELFCLASS64;
    eh[5] = elf::ELFDATA2LSB;
    eh[6] = elf::EV_CURRENT;
    eh[16..18].copy_from_slice(&elf::ET_EXEC.to_le_bytes());
    eh[18..20].copy_from_slice(&elf::EM_RISCV.to_le_bytes());
    eh[20..24].copy_from_slice(&(elf::EV_CURRENT as u32).to_le_bytes());
    eh[24..32].copy_from_slice(&entry.to_le_bytes());
    eh[32..40].copy_from_slice(&(EHDR as u64).to_le_bytes());
    eh[40..48].copy_from_slice(&shoff.to_le_bytes());
    eh[52..54].copy_from_slice(&(EHDR as u16).to_le_bytes());
    eh[54..56].copy_from_slice(&(PHDR as u16).to_le_bytes());
    eh[56..58].copy_from_slice(&(segs.len() as u16).to_le_bytes());
    eh[58..60].copy_from_slice(&(SHDR as u16).to_le_bytes());
    eh[60..62].copy_from_slice(&(shdrs.len() as u16 + 1).to_le_bytes());
    eh[62..64].copy_from_slice(&shstrndx.to_le_bytes());
    out[..EHDR].copy_from_slice(&eh);

    for (i, (flags, off, vaddr, filesz, memsz)) in phdrs.iter().enumerate() {
        let mut ph = [0u8; PHDR];
        ph[0..4].copy_from_slice(&elf::PT_LOAD.to_le_bytes());
        ph[4..8].copy_from_slice(&flags.to_le_bytes());
        ph[8..16].copy_from_slice(&off.to_le_bytes());
        ph[16..24].copy_from_slice(&vaddr.to_le_bytes());
        ph[24..32].copy_from_slice(&vaddr.to_le_bytes());
        ph[32..40].copy_from_slice(&filesz.to_le_bytes());
        ph[40..48].copy_from_slice(&memsz.to_le_bytes());
        ph[48..56].copy_from_slice(&PAGE.to_le_bytes());
        let at = EHDR + i * PHDR;
        out[at..at + PHDR].copy_from_slice(&ph);
    }
    out
}
