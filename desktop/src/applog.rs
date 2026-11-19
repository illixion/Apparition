//! What the desktop app writes to `apparition-core.log`, and how it keeps secrets out of it.
//!
//! The app is a GUI process: its stdout and stderr are `/dev/null`, so `eprintln!` is lost and
//! `tracing` (set up in main.rs by `apparition_core::init_logging`) is the only record. That log
//! is shared in debug traces and read by people and models, so a line may carry **code-defined
//! values** (an operation's name, a status, a count, a duration, a port) and nothing the person,
//! the network or the filesystem supplied. Error text you didn't write goes through [`scrub`]
//! first, or [`chain`] (which scrubs), and never a URL, address, token, email or home folder.

/// `text` with what must not reach the log taken out: URLs, email addresses, IP addresses,
/// home-folder user names and anything shaped like a token (a long run of letters, digits and
/// `-_+/=` with both). It works word by word and keeps everything else, so an error still reads
/// as one.
pub fn scrub(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    for c in text.chars() {
        if c.is_whitespace() {
            out.push_str(&scrub_word(&word));
            word.clear();
            out.push(c);
        } else {
            word.push(c);
        }
    }
    out.push_str(&scrub_word(&word));
    out
}

/// An error and what it wraps, as one scrubbed line.
pub fn chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(inner) = source {
        text.push_str(": ");
        text.push_str(&inner.to_string());
        source = inner.source();
    }
    scrub(&text)
}

const EDGE: &[char] = &['(', ')', '[', ']', '{', '}', '<', '>', ',', ';', ':', '"', '\'', '`', '.'];

fn scrub_word(word: &str) -> String {
    if word.is_empty() {
        return String::new();
    }
    let core = word.trim_matches(EDGE);
    if core.is_empty() {
        return word.to_string();
    }
    let start = word.find(core).unwrap_or(0);
    let (head, tail) = (&word[..start], &word[start + core.len()..]);
    let replacement = if core.contains("://") {
        "<url>".to_string()
    } else if core.contains('@') && core.contains('.') {
        "<email>".to_string()
    } else if is_ip(core) {
        "<ip>".to_string()
    } else if let Some(path) = home_path(core) {
        path
    } else if is_token(core) {
        "<secret>".to_string()
    } else {
        core.to_string()
    };
    format!("{head}{replacement}{tail}")
}

/// `1.2.3.4`, with a port or not; and IPv6 literals (two or more colons of hex).
fn is_ip(word: &str) -> bool {
    let host = word.rsplit_once(':').filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit())).map_or(word, |(host, _)| host);
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() == 4 && parts.iter().all(|p| !p.is_empty() && p.len() <= 3 && p.chars().all(|c| c.is_ascii_digit())) {
        return true;
    }
    let bare = word.trim_start_matches('[');
    bare.matches(':').count() >= 2 && bare.chars().all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '.' | '%' | ']' | '[' | '_') || c.is_ascii_alphanumeric())
}

/// A path under a home folder with the user's name taken out: `/Users/ann/x` -> `~/x`.
fn home_path(word: &str) -> Option<String> {
    for prefix in ["/Users/", "/home/"] {
        if let Some(rest) = word.strip_prefix(prefix) {
            return Some(rest.split_once('/').map_or("~".to_string(), |(_, tail)| format!("~/{tail}")));
        }
    }
    let lower = word.to_ascii_lowercase();
    if let Some(rest) = lower.find(":\\users\\").map(|i| &word[i + 8..]) {
        return Some(rest.split_once('\\').map_or("~".to_string(), |(_, tail)| format!("~\\{tail}")));
    }
    None
}

/// A long run of token characters holding a letter and a digit: an access token, a hash, a
/// password, a UUID. Plain long words (`ServiceUnavailable...`) have no digit and stay.
fn is_token(word: &str) -> bool {
    word.len() >= 20
        && word.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '/' | '=' | '.'))
        && word.chars().any(|c| c.is_ascii_digit())
        && word.chars().any(|c| c.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_out_what_the_log_must_not_carry() {
        for (text, gone) in [
            ("error sending request for url (https://build.example.org:8484/api/service/v1/state)", "build.example.org"),
            ("connect to 192.168.1.20:8484 failed", "192.168.1.20"),
            ("connect to [fe80::1ff:fe23:4567:890a] failed", "fe80"),
            ("mail ann@example.com failed", "ann@example.com"),
            ("can't read /Users/ann/Library/Application Support/x", "ann"),
            (r"can't read C:\Users\Ann\AppData\x", "Ann"),
            ("Authorization: Bearer 3f9a1c0d77b24e5a8c1e9d0b6a4f2e71", "3f9a1c0d"),
            ("build cef26e2b-28b3-4e68-9f20-5ba5174ac1d0 failed", "cef26e2b"),
        ] {
            let scrubbed = scrub(text);
            assert!(!scrubbed.contains(gone), "{text:?} -> {scrubbed:?}");
        }
    }

    #[test]
    fn keeps_what_an_error_needs_to_read_as_one() {
        for text in [
            "connection refused (os error 61)",
            "operation timed out after 95s",
            "state took 2884 ms, status 200",
            "tcp connect error: Connection refused",
            "dns error: ServiceUnavailableForLegalReasonsOrSimilar",
        ] {
            assert_eq!(scrub(text), text);
        }
        assert_eq!(scrub("a  b\tc"), "a  b\tc", "whitespace is kept");
        assert_eq!(scrub("see /Users/ann/x."), "see ~/x.");
    }

    #[test]
    fn a_chain_is_one_scrubbed_line() {
        let inner = std::io::Error::other("refused by 10.0.0.5:9");
        let outer = std::io::Error::other(inner);
        let text = chain(&outer);
        assert!(text.contains("refused") && !text.contains("10.0.0.5"), "{text}");
    }
}
