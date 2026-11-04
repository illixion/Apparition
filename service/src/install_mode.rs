//! How a build reaches its device, and whether the device's own Apparition is open to take it
//! (`store/lib/install-mode.js`). Modes: `mac` installs through the Mac's own tunnel, `device`
//! lets the Apparition app on the device fetch and install, `none` builds and keeps only, and
//! `auto` is `device` while that device's Apparition is open, else `mac`.

use std::{collections::HashMap, sync::Mutex};

pub const MODES: [&str; 4] = ["auto", "mac", "device", "none"];
/// Apparition polls the service every few seconds while it is in front; a quiet client is closed.
const PRESENCE_TTL_MS: u64 = 20_000;
const MAX_DEVICES: usize = 64;

fn key(name: &str) -> String {
    name.trim().to_lowercase()
}

#[derive(Default)]
pub struct Presence {
    /// key -> (last seen ms, insertion order)
    seen: Mutex<(HashMap<String, (u64, u64)>, u64)>,
}

impl Presence {
    /// The device name a client sends (Xcode's name for it); anything else is ignored.
    pub fn touch(&self, name: &str, now: u64) -> bool {
        let trimmed = name.trim();
        let valid = !trimmed.is_empty() && trimmed.chars().count() <= 128 && !trimmed.chars().any(|c| c.is_control());
        if !valid {
            return false;
        }
        let mut guard = self.seen.lock().unwrap();
        let (seen, counter) = &mut *guard;
        *counter += 1;
        seen.insert(key(name), (now, *counter));
        seen.retain(|_, (at, _)| now.saturating_sub(*at) <= PRESENCE_TTL_MS * 3);
        while seen.len() > MAX_DEVICES {
            let oldest = seen.iter().min_by_key(|(_, (_, order))| *order).map(|(k, _)| k.clone());
            match oldest {
                Some(k) => seen.remove(&k),
                None => break,
            };
        }
        true
    }

    pub fn listening(&self, name: &str, now: u64) -> bool {
        self.seen.lock().unwrap().0.get(&key(name)).is_some_and(|(at, _)| now.saturating_sub(*at) <= PRESENCE_TTL_MS)
    }

    /// Names of devices whose Apparition is open now, for the state a client reads.
    pub fn open(&self, now: u64) -> Vec<String> {
        let guard = self.seen.lock().unwrap();
        let mut open: Vec<_> = guard.0.iter().filter(|(_, (at, _))| now.saturating_sub(*at) <= PRESENCE_TTL_MS).collect();
        open.sort_by_key(|(_, (_, order))| *order);
        open.into_iter().map(|(k, _)| k.clone()).collect()
    }
}

/// The mode a build job gets. `launch` is whether the request wants the app started afterwards:
/// only the tunnel can start a process, so that is `mac` whatever else.
pub fn resolve(install: Option<&str>, deploy: Option<bool>, device: &str, presence: &Presence, launch: bool, now: u64) -> Result<&'static str, String> {
    let requested = match install {
        Some(mode) => *MODES.iter().find(|m| **m == mode).ok_or_else(|| format!("install must be one of {}", MODES.join(", ")))?,
        None if deploy == Some(false) => "none",
        None => "auto",
    };
    if requested != "auto" {
        return Ok(requested);
    }
    if launch {
        return Ok("mac");
    }
    Ok(if presence.listening(device, now) { "device" } else { "mac" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_is_open_for_twenty_seconds_after_it_last_spoke() {
        let p = Presence::default();
        assert!(p.touch("  Vision Pro ", 1_000));
        assert!(p.listening("vision pro", 20_900));
        assert!(!p.listening("vision pro", 21_001));
        assert_eq!(p.open(5_000), ["vision pro"]);
        assert!(p.open(30_000).is_empty());
    }

    #[test]
    fn names_with_control_characters_or_nothing_in_them_are_ignored() {
        let p = Presence::default();
        assert!(!p.touch("", 0) && !p.touch("   ", 0) && !p.touch("a\nb", 0) && !p.touch(&"x".repeat(129), 0));
        assert!(p.open(0).is_empty());
    }

    #[test]
    fn auto_picks_the_device_only_when_its_apparition_is_open_and_nothing_is_launched() {
        let p = Presence::default();
        p.touch("iPhone", 100);
        assert_eq!(resolve(None, None, "iPhone", &p, false, 200), Ok("device"));
        assert_eq!(resolve(None, None, "iPhone", &p, true, 200), Ok("mac"), "launching needs the tunnel");
        assert_eq!(resolve(None, None, "iPad", &p, false, 200), Ok("mac"));
        assert_eq!(resolve(None, Some(false), "iPhone", &p, false, 200), Ok("none"));
        assert_eq!(resolve(Some("device"), None, "iPad", &p, true, 200), Ok("device"));
        assert!(resolve(Some("bogus"), None, "iPad", &p, false, 0).is_err());
    }
}
