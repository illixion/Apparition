//! Advertise the hook protocol only to our own library while all threads stop.
//! A loaded image's dedicated section avoids symbol-server or ASLR assumptions.
use super::EntryRemote;
use rootcause::prelude::*;
const MAGIC: u64 = 0x4150_484f_4f4b_3031;
pub(super) const CAPABILITY: u64 = 0x4150_484b;
async fn read(remote: &mut impl EntryRemote, address: u64, length: usize) -> Result<Vec<u8>, Report> {
    if length > 256 * 1024 { bail!("hook metadata is too large"); }
    let mut bytes = Vec::with_capacity(length);
    while bytes.len() < length {
        let count = (length - bytes.len()).min(2048);
        let reply = remote.command(&format!("m{:x},{count:x}", address + bytes.len() as u64)).await?;
        if reply.len() != count * 2 || !reply.is_ascii() { bail!("couldn't read hook metadata"); }
        for pair in reply.as_bytes().chunks_exact(2) {
            bytes.push(u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).context("invalid hook metadata")?);
        }
    }
    Ok(bytes)
}
async fn path_string(remote: &mut impl EntryRemote, address: u64) -> Result<String, Report> {
    if let Ok(bytes) = read(remote, address, 256).await {
        if let Some(end) = bytes.iter().position(|&b| b == 0) {
            return Ok(String::from_utf8_lossy(&bytes[..end]).into_owned());
        }
    }
    // dyld may store a path at the very end of a mapped page. A fixed-size
    // read past the NUL can fail even though the string itself is readable.
    let mut bytes = Vec::new();
    while bytes.len() < 4096 {
        let next = address + bytes.len() as u64;
        let count = (4096 - next as usize % 4096).min(64);
        let chunk = read(remote, next, count).await?;
        if let Some(end) = chunk.iter().position(|&b| b == 0) {
            bytes.extend_from_slice(&chunk[..end]);
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
        bytes.extend(chunk);
    }
    bail!("unterminated image path")
}
fn u32_at(data: &[u8], offset: usize) -> Option<u32> { Some(u32::from_le_bytes(data.get(offset..offset + 4)?.try_into().ok()?)) }
fn u64_at(data: &[u8], offset: usize) -> Option<u64> { Some(u64::from_le_bytes(data.get(offset..offset + 8)?.try_into().ok()?)) }
fn name(data: &[u8], wanted: &[u8]) -> bool { data.split(|&b| b == 0).next() == Some(wanted) }
/// Parse mapped load commands and return the control section's slid address.
fn control_section(header: u64, commands: &[u8], count: u32) -> Option<u64> {
    let mut at = 0usize;
    let mut text = None;
    let mut control = None;
    for _ in 0..count {
        let size = u32_at(commands, at + 4)? as usize;
        if size < 8 { return None; }
        let cmd = commands.get(at..at.checked_add(size)?)?;
        if u32_at(cmd, 0)? == 0x19 && size >= 72 {
            if name(&cmd[8..24], b"__TEXT") && u64_at(cmd, 40)? == 0 { text = u64_at(cmd, 24); }
            let nsects = u32_at(cmd, 64)? as usize;
            if nsects > (size - 72) / 80 { return None; }
            for section in cmd[72..72 + nsects * 80].chunks_exact(80) {
                if name(&section[..16], b"__aphooks") && name(&section[16..32], b"__DATA") && u64_at(section, 40)? >= 40 {
                    control = u64_at(section, 32);
                }
            }
        }
        at += size;
    }
    header.checked_sub(text?)?.checked_add(control?)
}
pub(super) async fn arm(remote: &mut impl EntryRemote) -> Result<Option<u64>, Report> {
    // debugserver can return address/name pairs in one packet. Reading every
    // dyld pathname individually held real devices stopped for ~16 seconds.
    let reply = remote.command("jGetLoadedDynamicLibrariesInfos:{\"fetch_all_solibs\":true,\"information-level\":\"address-name\"}").await?;
    if let Some(headers) = candidate_headers(&reply) {
        for header in headers {
            if let Some(control) = arm_image(remote, header).await? { return Ok(Some(control)); }
        }
        return Ok(None);
    }
    let reply = remote.command("qShlibInfoAddr").await?;
    let Ok(address) = u64::from_str_radix(&reply, 16) else { return Ok(None); };
    let info = read(remote, address, 16).await?;
    let count = u32_at(&info, 4).unwrap_or(0) as usize;
    let array = u64_at(&info, 8).unwrap_or(0);
    if array == 0 || count == 0 || count > 4096 { return Ok(None); }
    let images = read(remote, array, count * 24).await?;
    for (index, image) in images.chunks_exact(24).enumerate() {
        let header = u64_at(image, 0).unwrap();
        let path = u64_at(image, 8).unwrap();
        if path == 0 { continue; }
        let Ok(path) = path_string(remote, path).await else { continue; };
        if index != 0 && !path.contains("CydiaSubstrate") && !path.contains("ApparitionHooks") { continue; }
        if let Some(control) = arm_image(remote, header).await? { return Ok(Some(control)); }
    }
    Ok(None)
}
fn candidate_headers(reply: &str) -> Option<Vec<u64>> {
    let value: serde_json::Value = serde_json::from_str(reply).ok()?;
    let images = value.get("images")?.as_array()?;
    if images.len() > 4096 { return None; }
    let mut headers = Vec::new();
    for image in images {
        let path = image.get("pathname").and_then(|v| v.as_str()).unwrap_or("");
        if path.contains("CydiaSubstrate") || path.contains("ApparitionHooks") {
            if let Some(address) = image.get("load_address").and_then(|v| v.as_u64()) { headers.push(address); }
        }
    }
    // The Lab links the runtime into its main executable.
    if let Some(address) = images.first().and_then(|i| i.get("load_address")).and_then(|v| v.as_u64()) {
        if !headers.contains(&address) { headers.push(address); }
    }
    Some(headers)
}
async fn arm_image(remote: &mut impl EntryRemote, header: u64) -> Result<Option<u64>, Report> {
    let bytes = read(remote, header, 32).await?;
    if u32_at(&bytes, 0) != Some(0xfeed_facf) { return Ok(None); }
    let ncmds = u32_at(&bytes, 16).unwrap();
    let size = u32_at(&bytes, 20).unwrap() as usize;
    let commands = read(remote, header + 32, size).await?;
    let Some(control) = control_section(header, &commands, ncmds) else { return Ok(None); };
    let info = read(remote, control, 40).await?;
    if u64_at(&info, 0) != Some(MAGIC) || u64_at(&info, 8) != Some(1) { return Ok(None); }
    if write_ready(remote, control, CAPABILITY).await? { return Ok(Some(control)); }
    Ok(None)
}
async fn write_ready(remote: &mut impl EntryRemote, control: u64, value: u64) -> Result<bool, Report> {
    let bytes = value.to_le_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>();
    let reply = remote.command(&format!("M{:x},8:{bytes}", control + 16)).await?;
    if reply != "OK" { return Ok(false); }
    Ok(read(remote, control + 16, 8).await? == value.to_le_bytes())
}
pub(super) async fn revoke(remote: &mut impl EntryRemote, control: Option<u64>) -> Result<(), Report> {
    if let Some(control) = control {
        if !write_ready(remote, control, 0).await? { bail!("couldn't clear the hook debugger capability"); }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bulk_image_lookup_selects_the_runtime_before_the_main_image() {
        let reply = r#"{"images":[{"load_address":4096,"pathname":"/app/Main"},{"load_address":8192,"pathname":"/System/lib.dylib"},{"load_address":12288,"pathname":"/app/Frameworks/CydiaSubstrate.framework/CydiaSubstrate"}]}"#;
        assert_eq!(candidate_headers(reply), Some(vec![12288, 4096]));
        assert_eq!(candidate_headers("E01"), None);
    }
    #[test]
    fn section_lookup_uses_the_image_slide_and_validates_bounds() {
        let mut commands = vec![0u8; 72 + 72 + 80];
        for (at, size) in [(0usize, 72u32), (72, 152)] {
            commands[at..at + 4].copy_from_slice(&0x19u32.to_le_bytes());
            commands[at + 4..at + 8].copy_from_slice(&size.to_le_bytes());
        }
        commands[8..14].copy_from_slice(b"__TEXT");
        commands[24..32].copy_from_slice(&0x1000u64.to_le_bytes());
        commands[136..140].copy_from_slice(&1u32.to_le_bytes());
        commands[144..153].copy_from_slice(b"__aphooks");
        commands[160..166].copy_from_slice(b"__DATA");
        commands[176..184].copy_from_slice(&0x3000u64.to_le_bytes());
        commands[184..192].copy_from_slice(&40u64.to_le_bytes());
        assert_eq!(control_section(0x9000, &commands, 2), Some(0xb000));
        assert_eq!(control_section(0x9000, &commands[..180], 2), None);
        commands[136..140].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(control_section(0x9000, &commands, 2), None);
    }
}

#[cfg(test)]
mod remote_tests {
    use super::*;
    struct Remote { memory: std::collections::BTreeMap<u64, Vec<u8>> }
    impl super::super::EntryRemote for Remote {
        async fn pc(&mut self, _: u64) -> Result<u64, Report> { Ok(0) }
        async fn command(&mut self, command: &str) -> Result<String, Report> {
            if command.starts_with("jGetLoadedDynamicLibrariesInfos:") { return Ok(String::new()); }
            if command == "qShlibInfoAddr" { return Ok("1000".into()); }
            if let Some(read) = command.strip_prefix('m') {
                let (address, count) = read.split_once(',').unwrap();
                let address = u64::from_str_radix(address, 16).unwrap();
                let count = usize::from_str_radix(count, 16).unwrap();
                let bytes = self.memory.get(&address).ok_or_else(|| report!("unmapped"))?;
                let bytes = bytes.get(..count).ok_or_else(|| report!("read crosses mapping"))?;
                return Ok(bytes.iter().map(|b| format!("{b:02x}")).collect());
            }
            let (head, bytes) = command.strip_prefix('M').unwrap().split_once(':').unwrap();
            let address = u64::from_str_radix(head.split(',').next().unwrap(), 16).unwrap();
            let bytes = bytes.as_bytes().chunks_exact(2).map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap()).collect();
            self.memory.insert(address, bytes);
            Ok("OK".into())
        }
    }
    #[tokio::test]
    async fn image_paths_at_page_boundaries_do_not_require_unmapped_tail_bytes() {
        let mut remote = Remote { memory: std::collections::BTreeMap::from([(0x3ffc, b"lib\0".to_vec())]) };
        assert_eq!(path_string(&mut remote, 0x3ffc).await.unwrap(), "lib");
    }
    #[tokio::test]
    async fn advertises_only_a_versioned_control_section_and_revokes_before_detach() {
        let mut memory = std::collections::BTreeMap::new();
        let mut info = vec![0; 16]; info[4..8].copy_from_slice(&1u32.to_le_bytes()); info[8..16].copy_from_slice(&0x2000u64.to_le_bytes());
        memory.insert(0x1000, info);
        let image: Vec<_> = [0x9000u64, 0x3000, 0].into_iter().flat_map(u64::to_le_bytes).collect();
        memory.insert(0x2000, image);
        let mut path = vec![0; 256]; path[..4].copy_from_slice(b"main"); memory.insert(0x3000, path);
        let mut header = vec![0; 32]; header[..4].copy_from_slice(&0xfeed_facfu32.to_le_bytes());
        header[16..20].copy_from_slice(&1u32.to_le_bytes()); header[20..24].copy_from_slice(&152u32.to_le_bytes());
        memory.insert(0x9000, header);
        let mut commands = vec![0; 152]; commands[..4].copy_from_slice(&0x19u32.to_le_bytes()); commands[4..8].copy_from_slice(&152u32.to_le_bytes());
        commands[8..14].copy_from_slice(b"__TEXT"); commands[24..32].copy_from_slice(&0x1000u64.to_le_bytes()); commands[64..68].copy_from_slice(&1u32.to_le_bytes());
        commands[72..81].copy_from_slice(b"__aphooks"); commands[88..94].copy_from_slice(b"__DATA"); commands[104..112].copy_from_slice(&0x3000u64.to_le_bytes()); commands[112..120].copy_from_slice(&40u64.to_le_bytes());
        memory.insert(0x9020, commands);
        let control = [MAGIC, 1, 0, 0, 0].into_iter().flat_map(u64::to_le_bytes).collect(); memory.insert(0xb000, control);
        let mut remote = Remote { memory };
        assert_eq!(arm(&mut remote).await.unwrap(), Some(0xb000));
        assert_eq!(remote.memory[&0xb010], CAPABILITY.to_le_bytes());
        revoke(&mut remote, Some(0xb000)).await.unwrap();
        assert_eq!(remote.memory[&0xb010], [0; 8]);
        remote.memory.get_mut(&0xb000).unwrap()[8] = 2;
        assert_eq!(arm(&mut remote).await.unwrap(), None);
    }
}
