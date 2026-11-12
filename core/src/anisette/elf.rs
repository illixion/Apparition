//! Loads a shared object built for Linux or Android (ELF64, little endian) into this process
//! without the system's dynamic loader.
//!
//! Written from the ELF gABI (program headers, the dynamic section, the DT_HASH symbol table), the
//! AArch64 ELF ABI and the x86-64 psABI (relocation types). The steps:
//!
//! 1. read the program headers and pick where the image goes (see `placement`);
//! 2. reserve one anonymous region, and copy each PT_LOAD segment to its virtual address;
//! 3. apply the RELA relocations. Symbols the library defines point into the image; the rest
//!    are asked of a resolver the caller supplies, which is how the library gets its "libc";
//! 4. give every page the protection its segments ask for.
//!
//! Not supported, and refused when a library needs it: thread-local storage, IFUNC, packed or
//! REL-style relocations. Initializers (DT_INIT, DT_INIT_ARRAY) aren't run, because what this is
//! used for doesn't need them, and running them would call into libraries that aren't there.

use std::ffi::{CStr, c_void};

use rootcause::prelude::*;

#[cfg(target_arch = "aarch64")]
const HOST_MACHINE: u16 = 183;
#[cfg(target_arch = "x86_64")]
const HOST_MACHINE: u16 = 62;

const ET_DYN: u16 = 3;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_TLS: u32 = 7;
const PF_X: u32 = 1;
const PF_W: u32 = 2;

const DT_NULL: i64 = 0;
const DT_PLTRELSZ: i64 = 2;
const DT_HASH: i64 = 4;
const DT_STRTAB: i64 = 5;
const DT_SYMTAB: i64 = 6;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_RELAENT: i64 = 9;
const DT_SYMENT: i64 = 11;
const DT_REL: i64 = 17;
const DT_JMPREL: i64 = 23;
const DT_RELR: i64 = 36;
const DT_ANDROID_REL: i64 = 0x6000_000f;
const DT_ANDROID_RELA: i64 = 0x6000_0011;

#[cfg(target_arch = "aarch64")]
mod reloc {
    pub const ABSOLUTE: u32 = 257;
    pub const GLOB_DAT: u32 = 1025;
    pub const JUMP_SLOT: u32 = 1026;
    pub const RELATIVE: u32 = 1027;
    /// Whether GLOB_DAT and JUMP_SLOT add the addend (S + A, as opposed to S).
    pub const SLOT_ADDS_ADDEND: bool = true;
}
#[cfg(target_arch = "x86_64")]
mod reloc {
    pub const ABSOLUTE: u32 = 1;
    pub const GLOB_DAT: u32 = 6;
    pub const JUMP_SLOT: u32 = 7;
    pub const RELATIVE: u32 = 8;
    pub const SLOT_ADDS_ADDEND: bool = false;
}

const STT_OBJECT: u8 = 1;
const STB_WEAK: u8 = 2;

/// What the library wants an undefined symbol for.
pub(crate) struct Import<'a> {
    pub name: &'a str,
    /// Data (an `extern` variable) rather than code: it needs an address that can be read.
    pub object: bool,
}

pub(crate) struct Library {
    /// Where virtual address 0 would be: a symbol's address is `bias + st_value`.
    bias: usize,
    symbols: usize,
    strings: usize,
    hash: usize,
    symbol_count: usize,
}

// The image is never unmapped or moved, and nothing in it is touched from Rust after loading
// except reading the symbol table.
unsafe impl Send for Library {}
unsafe impl Sync for Library {}

struct Segment {
    vaddr: u64,
    offset: u64,
    filesz: u64,
    memsz: u64,
    flags: u32,
}

struct Bytes<'a>(&'a [u8]);

impl Bytes<'_> {
    fn get<const N: usize>(&self, at: u64) -> Result<[u8; N], Report> {
        let start = usize::try_from(at).ok();
        let slice = start.and_then(|start| self.0.get(start..start.checked_add(N)?));
        match slice {
            Some(slice) => Ok(slice.try_into().expect("N bytes")),
            None => bail!("the file ends before offset {at:#x}"),
        }
    }
    fn u16(&self, at: u64) -> Result<u16, Report> {
        Ok(u16::from_le_bytes(self.get(at)?))
    }
    fn u32(&self, at: u64) -> Result<u32, Report> {
        Ok(u32::from_le_bytes(self.get(at)?))
    }
    fn u64(&self, at: u64) -> Result<u64, Report> {
        Ok(u64::from_le_bytes(self.get(at)?))
    }
}

fn page_size() -> usize {
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

fn round_up(value: usize, to: usize) -> usize {
    value.div_ceil(to) * to
}

/// How far, in steps of 4 KiB, to move the image from the start of its region so that no page of
/// this machine holds both executable and writable bytes of it.
///
/// A library linked for 4 KiB pages puts its code and its data in neighbouring 4 KiB pages, and
/// where pages are 16 KiB (Apple silicon) one page can then hold the end of the code and the start
/// of the data. A page can't be both writable and executable, so the image is shifted until the
/// boundary between them falls on a page boundary. Segments only need to keep their offset
/// within a 4 KiB page, which a shift of a multiple of 4 KiB keeps.
fn placement(segments: &[Segment], low: u64, page: usize) -> Result<usize, Report> {
    let pages = |shift: usize, segment: &Segment| {
        let start = shift as u64 + (segment.vaddr - low);
        (start / page as u64, (start + segment.memsz - 1) / page as u64)
    };
    for shift in (0..page).step_by(0x1000) {
        let code: Vec<_> = segments.iter().filter(|s| s.memsz > 0 && s.flags & PF_X != 0).map(|s| pages(shift, s)).collect();
        let data: Vec<_> = segments.iter().filter(|s| s.memsz > 0 && s.flags & PF_W != 0).map(|s| pages(shift, s)).collect();
        let shared = code.iter().any(|c| data.iter().any(|d| c.0 <= d.1 && d.0 <= c.1));
        if !shared {
            return Ok(shift);
        }
    }
    bail!("no placement keeps code and data on separate {page}-byte pages")
}

impl Library {
    /// Loads `file` and resolves its imports through `resolve` (`None` leaves a weak symbol null
    /// and fails for any other).
    pub(crate) fn load(file: &[u8], resolve: &dyn Fn(&Import) -> Option<usize>) -> Result<Library, Report> {
        let bytes = Bytes(file);
        if file.get(..4) != Some(b"\x7fELF") || file.get(4) != Some(&2) || file.get(5) != Some(&1) {
            bail!("not a 64-bit little-endian ELF file");
        }
        if bytes.u16(16)? != ET_DYN {
            bail!("not a shared object");
        }
        let machine = bytes.u16(18)?;
        if machine != HOST_MACHINE {
            bail!("built for machine {machine}, this is {HOST_MACHINE}");
        }

        let table = bytes.u64(32)?;
        let entry_size = u64::from(bytes.u16(54)?);
        let count = u64::from(bytes.u16(56)?);
        let mut segments = Vec::new();
        let mut dynamic = None;
        for index in 0..count {
            let at = table + index * entry_size;
            let kind = bytes.u32(at)?;
            let segment = Segment {
                flags: bytes.u32(at + 4)?,
                offset: bytes.u64(at + 8)?,
                vaddr: bytes.u64(at + 16)?,
                filesz: bytes.u64(at + 32)?,
                memsz: bytes.u64(at + 40)?,
            };
            match kind {
                PT_LOAD => {
                    if segment.filesz > segment.memsz {
                        bail!("a segment has more file bytes than memory");
                    }
                    if segment.offset.checked_add(segment.filesz).is_none_or(|end| end > file.len() as u64) {
                        bail!("a segment reaches past the end of the file");
                    }
                    if segment.flags & PF_X != 0 && segment.flags & PF_W != 0 {
                        bail!("a segment is both writable and executable");
                    }
                    segments.push(segment);
                }
                PT_DYNAMIC => dynamic = Some(segment.vaddr),
                PT_TLS => bail!("the library uses thread-local storage"),
                _ => {}
            }
        }
        segments.retain(|s| s.memsz > 0);
        let dynamic = dynamic.ok_or_else(|| report!("no dynamic section"))?;
        let low = segments.iter().map(|s| s.vaddr).min().ok_or_else(|| report!("nothing to load"))? & !0xfff;
        let high = segments.iter().map(|s| s.vaddr + s.memsz).max().expect("segments");

        let page = page_size();
        let shift = placement(&segments, low, page)?;
        let length = round_up(shift + (high - low) as usize, page);
        let region = unsafe {
            libc::mmap(std::ptr::null_mut(), length, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANON, -1, 0)
        };
        if region == libc::MAP_FAILED {
            bail!("couldn't reserve {length} bytes: {}", std::io::Error::last_os_error());
        }
        let bias = (region as usize + shift).wrapping_sub(low as usize);
        for segment in &segments {
            let from = &file[segment.offset as usize..(segment.offset + segment.filesz) as usize];
            unsafe { std::ptr::copy_nonoverlapping(from.as_ptr(), bias.wrapping_add(segment.vaddr as usize) as *mut u8, from.len()) };
        }
        let image = Image { bias, low, high };

        // The dynamic section: tag/value pairs up to DT_NULL.
        let mut tags = std::collections::HashMap::new();
        let mut at = dynamic;
        loop {
            let tag = image.read::<u64>(at)? as i64;
            if tag == DT_NULL {
                break;
            }
            tags.entry(tag).or_insert(image.read::<u64>(at + 8)?);
            at += 16;
        }
        for unsupported in [DT_REL, DT_RELR, DT_ANDROID_REL, DT_ANDROID_RELA] {
            if tags.contains_key(&unsupported) {
                bail!("the library uses a relocation format that isn't supported (tag {unsupported:#x})");
            }
        }
        let need = |tag: i64, what: &str| tags.get(&tag).copied().ok_or_else(|| report!("no {what} in the dynamic section"));
        let symbols = need(DT_SYMTAB, "symbol table")?;
        let strings = need(DT_STRTAB, "string table")?;
        let hash = need(DT_HASH, "symbol hash")?;
        if tags.get(&DT_SYMENT).is_some_and(|&size| size != 24) || tags.get(&DT_RELAENT).is_some_and(|&size| size != 24) {
            bail!("unexpected symbol or relocation entry size");
        }
        let symbol_count = image.read::<u32>(hash + 4)? as usize;
        let library = Library { bias, symbols: bias.wrapping_add(symbols as usize), strings: bias.wrapping_add(strings as usize), hash: bias.wrapping_add(hash as usize), symbol_count };

        for (start, size) in [(DT_RELA, DT_RELASZ), (DT_JMPREL, DT_PLTRELSZ)] {
            if let Some(&start) = tags.get(&start) {
                let size = tags.get(&size).copied().unwrap_or(0);
                for index in 0..size / 24 {
                    library.relocate(&image, start + index * 24, resolve)?;
                }
            }
        }

        library.protect(&segments, region as usize, low, shift, length, page)?;
        Ok(library)
    }

    fn relocate(&self, image: &Image, at: u64, resolve: &dyn Fn(&Import) -> Option<usize>) -> Result<(), Report> {
        let offset = image.read::<u64>(at)?;
        let info = image.read::<u64>(at + 8)?;
        let addend = image.read::<u64>(at + 16)?;
        let (kind, symbol) = (info as u32, (info >> 32) as usize);
        let target = image.address(offset, 8)?;
        let value = match kind {
            reloc::RELATIVE => self.bias.wrapping_add(addend as usize),
            reloc::ABSOLUTE | reloc::GLOB_DAT | reloc::JUMP_SLOT => {
                let found = self.symbol_address(symbol, resolve)?;
                if kind == reloc::ABSOLUTE || reloc::SLOT_ADDS_ADDEND { found.wrapping_add(addend as usize) } else { found }
            }
            other => bail!("unsupported relocation type {other} at {offset:#x}"),
        };
        unsafe { (target as *mut usize).write_unaligned(value) };
        Ok(())
    }

    /// The address a relocation against symbol `index` takes: the library's own definition, or
    /// what the resolver says.
    fn symbol_address(&self, index: usize, resolve: &dyn Fn(&Import) -> Option<usize>) -> Result<usize, Report> {
        if index == 0 || index >= self.symbol_count {
            bail!("relocation against symbol {index}, which doesn't exist");
        }
        let entry = self.symbols + index * 24;
        let (name, info, section, value) = unsafe {
            (
                self.name_at((entry as *const u32).read_unaligned()),
                *((entry + 4) as *const u8),
                ((entry + 6) as *const u16).read_unaligned(),
                ((entry + 8) as *const u64).read_unaligned(),
            )
        };
        if section != 0 {
            return Ok(self.bias.wrapping_add(value as usize));
        }
        let name = name.to_str().map_err(|_| report!("a symbol name isn't UTF-8"))?;
        match resolve(&Import { name, object: info & 0xf == STT_OBJECT }) {
            Some(address) => Ok(address),
            None if info >> 4 == STB_WEAK => Ok(0),
            None => bail!("nothing provides {name}"),
        }
    }

    fn name_at(&self, offset: u32) -> &CStr {
        unsafe { CStr::from_ptr((self.strings + offset as usize) as *const _) }
    }

    /// Gives every page the union of what its segments ask for, and makes sure what was written
    /// as data is visible as code.
    fn protect(&self, segments: &[Segment], region: usize, low: u64, shift: usize, length: usize, page: usize) -> Result<(), Report> {
        let mut wanted = vec![0; length / page];
        for segment in segments {
            let start = shift + (segment.vaddr - low) as usize;
            let end = start + segment.memsz as usize;
            let mut protection = libc::PROT_READ;
            if segment.flags & PF_W != 0 {
                protection |= libc::PROT_WRITE;
            }
            if segment.flags & PF_X != 0 {
                protection |= libc::PROT_EXEC;
                invalidate_instruction_cache((region + start) as *mut c_void, end - start);
            }
            for entry in &mut wanted[start / page..=(end - 1) / page] {
                *entry |= protection;
            }
        }
        let mut index = 0;
        while index < wanted.len() {
            let run = wanted[index..].iter().take_while(|&&p| p == wanted[index]).count();
            if wanted[index] != libc::PROT_READ | libc::PROT_WRITE {
                let result = unsafe { libc::mprotect((region + index * page) as *mut c_void, run * page, wanted[index]) };
                if result != 0 {
                    bail!("mprotect: {}", std::io::Error::last_os_error());
                }
            }
            index += run;
        }
        Ok(())
    }

    /// The address of an exported symbol, from the library's DT_HASH table.
    pub(crate) fn symbol(&self, name: &str) -> Option<usize> {
        let (buckets, chains) = unsafe { (((self.hash) as *const u32).read_unaligned() as usize, self.hash + 8) };
        if buckets == 0 {
            return None;
        }
        let read = |at: usize| unsafe { (at as *const u32).read_unaligned() as usize };
        let mut index = read(chains + (sysv_hash(name.as_bytes()) as usize % buckets) * 4);
        let chain = chains + buckets * 4;
        while index != 0 && index < self.symbol_count {
            let entry = self.symbols + index * 24;
            let (defined, value) = unsafe { (((entry + 6) as *const u16).read_unaligned() != 0, ((entry + 8) as *const u64).read_unaligned()) };
            if defined && self.name_at(read(entry) as u32).to_bytes() == name.as_bytes() {
                return Some(self.bias.wrapping_add(value as usize));
            }
            index = read(chain + index * 4);
        }
        None
    }
}

/// The hash function the ELF gABI defines for DT_HASH.
fn sysv_hash(name: &[u8]) -> u32 {
    let mut hash = 0u32;
    for &byte in name {
        hash = (hash << 4).wrapping_add(u32::from(byte));
        let high = hash & 0xf000_0000;
        if high != 0 {
            hash ^= high >> 24;
        }
        hash &= !high;
    }
    hash
}

/// The loaded image, for reads by virtual address while it's being set up.
struct Image {
    bias: usize,
    low: u64,
    high: u64,
}

impl Image {
    fn address(&self, vaddr: u64, length: u64) -> Result<usize, Report> {
        if vaddr < self.low || vaddr.checked_add(length).is_none_or(|end| end > self.high) {
            bail!("address {vaddr:#x} lies outside the image");
        }
        Ok(self.bias.wrapping_add(vaddr as usize))
    }

    fn read<T: Copy>(&self, vaddr: u64) -> Result<T, Report> {
        let address = self.address(vaddr, size_of::<T>() as u64)?;
        Ok(unsafe { (address as *const T).read_unaligned() })
    }
}

#[cfg(all(target_arch = "aarch64", target_vendor = "apple"))]
fn invalidate_instruction_cache(start: *mut c_void, length: usize) {
    unsafe extern "C" {
        fn sys_icache_invalidate(start: *mut c_void, length: usize);
    }
    unsafe { sys_icache_invalidate(start, length) }
}

/// The Arm ARM's sequence for making written instructions visible to instruction fetch: clean
/// each data cache line to the point of unification, then invalidate each instruction cache
/// line, with barriers between. Written out rather than calling the compiler runtime's
/// `__clear_cache`, which a static musl build doesn't link. Linux lets user space read CTR_EL0
/// and run both maintenance instructions.
#[cfg(all(target_arch = "aarch64", not(target_vendor = "apple")))]
fn invalidate_instruction_cache(start: *mut c_void, length: usize) {
    use std::arch::asm;
    let ctr: u64;
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
    // DminLine (bits 19:16) and IminLine (bits 3:0) are log2 of the smallest line, in words.
    let data_line = 4usize << ((ctr >> 16) & 0xf);
    let instruction_line = 4usize << (ctr & 0xf);
    // IDC (bit 28) and DIC (bit 29) say a step isn't needed.
    let (idc, dic) = (ctr & (1 << 28) != 0, ctr & (1 << 29) != 0);
    let (begin, end) = (start as usize, start as usize + length);
    unsafe {
        if !idc {
            let mut line = begin & !(data_line - 1);
            while line < end {
                asm!("dc cvau, {}", in(reg) line, options(nostack));
                line += data_line;
            }
        }
        asm!("dsb ish", options(nostack));
        if !dic {
            let mut line = begin & !(instruction_line - 1);
            while line < end {
                asm!("ic ivau, {}", in(reg) line, options(nostack));
                line += instruction_line;
            }
            asm!("dsb ish", options(nostack));
        }
        asm!("isb", options(nostack));
    }
}

// x86-64 keeps the instruction cache coherent.
#[cfg(target_arch = "x86_64")]
fn invalidate_instruction_cache(_start: *mut c_void, _length: usize) {}
