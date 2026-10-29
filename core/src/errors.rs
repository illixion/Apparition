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
    let _ = hooks.context_formatter::<reqwest::Error, _>(RequestError).context_formatter::<reqwest_middleware::Error, _>(RequestError).install();
}

/// Apple's servers turning the network away, if that is what `error` or its causes say (see
/// isideload's `edge`): reqwest buries it under "error sending request".
fn apple_refusal<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a isideload::auth::edge::EdgeError> {
    use isideload::auth::edge::EdgeError;
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(refusal) = error.downcast_ref::<EdgeError>() {
            return Some(refusal);
        }
        if let Some(reqwest_middleware::Error::Middleware(inner)) = error.downcast_ref::<reqwest_middleware::Error>()
            && let Some(refusal) = inner.downcast_ref::<EdgeError>()
        {
            return Some(refusal);
        }
        current = error.source();
    }
    None
}

/// reqwest's own message ("error sending request") hides the reason; add each source's. When the
/// reason is Apple's servers turning the network away, say only that, in its own words.
struct RequestError;

impl RequestError {
    fn write(
        f: &mut std::fmt::Formatter<'_>,
        error: &(dyn std::error::Error + 'static),
        unhooked: impl std::fmt::Display,
        mut source: Option<&(dyn std::error::Error + 'static)>,
    ) -> std::fmt::Result {
        if let Some(refusal) = apple_refusal(error) {
            return write!(f, "{refusal}");
        }
        write!(f, "{unhooked}")?;
        while let Some(cause) = source {
            write!(f, ": {cause}")?;
            source = cause.source();
        }
        Ok(())
    }
}

impl ContextFormatterHook<reqwest::Error> for RequestError {
    fn display(
        &self,
        report: rootcause::ReportRef<'_, reqwest::Error, rootcause::markers::Uncloneable, rootcause::markers::Local>,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        Self::write(f, report.current_context(), report.format_current_context_unhooked(), report.current_context_error_source())
    }
}

impl ContextFormatterHook<reqwest_middleware::Error> for RequestError {
    fn display(
        &self,
        report: rootcause::ReportRef<'_, reqwest_middleware::Error, rootcause::markers::Uncloneable, rootcause::markers::Local>,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        Self::write(f, report.current_context(), report.format_current_context_unhooked(), report.current_context_error_source())
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
