/// Whether dotted version `candidate` is newer than `current` ("0.10.0" > "0.9.2"). A '+' suffix
/// (build metadata) is ignored. A '-' suffix marks a pre-release, which is older than its release:
/// "0.3.0" is newer than "0.3.0-beta.1", never the other way round, and two pre-releases of one
/// version aren't ordered.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let pre = |v: &str| v.split('+').next().unwrap_or("").contains('-');
    let parts = |v: &str| -> Vec<u64> {
        v.split(['-', '+']).next().unwrap_or("").split('.').map(|p| p.parse().unwrap_or(0)).collect()
    };
    let (a, b) = (parts(candidate), parts(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    pre(current) && !pre(candidate)
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn compares_numerically_and_ignores_suffixes() {
        assert!(is_newer("0.10.0", "0.9.2"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0-beta", "0.1.0"));
        assert!(!is_newer("0.9.2", "0.10.0"));
        assert!(is_newer("0.3.0", "0.3.0-beta.1"), "a release replaces its pre-release");
        assert!(!is_newer("0.3.0-beta.2", "0.3.0-beta.1"), "pre-releases aren't ordered");
        assert!(!is_newer("0.3.0+build.7", "0.3.0"), "build metadata isn't a version");
        assert!(is_newer("0.3.1-beta", "0.3.0"));
    }
}
