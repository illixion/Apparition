//! A release is one `SHA256SUMS` file listing its assets, signed (SSHSIG) by a pinned key. That
//! file is the only thing trusted: where a file is downloaded from proves nothing, and the version
//! is covered too, because every signed file name carries it (`Vergence-0.2.0-macos-universal.zip`),
//! so a host can't relabel an old release as a new one.

use std::collections::BTreeMap;

use crate::error::{Result, fail};
use crate::sshsig::verify_sshsig;

/// A signed manifest whose names all belong to one product.
#[derive(Debug, Clone)]
pub struct Manifest {
    sums: BTreeMap<String, String>,
    /// The principal of the key that signed it.
    pub signer: String,
}

impl Manifest {
    /// Checks `sums` against `signature` (made under `namespace` by one of `signers`), then that
    /// every listed file is `<product>-…`.
    pub fn verify(sums: &[u8], signature: &str, namespace: &str, signers: &str, product: &str) -> Result<Self> {
        let signer = verify_sshsig(sums, signature, namespace, signers)?;
        let sums = parse_sums(&String::from_utf8_lossy(sums))?;
        let prefix = format!("{product}-");
        for name in sums.keys() {
            if !name.starts_with(&prefix) {
                fail!("The signed manifest lists {name}, which isn't part of {product}");
            }
        }
        Ok(Self { sums, signer })
    }

    /// The signed SHA-256 (lowercase hex) of `name`.
    pub fn hash(&self, name: &str) -> Option<&str> {
        self.sums.get(name).map(String::as_str)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.sums.keys().map(String::as_str)
    }

    /// The version and name of the asset matching `pattern`, a file name with `{version}` in it
    /// (`Vergence-{version}-macos-universal.zip`); None when the release has no such asset (a
    /// release for another platform only). Refuses a manifest that lists one version of it twice
    /// over, and one whose other files don't carry the same version.
    pub fn find(&self, pattern: &str) -> Result<Option<(String, String)>> {
        let Some((prefix, suffix)) = pattern.split_once("{version}") else {
            fail!("the asset pattern {pattern} has no {{version}}");
        };
        let mut found: Option<(String, String)> = None;
        for name in self.sums.keys() {
            let Some(version) = name.strip_prefix(prefix).and_then(|rest| rest.strip_suffix(suffix)) else {
                continue;
            };
            if !is_version(version) {
                continue;
            }
            if let Some((other, _)) = &found {
                fail!("The signed manifest lists {pattern} for both {other} and {version}");
            }
            found = Some((version.to_string(), name.clone()));
        }
        let Some((version, name)) = found else { return Ok(None) };
        for other in self.sums.keys() {
            if !other.contains(&format!("-{version}-")) && !other.contains(&format!("-{version}.")) {
                fail!("The signed manifest lists {other}, which isn't part of version {version}");
            }
        }
        Ok(Some((version, name)))
    }
}

/// Digits and dots, at least one dot: "0.2.0", "1.10".
fn is_version(text: &str) -> bool {
    text.contains('.')
        && text.split('.').all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// `sha256sum`/`shasum -a 256` output: `<hex>  <name>` per line (a `*` before the name, for binary
/// mode, is allowed). Name -> lowercase hex.
pub fn parse_sums(text: &str) -> Result<BTreeMap<String, String>> {
    let mut sums = BTreeMap::new();
    for line in text.lines().map(str::trim_end).filter(|l| !l.is_empty()) {
        let (hash, name) = line
            .split_once(char::is_whitespace)
            .ok_or_else(|| format!("bad SHA256SUMS line: {line}"))?;
        let name = name.trim_start().trim_start_matches('*');
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) || name.is_empty() {
            fail!("bad SHA256SUMS line: {line}");
        }
        if sums.insert(name.to_string(), hash.to_ascii_lowercase()).is_some() {
            fail!("SHA256SUMS lists {name} twice");
        }
    }
    if sums.is_empty() {
        fail!("SHA256SUMS is empty");
    }
    Ok(sums)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(names: &[&str]) -> Manifest {
        Manifest { sums: names.iter().map(|n| (n.to_string(), "0".repeat(64))).collect(), signer: "t".into() }
    }

    #[test]
    fn parses_sums_strictly() {
        assert!(parse_sums("").is_err());
        assert!(parse_sums("xyz  a").is_err());
        let a = "0".repeat(64);
        assert!(parse_sums(&format!("{a}  x\n{a}  x\n")).is_err());
        assert_eq!(parse_sums(&format!("{a} *x\n")).unwrap()["x"], a);
        assert_eq!(parse_sums(&format!("{}  x\n", a.replace('0', "A"))).unwrap()["x"], "a".repeat(64));
    }

    #[test]
    fn finds_the_version_in_the_signed_name() {
        let m = manifest(&["Vergence-1.2.0-macos-universal.zip", "Vergence-1.2.0-windows-x86_64.zip"]);
        let (version, name) = m.find("Vergence-{version}-macos-universal.zip").unwrap().unwrap();
        assert_eq!((version.as_str(), name.as_str()), ("1.2.0", "Vergence-1.2.0-macos-universal.zip"));
        assert!(m.find("Vergence-{version}-linux-x86_64.zip").unwrap().is_none());
        assert!(m.find("Vergence-macos.zip").is_err());
    }

    #[test]
    fn refuses_a_manifest_that_mixes_versions() {
        let m = manifest(&["Vergence-1.2.0-macos-universal.zip", "Vergence-1.1.0-windows-x86_64.zip"]);
        assert!(m.find("Vergence-{version}-macos-universal.zip").is_err());
        let twice = manifest(&["Vergence-1.2.0-macos-universal.zip", "Vergence-1.1.0-macos-universal.zip"]);
        assert!(twice.find("Vergence-{version}-macos-universal.zip").is_err());
    }

    #[test]
    fn a_name_that_merely_resembles_a_version_is_not_one() {
        let m = manifest(&["Vergence-latest-macos-universal.zip", "Vergence-1-macos-universal.zip"]);
        assert!(m.find("Vergence-{version}-macos-universal.zip").unwrap().is_none());
    }
}
