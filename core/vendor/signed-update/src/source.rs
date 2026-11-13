/// Where one copy of a product's releases is served from. A source is two URL prefixes:
///
/// - `latest`: where the newest release's `SHA256SUMS` and `SHA256SUMS.sig` are served;
/// - `files`: where a release's files are, with `{version}` for the release's version.
///
/// The front door is [`Source::apps`]: apps.illixion.com holds no files, only redirects (a
/// Cloudflare `_redirects` rule per app) to wherever the release really is, GitHub Releases
/// normally and a self-hosted copy while GitHub is down, so moving every client is an edit of two
/// lines there. [`Source::github`] is the last resort in case that domain itself is down. Neither
/// is trusted: a source can serve a stale or a hostile copy, and the signature and hashes are what
/// the updater believes. So sources differ only in whether they are up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    latest: String,
    files: String,
    concealed: bool,
}

impl Source {
    /// A source from two URL prefixes (a trailing `/` is added). `files` may hold `{version}`.
    pub fn new(latest: impl Into<String>, files: impl Into<String>) -> Self {
        Self { latest: slash(latest.into()), files: slash(files.into()), concealed: false }
    }

    /// Marks this source's host as one a network shouldn't see you talk to. With
    /// [`Privacy::Ech`] it is reached only over DNS-over-HTTPS and Encrypted Client Hello, and
    /// skipped (never reached in the clear) when that isn't possible. [`Source::apps`] is concealed.
    pub fn concealed(mut self) -> Self {
        self.concealed = true;
        self
    }

    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    pub(crate) fn is_concealed(&self) -> bool {
        self.concealed
    }

    /// The host, normalised as [`normal_host`] does: what a request's host is matched against. The
    /// port is left out on purpose, so a redirect to another port of a concealed host is concealed too.
    #[cfg(feature = "http")]
    pub(crate) fn host(&self) -> Option<String> {
        reqwest::Url::parse(&self.latest).ok()?.host_str().map(normal_host)
    }

    /// GitHub Releases of `repo` ("illixion/vergence"), tagged `v<version>`. `latest` is GitHub's
    /// own redirect to the newest published, non-prerelease release: no API, no token, no rate limit.
    pub fn github(repo: &str) -> Self {
        Self::github_tagged(repo, "v{version}")
    }

    /// [`Source::github`] for a repository whose tags aren't `v<version>`; `tag` has `{version}`.
    pub fn github_tagged(repo: &str, tag: &str) -> Self {
        Self::new(
            format!("https://github.com/{repo}/releases/latest/download/"),
            format!("https://github.com/{repo}/releases/download/{tag}/"),
        )
    }

    /// apps.illixion.com's redirects for `app`: `/<app>/latest/*` and `/<app>/releases/<version>/*`.
    pub fn apps(app: &str) -> Self {
        Self::mirror("https://apps.illixion.com", app).concealed()
    }

    /// Any host that serves `<base>/<app>/latest/<file>` and `<base>/<app>/releases/<version>/<file>`
    /// (a redirect to the real location works as well as the file): a self-hosted copy.
    pub fn mirror(base: &str, app: &str) -> Self {
        let base = base.trim_end_matches('/');
        Self::new(format!("{base}/{app}/latest/"), format!("{base}/{app}/releases/{{version}}/"))
    }

    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    pub(crate) fn latest_url(&self, name: &str) -> String {
        format!("{}{name}", self.latest)
    }

    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    pub(crate) fn file_url(&self, version: &str, name: &str) -> String {
        format!("{}{name}", self.files.replace("{version}", version))
    }
}

/// A host name the way two spellings of it compare equal: lowercase, without the trailing dot of a
/// fully qualified name (`APPS.illixion.com.` is apps.illixion.com, and must be concealed as one).
#[cfg(feature = "http")]
pub(crate) fn normal_host(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

fn slash(mut url: String) -> String {
    if !url.ends_with('/') {
        url.push('/');
    }
    url
}

/// How much of the network an update check may show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Privacy {
    /// Ordinary requests with the system's DNS.
    #[default]
    Off,
    /// A [concealed](Source::concealed) source is reached only through DNS-over-HTTPS (by IP, so no
    /// name is sent) and Encrypted Client Hello, so a network log shows a lookup of nothing and a
    /// connection to Cloudflare's shared ECH name, not apps.illixion.com. If the resolver, the ECH
    /// config or the handshake is blocked, that source is skipped and the next one asked: it is
    /// never reached in the clear. Needs the `ech` feature; without it concealed sources are skipped.
    Ech,
}

/// What an updater trusts and where it looks.
#[derive(Debug, Clone)]
pub struct Config {
    /// The file-name prefix every release file carries (`Vergence` for `Vergence-0.2.0-…`).
    pub product: String,
    /// The SSHSIG namespace releases are signed under (`ssh-keygen -Y sign -n vergence-release`).
    /// Not "file", so a signature made for something else with the same keys can't pass here.
    pub namespace: String,
    /// The pinned keys, in ssh-keygen's allowed_signers format: compile the file in.
    pub signers: String,
    /// Tried in order for the signed manifest, and for the file: [`Source::apps`] first, then
    /// [`Source::github`].
    pub sources: Vec<Source>,
    /// Sent with every request.
    pub user_agent: String,
    /// What a check may show a network; see [`Privacy`].
    pub privacy: Privacy,
    /// DNS-over-HTTPS endpoints for [`Privacy::Ech`], asked in order. By IP, so reaching them sends
    /// no name: a certificate for the IP is what is checked.
    pub doh: Vec<String>,
    /// Decides which servers' certificates are trusted, for every request; see [`Config::verifier`].
    #[cfg(feature = "http")]
    pub verifier: Option<std::sync::Arc<dyn rustls::client::danger::ServerCertVerifier>>,
}

impl Config {
    pub fn new(product: &str, namespace: &str, signers: &str) -> Self {
        Self {
            product: product.into(),
            namespace: namespace.into(),
            signers: signers.into(),
            sources: Vec::new(),
            user_agent: format!("{product}-updater"),
            privacy: Privacy::Off,
            doh: vec!["https://1.1.1.1/dns-query".into(), "https://1.0.0.1/dns-query".into()],
            #[cfg(feature = "http")]
            verifier: None,
        }
    }

    /// The app's own certificate verifier, for every request a check and a download make (DNS-over-
    /// HTTPS and ECH included), instead of the operating system's trust store alone: one that also
    /// refuses CAs the app knows intercept, say. It decides who can see or block a check, never what
    /// is installed, which only the release signature does. Built on this crate's rustls
    /// ([`crate::rustls`]).
    #[cfg(feature = "http")]
    pub fn verifier(mut self, verifier: std::sync::Arc<dyn rustls::client::danger::ServerCertVerifier>) -> Self {
        self.verifier = Some(verifier);
        self
    }

    pub fn privacy(mut self, privacy: Privacy) -> Self {
        self.privacy = privacy;
        self
    }

    pub fn doh(mut self, endpoints: Vec<String>) -> Self {
        self.doh = endpoints;
        self
    }

    pub fn source(mut self, source: Source) -> Self {
        self.sources.push(source);
        self
    }

    pub fn user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = user_agent.into();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_urls() {
        let s = Source::github("illixion/vergence");
        assert_eq!(s.latest_url("SHA256SUMS"), "https://github.com/illixion/vergence/releases/latest/download/SHA256SUMS");
        assert_eq!(
            s.file_url("0.2.0", "Vergence-0.2.0-macos-universal.zip"),
            "https://github.com/illixion/vergence/releases/download/v0.2.0/Vergence-0.2.0-macos-universal.zip"
        );
        let odd = Source::github_tagged("illixion/Longwave", "{version}-abc");
        assert!(odd.file_url("0.1.0", "x").ends_with("/download/0.1.0-abc/x"));
    }

    #[test]
    fn apps_is_concealed_and_others_are_not() {
        assert!(Source::apps("x").is_concealed());
        assert!(!Source::github("o/r").is_concealed());
        assert!(!Source::mirror("https://self.example", "x").is_concealed());
        assert!(Source::mirror("https://self.example", "x").concealed().is_concealed());
    }

    #[cfg(feature = "http")]
    #[test]
    fn hosts_compare_without_case_port_or_trailing_dot() {
        assert_eq!(Source::apps("x").host().as_deref(), Some("apps.illixion.com"));
        assert_eq!(Source::new("http://127.0.0.1:8080/a/", "http://127.0.0.1:8080/b/").host().as_deref(), Some("127.0.0.1"));
        assert_eq!(Source::mirror("https://APPS.illixion.com.", "x").host().as_deref(), Some("apps.illixion.com"));
        assert_eq!(normal_host("Apps.Illixion.COM."), "apps.illixion.com");
    }

    #[test]
    fn mirror_urls() {
        let s = Source::mirror("https://apps.illixion.com/", "vergence");
        assert_eq!(s.latest_url("SHA256SUMS.sig"), "https://apps.illixion.com/vergence/latest/SHA256SUMS.sig");
        assert_eq!(s.file_url("0.2.0", "f.zip"), "https://apps.illixion.com/vergence/releases/0.2.0/f.zip");
    }
}
