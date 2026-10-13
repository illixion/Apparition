//! Errors as people read them: what went wrong, outermost first, one line per cause.
//!
//! rootcause's own rendering is a tree with the source file and line of every cause
//! (`├ /Users/…/.cargo/…/apple_account.rs:191`), which is for developers and shows the build
//! machine's paths. Release builds record no locations, and every message shown to a person goes
//! through [`describe`]. Development builds keep the locations, as attachments under each cause.

use rootcause::hooks::Hooks;
use rootcause::hooks::context_formatter::ContextFormatterHook;

/// Installs the hooks every report uses. Call once, before anything makes a report; isideload's
/// own (`isideload::init`) then finds hooks installed and leaves them.
pub fn install_hooks() {
    let hooks = if cfg!(debug_assertions) { Hooks::new() } else { Hooks::new_without_locations() };
    let _ = hooks.context_formatter::<reqwest::Error, _>(RequestError).install();
}

/// reqwest's own message ("error sending request") hides the reason; add each source's.
struct RequestError;

impl ContextFormatterHook<reqwest::Error> for RequestError {
    fn display(
        &self,
        report: rootcause::ReportRef<'_, reqwest::Error, rootcause::markers::Uncloneable, rootcause::markers::Local>,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        write!(f, "{}", report.format_current_context_unhooked())?;
        let mut source = report.current_context_error_source();
        while let Some(cause) = source {
            write!(f, ": {cause}")?;
            source = cause.source();
        }
        Ok(())
    }
}

/// The report as plain lines: each cause's message, outermost first, then anything attached to
/// it. Repeated lines (a context restating its cause) appear once.
pub fn describe<C: ?Sized, O, T>(error: &rootcause::Report<C, O, T>) -> String
where
    O: rootcause::markers::ReportOwnershipMarker,
{
    let mut lines: Vec<String> = Vec::new();
    let mut add = |text: String| {
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
            if !lines.iter().any(|seen| seen == line) {
                lines.push(line.to_string());
            }
        }
    };
    for report in error.iter_reports() {
        add(report.format_current_context().to_string());
        for attachment in report.attachments().iter() {
            add(attachment.format_inner().to_string());
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rootcause::prelude::*;

    #[test]
    fn outermost_first_without_repeats() {
        let inner: Result<(), Report> = Err(report!("Auth error -80045: This action cannot be completed at this time."));
        let error = inner.context("GrandSlam error during initial login request").context("Apple ID sign-in failed").unwrap_err();
        let text = describe(&error);
        let lines: Vec<&str> = text.lines().filter(|line| !line.contains(".rs:")).collect();
        assert_eq!(
            lines,
            ["Apple ID sign-in failed", "GrandSlam error during initial login request", "Auth error -80045: This action cannot be completed at this time."]
        );
        assert!(!text.contains('●') && !text.contains('╰'));
    }
}
