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
}

impl Source {
    /// A source from two URL prefixes (a trailing `/` is added). `files` may hold `{version}`.
    pub fn new(latest: impl Into<String>, files: impl Into<String>) -> Self {
        Self { latest: slash(latest.into()), files: slash(files.into()) }
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
        Self::mirror("https://apps.illixion.com", app)
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

fn slash(mut url: String) -> String {
    if !url.ends_with('/') {
        url.push('/');
    }
    url
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
}

impl Config {
    pub fn new(product: &str, namespace: &str, signers: &str) -> Self {
        Self {
            product: product.into(),
            namespace: namespace.into(),
            signers: signers.into(),
            sources: Vec::new(),
            user_agent: format!("{product}-updater"),
        }
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
    fn mirror_urls() {
        let s = Source::mirror("https://apps.illixion.com/", "vergence");
        assert_eq!(s.latest_url("SHA256SUMS.sig"), "https://apps.illixion.com/vergence/latest/SHA256SUMS.sig");
        assert_eq!(s.file_url("0.2.0", "f.zip"), "https://apps.illixion.com/vergence/releases/0.2.0/f.zip");
    }
}
