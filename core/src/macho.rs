//! Just enough Mach-O editing to inject a tweak: read and rewrite the load commands of each
//! 64-bit slice of an executable or dylib, in the free space between them and the first
//! section. Nothing else in the file moves, so a fat binary's slices keep their offsets and a
//! code signature (replaced by signing afterwards anyway) stays where its command says.
//!
//! Formats from <mach-o/loader.h> and <mach-o/fat.h>: little-endian 64-bit slices
//! (`MH_MAGIC_64`), in a big-endian fat header (`FAT_MAGIC`, `FAT_MAGIC_64`) or alone.
//! 32-bit slices are left as they are: no device Apparition reaches runs them.

use rootcause::prelude::*;

pub const LC_LOAD_DYLIB: u32 = 0xc;
pub const LC_LAZY_LOAD_DYLIB: u32 = 0x20;
pub const LC_LOAD_WEAK_DYLIB: u32 = 0x8000_0018;
pub const LC_REEXPORT_DYLIB: u32 = 0x8000_001f;
pub const LC_LOAD_UPWARD_DYLIB: u32 = 0x8000_0023;
pub const LC_RPATH: u32 = 0x8000_001c;
const LC_SEGMENT_64: u32 = 0x19;
const LC_CODE_SIGNATURE: u32 = 0x1d;
const LC_ENCRYPTION_INFO_64: u32 = 0x2c;
const LC_BUILD_VERSION: u32 = 0x32;
const LC_VERSION_MIN_IPHONEOS: u32 = 0x25;
const LC_VERSION_MIN_TVOS: u32 = 0x2f;

const MH_MAGIC_64: u32 = 0xfeed_facf;
const MH_MAGIC: u32 = 0xfeed_face;
const FAT_MAGIC: u32 = 0xcafe_babe;
const FAT_MAGIC_64: u32 = 0xcafe_babf;
const HEADER_64: usize = 32;
/// What apple-codesign needs to add an `LC_CODE_SIGNATURE` to an unsigned binary.
const SIGNATURE_COMMAND: usize = 16;

#[cfg(test)]
pub const MH_EXECUTE: u32 = 2;
pub const MH_DYLIB: u32 = 6;

/// `LC_BUILD_VERSION` platforms Apparition tells apart.
pub const PLATFORM_IOS: u32 = 2;
pub const PLATFORM_TVOS: u32 = 3;
pub const PLATFORM_VISIONOS: u32 = 11;

/// Whether `data` starts like a Mach-O file (thin or fat).
pub fn is_macho(data: &[u8]) -> bool {
    let Some(head) = data.get(..4) else { return false };
    let le = u32::from_le_bytes(head.try_into().unwrap());
    let be = u32::from_be_bytes(head.try_into().unwrap());
    matches!(le, MH_MAGIC_64 | MH_MAGIC) || matches!(be, FAT_MAGIC | FAT_MAGIC_64)
}

/// Undefined external symbols from each 64-bit slice's nlist_64 table. Inspect
/// the actual imports rather than strings embedded in resources or debug info.
pub fn imported_symbols(data: &[u8]) -> Result<Vec<String>, Report> {
    let mut names = std::collections::BTreeSet::new();
    for (offset, length) in slices(data)? {
        let bytes = &data[offset..offset + length];
        let slice = Slice::read(bytes)?;
        for command in slice.commands.iter().filter(|c| c.cmd == 2) { // LC_SYMTAB
            let field = |at| u32_at(&command.bytes, at).map(|n| n as usize).ok_or_else(|| report!("a truncated symbol table command"));
            let start = field(8)?;
            let count = field(12)?;
            let strings_start = field(16)?;
            let strings_length = field(20)?;
            let table_length = count.checked_mul(16).ok_or_else(|| report!("a damaged symbol table"))?;
            let table = bytes.get(start..start.checked_add(table_length).ok_or_else(|| report!("a damaged symbol table"))?).ok_or_else(|| report!("symbols run past the end of the slice"))?;
            let strings = bytes.get(strings_start..strings_start.checked_add(strings_length).ok_or_else(|| report!("a damaged string table"))?).ok_or_else(|| report!("strings run past the end of the slice"))?;
            for entry in table.chunks_exact(16) {
                // N_STAB == 0, N_TYPE == N_UNDF, N_EXT set. Common
                // definitions have nonzero n_value and are not imports.
                if entry[4] & !0x10 != 1 || u64_at(entry, 8) != Some(0) {
                    continue;
                }
                let index = u32_at(entry, 0).unwrap() as usize;
                let tail = strings.get(index..).ok_or_else(|| report!("a symbol names a missing string"))?;
                let end = tail.iter().position(|&b| b == 0).ok_or_else(|| report!("an unterminated symbol name"))?;
                if let Ok(name) = std::str::from_utf8(&tail[..end]) {
                    names.insert(name.to_owned());
                }
            }
        }
    }
    Ok(names.into_iter().collect())
}

/// Where each 64-bit slice of `data` is: (start, length). Fails when there is none.
pub fn slices(data: &[u8]) -> Result<Vec<(usize, usize)>, Report> {
    let head = data.get(..8).ok_or_else(|| report!("too short to be a Mach-O file"))?;
    let be = u32::from_be_bytes(head[..4].try_into().unwrap());
    let le = u32::from_le_bytes(head[..4].try_into().unwrap());
    let mut found = Vec::new();
    if be == FAT_MAGIC || be == FAT_MAGIC_64 {
        let count = u32::from_be_bytes(head[4..8].try_into().unwrap()) as usize;
        let entry = if be == FAT_MAGIC { 20 } else { 32 };
        for index in 0..count {
            let at = 8 + index * entry;
            let arch = data.get(at..at + entry).ok_or_else(|| report!("a damaged fat header"))?;
            let (offset, size) = if be == FAT_MAGIC {
                (u32::from_be_bytes(arch[8..12].try_into().unwrap()) as usize, u32::from_be_bytes(arch[12..16].try_into().unwrap()) as usize)
            } else {
                (u64::from_be_bytes(arch[8..16].try_into().unwrap()) as usize, u64::from_be_bytes(arch[16..24].try_into().unwrap()) as usize)
            };
            let slice = data.get(offset..offset + size).ok_or_else(|| report!("a fat slice runs past the end of the file"))?;
            if slice.len() >= 4 && u32::from_le_bytes(slice[..4].try_into().unwrap()) == MH_MAGIC_64 {
                found.push((offset, size));
            }
        }
    } else if le == MH_MAGIC_64 {
        found.push((0, data.len()));
    } else if le == MH_MAGIC {
        bail!("a 32-bit Mach-O file, which no device Apparition reaches runs");
    } else {
        bail!("not a Mach-O file");
    }
    if found.is_empty() {
        bail!("no 64-bit code in this Mach-O file");
    }
    Ok(found)
}

/// One load command: its type and every byte of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    pub cmd: u32,
    pub bytes: Vec<u8>,
}

/// A 64-bit slice's load commands, and how far they may grow.
#[derive(Debug, Clone)]
pub struct Slice {
    pub filetype: u32,
    pub commands: Vec<Command>,
    /// Where the first section's (or segment's) data starts: load commands end before it.
    limit: usize,
    /// The commands' size as read, so `write` can clear what a shrunk list no longer uses.
    original_size: usize,
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    bytes.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    bytes.get(at..at + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
}

/// The NUL-terminated string at `offset` (from the command's own start) in a command.
fn string_at(bytes: &[u8], offset: usize) -> Option<String> {
    let tail = bytes.get(offset..)?;
    let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
    String::from_utf8(tail[..end].to_vec()).ok()
}

fn is_dylib_load(cmd: u32) -> bool {
    matches!(cmd, LC_LOAD_DYLIB | LC_LOAD_WEAK_DYLIB | LC_REEXPORT_DYLIB | LC_LAZY_LOAD_DYLIB | LC_LOAD_UPWARD_DYLIB)
}

fn align8(n: usize) -> usize {
    (n + 7) & !7
}

impl Slice {
    pub fn read(slice: &[u8]) -> Result<Self, Report> {
        if u32_at(slice, 0) != Some(MH_MAGIC_64) {
            bail!("not a 64-bit Mach-O slice");
        }
        let filetype = u32_at(slice, 12).unwrap();
        let count = u32_at(slice, 16).unwrap() as usize;
        let size = u32_at(slice, 20).unwrap() as usize;
        let region = slice.get(HEADER_64..HEADER_64 + size).ok_or_else(|| report!("load commands run past the end of the file"))?;
        let mut commands = Vec::with_capacity(count);
        let mut limit = slice.len();
        let mut at = 0;
        for _ in 0..count {
            let cmd = u32_at(region, at).ok_or_else(|| report!("a truncated load command"))?;
            let length = u32_at(region, at + 4).ok_or_else(|| report!("a truncated load command"))? as usize;
            if length < 8 || at + length > region.len() {
                bail!("a damaged load command");
            }
            let bytes = region[at..at + length].to_vec();
            if cmd == LC_SEGMENT_64 {
                let fileoff = u64_at(&bytes, 40).unwrap_or(0) as usize;
                let filesize = u64_at(&bytes, 48).unwrap_or(0) as usize;
                if fileoff > 0 && filesize > 0 {
                    limit = limit.min(fileoff);
                }
                let sections = u32_at(&bytes, 64).unwrap_or(0) as usize;
                for index in 0..sections {
                    let offset = u32_at(&bytes, 72 + index * 80 + 48).unwrap_or(0) as usize;
                    if offset > 0 {
                        limit = limit.min(offset);
                    }
                }
            }
            commands.push(Command { cmd, bytes });
            at += length;
        }
        Ok(Slice { filetype, commands, limit, original_size: size })
    }

    fn size(&self) -> usize {
        self.commands.iter().map(|c| c.bytes.len()).sum()
    }

    /// Writes the commands back into `slice` (the same bytes `read` read).
    pub fn write(&self, slice: &mut [u8]) -> Result<(), Report> {
        let size = self.size();
        let reserve = if self.commands.iter().any(|c| c.cmd == LC_CODE_SIGNATURE) { 0 } else { SIGNATURE_COMMAND };
        if HEADER_64 + size + reserve > self.limit {
            bail!(
                "its load commands don't fit: {} bytes were free before its code starts, and the changes need {}",
                self.limit.saturating_sub(HEADER_64 + self.original_size + reserve),
                size.saturating_sub(self.original_size)
            );
        }
        slice[16..20].copy_from_slice(&(self.commands.len() as u32).to_le_bytes());
        slice[20..24].copy_from_slice(&(size as u32).to_le_bytes());
        let mut at = HEADER_64;
        for command in &self.commands {
            slice[at..at + command.bytes.len()].copy_from_slice(&command.bytes);
            at += command.bytes.len();
        }
        let end = HEADER_64 + self.original_size.max(size);
        slice[at..end].fill(0);
        Ok(())
    }

    /// Every library it loads: (index of the command, its path).
    pub fn dylibs(&self) -> Vec<(usize, String)> {
        self.commands
            .iter()
            .enumerate()
            .filter(|(_, c)| is_dylib_load(c.cmd))
            .filter_map(|(index, c)| Some((index, string_at(&c.bytes, u32_at(&c.bytes, 8)? as usize)?)))
            .collect()
    }

    pub fn rpaths(&self) -> Vec<String> {
        self.commands
            .iter()
            .filter(|c| c.cmd == LC_RPATH)
            .filter_map(|c| string_at(&c.bytes, u32_at(&c.bytes, 8)? as usize))
            .collect()
    }

    /// Whether the App Store's FairPlay encryption is still on (`cryptid` set).
    pub fn encrypted(&self) -> bool {
        self.commands.iter().any(|c| c.cmd == LC_ENCRYPTION_INFO_64 && u32_at(&c.bytes, 16).unwrap_or(0) != 0)
    }

    /// The platform it's built for: `LC_BUILD_VERSION`'s, else what an older version-min
    /// command implies.
    pub fn platform(&self) -> Option<u32> {
        self.commands.iter().find_map(|c| match c.cmd {
            LC_BUILD_VERSION => u32_at(&c.bytes, 8),
            LC_VERSION_MIN_IPHONEOS => Some(PLATFORM_IOS),
            LC_VERSION_MIN_TVOS => Some(PLATFORM_TVOS),
            _ => None,
        })
    }

    /// Points the library command at `index` at `path`, keeping its kind and versions.
    pub fn set_dylib_path(&mut self, index: usize, path: &str) {
        let old = &self.commands[index];
        let (timestamp, current, compatibility) = (
            u32_at(&old.bytes, 12).unwrap_or(0),
            u32_at(&old.bytes, 16).unwrap_or(0),
            u32_at(&old.bytes, 20).unwrap_or(0),
        );
        self.commands[index] = dylib_command(old.cmd, path, timestamp, current, compatibility);
    }

    /// Loads `path` too, weakly when `weak` (a missing library then doesn't stop the app),
    /// unless it already loads exactly that path. True when it was added.
    pub fn add_dylib(&mut self, path: &str, weak: bool) -> bool {
        if self.dylibs().iter().any(|(_, existing)| existing == path) {
            return false;
        }
        // insert_dylib's values: no version is asked of the library.
        self.commands.push(dylib_command(if weak { LC_LOAD_WEAK_DYLIB } else { LC_LOAD_DYLIB }, path, 2, 0, 0));
        true
    }

    /// Searches `path` for `@rpath/` libraries too, unless it already does. True when added.
    pub fn add_rpath(&mut self, path: &str) -> bool {
        if self.rpaths().iter().any(|existing| existing == path) {
            return false;
        }
        let size = align8(12 + path.len() + 1);
        let mut bytes = Vec::with_capacity(size);
        bytes.extend_from_slice(&LC_RPATH.to_le_bytes());
        bytes.extend_from_slice(&(size as u32).to_le_bytes());
        bytes.extend_from_slice(&12u32.to_le_bytes());
        bytes.extend_from_slice(path.as_bytes());
        bytes.resize(size, 0);
        self.commands.push(Command { cmd: LC_RPATH, bytes });
        true
    }
}

fn dylib_command(cmd: u32, path: &str, timestamp: u32, current: u32, compatibility: u32) -> Command {
    let size = align8(24 + path.len() + 1);
    let mut bytes = Vec::with_capacity(size);
    bytes.extend_from_slice(&cmd.to_le_bytes());
    bytes.extend_from_slice(&(size as u32).to_le_bytes());
    bytes.extend_from_slice(&24u32.to_le_bytes());
    bytes.extend_from_slice(&timestamp.to_le_bytes());
    bytes.extend_from_slice(&current.to_le_bytes());
    bytes.extend_from_slice(&compatibility.to_le_bytes());
    bytes.extend_from_slice(path.as_bytes());
    bytes.resize(size, 0);
    Command { cmd, bytes }
}

/// Reads every 64-bit slice of `data`, lets `edit` change each, and writes them back. `edit`
/// returns whether it changed anything; the file is left alone when no slice changed.
pub fn edit(data: &mut [u8], mut edit: impl FnMut(&mut Slice) -> Result<bool, Report>) -> Result<bool, Report> {
    let mut changed = false;
    for (start, length) in slices(data)? {
        let bytes = &mut data[start..start + length];
        let mut slice = Slice::read(bytes)?;
        if edit(&mut slice)? {
            slice.write(bytes)?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Every 64-bit slice of `data`, read.
pub fn read_all(data: &[u8]) -> Result<Vec<Slice>, Report> {
    slices(data)?.into_iter().map(|(start, length)| Slice::read(&data[start..start + length])).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn imports_are_undefined_symbols_in_each_slice() {
        let strings = b"\0_MSHookFunction\0_LHHookFunctions\0";
        let command = Command { cmd: 2, bytes: [2u32, 24, 0x2000, 2, 0x2020, strings.len() as u32].into_iter().flat_map(u32::to_le_bytes).collect() };
        let mut data = build(MH_DYLIB, 0x2000, &[command]);
        data.resize(0x2020 + strings.len(), 0);
        data[0x2000..0x2020].fill(0);
        data[0x2000..0x2004].copy_from_slice(&1u32.to_le_bytes());
        data[0x2004] = 1; // external undefined
        data[0x2010..0x2014].copy_from_slice(&17u32.to_le_bytes());
        data[0x2014] = 0x0f; // external definition, not an import
        data[0x2020..0x2020 + strings.len()].copy_from_slice(strings);
        assert_eq!(imported_symbols(&data).unwrap(), ["_MSHookFunction"]);
        assert_eq!(imported_symbols(&fat(&[data.clone(), data.clone()])).unwrap(), ["_MSHookFunction"]);
        data[0x2000..0x2004].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(imported_symbols(&data).is_err());
        data.truncate(0x2010);
        assert!(imported_symbols(&data).is_err());
    }

    /// A minimal 64-bit Mach-O: a __TEXT segment whose one section starts at `code_at`, then
    /// the given extra commands. Enough for `Slice` to read and edit.
    pub(crate) fn build(filetype: u32, code_at: u32, extra: &[Command]) -> Vec<u8> {
        let mut segment = Vec::new();
        segment.extend_from_slice(&LC_SEGMENT_64.to_le_bytes());
        segment.extend_from_slice(&(72u32 + 80).to_le_bytes());
        let mut name = [0u8; 16];
        name[..6].copy_from_slice(b"__TEXT");
        segment.extend_from_slice(&name);
        segment.extend_from_slice(&0u64.to_le_bytes()); // vmaddr
        segment.extend_from_slice(&0x4000u64.to_le_bytes()); // vmsize
        segment.extend_from_slice(&0u64.to_le_bytes()); // fileoff
        segment.extend_from_slice(&0x4000u64.to_le_bytes()); // filesize
        segment.extend_from_slice(&5u32.to_le_bytes());
        segment.extend_from_slice(&5u32.to_le_bytes());
        segment.extend_from_slice(&1u32.to_le_bytes()); // nsects
        segment.extend_from_slice(&0u32.to_le_bytes());
        let mut section = vec![0u8; 80];
        section[..6].copy_from_slice(b"__text");
        section[16..22].copy_from_slice(b"__TEXT");
        section[48..52].copy_from_slice(&code_at.to_le_bytes());
        segment.extend_from_slice(&section);
        let mut commands = vec![Command { cmd: LC_SEGMENT_64, bytes: segment }];
        commands.extend_from_slice(extra);
        let size: usize = commands.iter().map(|c| c.bytes.len()).sum();
        let mut data = Vec::new();
        data.extend_from_slice(&MH_MAGIC_64.to_le_bytes());
        data.extend_from_slice(&0x0100_000cu32.to_le_bytes()); // CPU_TYPE_ARM64
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&filetype.to_le_bytes());
        data.extend_from_slice(&(commands.len() as u32).to_le_bytes());
        data.extend_from_slice(&(size as u32).to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        for command in &commands {
            data.extend_from_slice(&command.bytes);
        }
        data.resize(code_at as usize + 64, 0);
        data[code_at as usize..].fill(0xAA);
        data
    }

    pub(crate) fn load(path: &str) -> Command {
        dylib_command(LC_LOAD_DYLIB, path, 2, 0x10000, 0x10000)
    }

    pub(crate) fn build_version(platform: u32) -> Command {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&LC_BUILD_VERSION.to_le_bytes());
        bytes.extend_from_slice(&24u32.to_le_bytes());
        bytes.extend_from_slice(&platform.to_le_bytes());
        bytes.extend_from_slice(&0x0011_0000u32.to_le_bytes());
        bytes.extend_from_slice(&0x0011_0000u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        Command { cmd: LC_BUILD_VERSION, bytes }
    }

    pub(crate) fn encryption(cryptid: u32) -> Command {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&LC_ENCRYPTION_INFO_64.to_le_bytes());
        bytes.extend_from_slice(&24u32.to_le_bytes());
        bytes.extend_from_slice(&0x4000u32.to_le_bytes());
        bytes.extend_from_slice(&0x4000u32.to_le_bytes());
        bytes.extend_from_slice(&cryptid.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        Command { cmd: LC_ENCRYPTION_INFO_64, bytes }
    }

    /// `slices` wrapped in a fat header, one after another at 16 KB boundaries.
    pub(crate) fn fat(slices: &[Vec<u8>]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&FAT_MAGIC.to_be_bytes());
        data.extend_from_slice(&(slices.len() as u32).to_be_bytes());
        let mut offset = 0x4000usize;
        let mut placed = Vec::new();
        for slice in slices {
            data.extend_from_slice(&0x0100_000cu32.to_be_bytes());
            data.extend_from_slice(&0u32.to_be_bytes());
            data.extend_from_slice(&(offset as u32).to_be_bytes());
            data.extend_from_slice(&(slice.len() as u32).to_be_bytes());
            data.extend_from_slice(&14u32.to_be_bytes());
            placed.push(offset);
            offset = (offset + slice.len() + 0x3fff) & !0x3fff;
        }
        for (slice, at) in slices.iter().zip(placed) {
            data.resize(at, 0);
            data.extend_from_slice(slice);
        }
        data
    }

    #[test]
    fn reads_libraries_platform_and_encryption() {
        let data = build(MH_EXECUTE, 0x1000, &[load("/usr/lib/libSystem.B.dylib"), build_version(PLATFORM_IOS), encryption(0)]);
        let slice = Slice::read(&data).unwrap();
        assert_eq!(slice.dylibs().iter().map(|(_, p)| p.as_str()).collect::<Vec<_>>(), ["/usr/lib/libSystem.B.dylib"]);
        assert_eq!(slice.platform(), Some(PLATFORM_IOS));
        assert!(!slice.encrypted());
        assert!(Slice::read(&build(MH_EXECUTE, 0x1000, &[encryption(1)])).unwrap().encrypted());
    }

    #[test]
    fn adds_and_rewrites_commands_without_touching_the_code() {
        let mut data = build(MH_DYLIB, 0x1000, &[load("/Library/Frameworks/CydiaSubstrate.framework/CydiaSubstrate")]);
        let changed = edit(&mut data, |slice| {
            let (index, _) = slice.dylibs()[0];
            slice.set_dylib_path(index, "@rpath/CydiaSubstrate.framework/CydiaSubstrate");
            assert!(slice.add_dylib("@rpath/Tweak.dylib", true));
            assert!(!slice.add_dylib("@rpath/Tweak.dylib", true), "the same path twice");
            assert!(slice.add_rpath("@executable_path/Frameworks"));
            assert!(!slice.add_rpath("@executable_path/Frameworks"));
            Ok(true)
        })
        .unwrap();
        assert!(changed);
        let slice = Slice::read(&data).unwrap();
        let paths: Vec<_> = slice.dylibs().into_iter().map(|(_, p)| p).collect();
        assert_eq!(paths, ["@rpath/CydiaSubstrate.framework/CydiaSubstrate", "@rpath/Tweak.dylib"]);
        assert_eq!(slice.commands.iter().filter(|c| c.cmd == LC_LOAD_WEAK_DYLIB).count(), 1);
        assert_eq!(slice.rpaths(), ["@executable_path/Frameworks"]);
        assert!(data[0x1000..].iter().all(|&b| b == 0xAA), "the code is untouched");
    }

    #[test]
    fn refuses_what_does_not_fit_and_changes_nothing() {
        // Room for the header and the segment only: no command fits.
        let mut data = build(MH_EXECUTE, 32 + 152 + 16 + 8, &[]);
        let before = data.clone();
        let error = edit(&mut data, |slice| {
            slice.add_dylib("@rpath/AVeryLongTweakNameThatCannotFit.dylib", true);
            Ok(true)
        });
        assert!(error.is_err());
        assert_eq!(data, before);
    }

    #[test]
    fn a_shorter_path_clears_what_it_no_longer_uses() {
        let mut data = build(MH_DYLIB, 0x1000, &[load("/var/jb/Library/Frameworks/Very/Long/Path/To/Some.framework/Some")]);
        edit(&mut data, |slice| {
            slice.set_dylib_path(1, "@rpath/S");
            Ok(true)
        })
        .unwrap();
        let slice = Slice::read(&data).unwrap();
        let end = 32 + slice.size();
        assert!(data[end..0x1000].iter().all(|&b| b == 0));
    }

    #[test]
    fn edits_every_slice_of_a_fat_binary() {
        let one = build(MH_EXECUTE, 0x1000, &[load("/usr/lib/libSystem.B.dylib")]);
        let mut data = fat(&[one.clone(), one]);
        assert_eq!(slices(&data).unwrap().len(), 2);
        edit(&mut data, |slice| Ok(slice.add_dylib("@rpath/Tweak.dylib", true))).unwrap();
        for slice in read_all(&data).unwrap() {
            assert!(slice.dylibs().iter().any(|(_, p)| p == "@rpath/Tweak.dylib"));
        }
    }

    #[test]
    fn knows_a_mach_o_from_anything_else() {
        assert!(is_macho(&build(MH_DYLIB, 0x1000, &[])));
        assert!(is_macho(&fat(&[build(MH_DYLIB, 0x1000, &[])])));
        assert!(!is_macho(b"PK\x03\x04"));
        assert!(!is_macho(b"!<arch>\n"));
        assert!(slices(b"!<arch>\nxxxxxxxx").is_err());
    }
}
