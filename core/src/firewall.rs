//! Windows Defender Firewall and the ports Apparition listens on.
//!
//! Windows drops what comes in for a program until a rule allows that program. It asks the
//! first time the program listens, but only on the desktop and only once: started over SSH or
//! at login with nobody to answer, the program is blocked without a word (an unanswered question
//! leaves a block rule behind), and a device then finds that this computer "didn't answer".
//!
//! Windows' own question is the way in wherever it can be asked: it comes when a program first
//! listens, needs nothing from Apparition, and its answer is remembered. Apparition steps in only
//! where that question can't help ([`needs_rule`]): when it was left unanswered and Windows won't
//! ask again, and when there is no desktop to ask on. It looks for itself in the firewall's rules
//! ([`inbound`]) and puts the rule there ([`allow`]), with an administrator's approval when this
//! program doesn't already run as one. The rule names this program's file, covers incoming
//! connections only, and is made for private and domain networks, and for a public one when this
//! computer is on one at that moment.
//!
//! Apparition's setup for Windows (`scripts/windows/setup.nsi`) makes the rules before either
//! program first runs ([`allow_programs`], through `apparition-cli firewall allow`), so an
//! installed copy never meets the question, and takes them away when it is removed
//! ([`remove_programs`]). The rest above is for a copy unpacked from the zip, or moved since.
//!
//! The rules are read from where the firewall keeps them in the registry, which needs no rights
//! and reads the same in every language (`netsh`'s output is translated). Everywhere but Windows
//! there is nothing to manage, and [`inbound`] says so with `None`.

/// What the firewall does with connections coming in for a program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inbound {
    /// A rule lets them in.
    Allowed,
    /// A rule turns them away: usually the one Windows leaves when its question goes unanswered.
    Blocked,
    /// No rule names the program, so they are dropped, and Windows asks when it can.
    Unasked,
}

impl Inbound {
    /// A word for a front end to switch on.
    pub fn name(self) -> &'static str {
        match self {
            Inbound::Allowed => "allowed",
            Inbound::Blocked => "blocked",
            Inbound::Unasked => "unasked",
        }
    }
}

/// What a set of the firewall's stored rules says about `program` (its full path).
///
/// A stored rule is fields joined by `|`, as in
/// `v2.30|Action=Allow|Active=TRUE|Dir=In|Protocol=6|Profile=Private|App=C:\x\y.exe|Name=Y|`.
/// The ones that count are switched on, for incoming connections, name this program, and are
/// for TCP or for every protocol. A block rule wins over an allow rule, as it does in the
/// firewall. Which kinds of network a rule covers isn't weighed: one made for another kind than
/// the computer is on now reads as allowed here.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn verdict<'a>(rules: impl IntoIterator<Item = &'a str>, program: &str) -> Inbound {
    let mut found = Inbound::Unasked;
    for rule in rules {
        let field = |name: &str| rule.split('|').find_map(|field| field.strip_prefix(name)?.strip_prefix('='));
        let ours = field("App").is_some_and(|app| app.to_lowercase() == program.to_lowercase());
        let counts = field("Active") == Some("TRUE") && field("Dir") == Some("In") && matches!(field("Protocol"), None | Some("6"));
        if !ours || !counts {
            continue;
        }
        match field("Action") {
            Some("Block") => return Inbound::Blocked,
            Some("Allow") => found = Inbound::Allowed,
            _ => {}
        }
    }
    found
}

/// What the firewall does with connections coming in for this program, or `None` where there is
/// no such firewall (macOS, Linux) or its rules couldn't be read.
pub fn inbound() -> Option<Inbound> {
    #[cfg(windows)]
    {
        windows::inbound()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Whether Apparition has to make the firewall's rule itself ([`allow`]) for devices to reach this
/// program: a block rule is in the way, which Windows never asks about again, or no rule names
/// the program and this isn't a desktop session (SSH, a service), so Windows has nobody to ask.
/// On a desktop with no rule yet, Windows asks by itself when the program listens, and this says
/// no.
pub fn needs_rule() -> bool {
    #[cfg(windows)]
    {
        match windows::inbound() {
            Some(Inbound::Blocked) => true,
            Some(Inbound::Unasked) => !windows::on_a_desktop(),
            Some(Inbound::Allowed) | None => false,
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Lets connections in for this program: removes the rules that name it and adds one that
/// allows it. Windows asks for an administrator's approval unless this program already runs as
/// one. Fails when that is declined, or can't be asked (no desktop).
pub fn allow() -> Result<(), rootcause::Report> {
    #[cfg(windows)]
    {
        windows::allow(&[windows::program().ok_or_else(|| rootcause::report!("couldn't tell where this program is"))?])
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}

/// What the firewall does with connections coming in for the program at `program`, as
/// [`inbound`] says for this one.
pub fn inbound_for(program: &std::path::Path) -> Option<Inbound> {
    #[cfg(windows)]
    {
        windows::inbound_for(program.to_str()?)
    }
    #[cfg(not(windows))]
    {
        let _ = program;
        None
    }
}

/// [`allow`] for the programs at `programs`, with one approval for them all: what Apparition's
/// setup does for the app and the command line it has just put in place, so that neither ever
/// meets Windows' question.
pub fn allow_programs(programs: &[std::path::PathBuf]) -> Result<(), rootcause::Report> {
    #[cfg(windows)]
    {
        windows::allow(&windows::texts(programs)?)
    }
    #[cfg(not(windows))]
    {
        let _ = programs;
        Ok(())
    }
}

/// Takes away every rule for incoming connections that names one of `programs`: what removing
/// Apparition does, so that no rule outlives the file it was for.
pub fn remove_programs(programs: &[std::path::PathBuf]) -> Result<(), rootcause::Report> {
    #[cfg(windows)]
    {
        windows::remove(&windows::texts(programs)?)
    }
    #[cfg(not(windows))]
    {
        let _ = programs;
        Ok(())
    }
}

#[cfg(windows)]
mod windows {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    use base64::Engine;
    use rootcause::prelude::*;

    use super::{Inbound, verdict};

    const RULES: &str = r"SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\FirewallRules";
    /// Keeps a console window from flashing up when the desktop app runs PowerShell.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// Whether this program runs in a session that has a desktop. Services, and what they start
    /// (a command run over SSH), share session 0, which has none.
    pub(super) fn on_a_desktop() -> bool {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcessId() -> u32;
            fn ProcessIdToSessionId(process: u32, session: *mut u32) -> i32;
        }
        let mut session = 0;
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) != 0 && session != 0 }
    }

    pub(super) fn program() -> Option<String> {
        std::env::current_exe().ok()?.to_str().map(str::to_owned)
    }

    /// Paths as the firewall's rules spell them.
    pub(super) fn texts(programs: &[std::path::PathBuf]) -> Result<Vec<String>, Report> {
        programs.iter().map(|program| program.to_str().map(str::to_owned).ok_or_else(|| report!("{} isn't a path the firewall can name", program.display()))).collect()
    }

    pub(super) fn inbound() -> Option<Inbound> {
        inbound_for(&program()?)
    }

    pub(super) fn inbound_for(program: &str) -> Option<Inbound> {
        let key = windows_registry::LOCAL_MACHINE.open(RULES).ok()?;
        let rules: Vec<String> = key.values().ok()?.filter_map(|(_, value)| String::try_from(value).ok()).collect();
        Some(verdict(rules.iter().map(String::as_str), program))
    }

    /// PowerShell's list of `programs`. A single-quoted PowerShell string takes everything
    /// literally but the quote itself.
    fn list(programs: &[String]) -> String {
        programs.iter().map(|program| format!("'{}'", program.replace('\'', "''"))).collect::<Vec<_>>().join(", ")
    }

    /// The PowerShell that takes away the rules for `programs`. Run as an administrator. The
    /// firewall cmdlets take and give the same names in every language.
    fn removal(programs: &[String]) -> String {
        format!(
            r#"$ErrorActionPreference = 'Stop'
foreach ($program in @({})) {{
    Get-NetFirewallApplicationFilter -Program $program -ErrorAction SilentlyContinue | Get-NetFirewallRule | Where-Object {{ $_.Direction -eq 'Inbound' }} | Remove-NetFirewallRule
}}
"#,
            list(programs)
        )
    }

    /// The PowerShell that makes the rules, for `programs`: [`removal`], then a rule each for
    /// private and domain networks, and for a public one when this computer is on one now (home
    /// Wi-Fi is often left that way).
    fn script(programs: &[String]) -> String {
        format!(
            r#"{}$kinds = @(@('Private', 'Domain') + @(Get-NetConnectionProfile | ForEach-Object {{ if ($_.NetworkCategory -eq 'DomainAuthenticated') {{ 'Domain' }} else {{ "$($_.NetworkCategory)" }} }}) | Sort-Object -Unique)
foreach ($program in @({})) {{
    New-NetFirewallRule -DisplayName 'Apparition' -Description 'Lets your devices reach Apparition on this computer.' -Direction Inbound -Action Allow -Program $program -Profile $kinds | Out-Null
}}
exit 0
"#,
            removal(programs),
            list(programs)
        )
    }

    fn encoded(script: &str) -> String {
        let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        base64::engine::general_purpose::STANDARD.encode(utf16)
    }

    fn powershell(script: &str) -> bool {
        // (Without a PowerShell 7's module path, which this may have been started from and
        // which makes Windows PowerShell load that version's modules.)
        Command::new("powershell.exe")
            .env_remove("PSModulePath")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", &encoded(script)])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .is_ok_and(|output| output.status.success())
    }

    /// Runs `script` as this program, which is enough when it runs as an administrator already
    /// (setup, or over SSH); else as an administrator, which Windows asks the person to approve.
    fn as_administrator(script: &str) {
        if !powershell(script) {
            let elevate = format!(
                "$ErrorActionPreference = 'Stop'\n$run = Start-Process powershell.exe -Verb RunAs -WindowStyle Hidden -Wait -PassThru -ArgumentList '-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-EncodedCommand','{}'\nexit $run.ExitCode\n",
                encoded(script)
            );
            powershell(&elevate);
        }
    }

    pub(super) fn allow(programs: &[String]) -> Result<(), Report> {
        as_administrator(&script(programs));
        if programs.iter().all(|program| inbound_for(program) == Some(Inbound::Allowed)) {
            return Ok(());
        }
        bail!("Windows Defender Firewall still blocks Apparition: the change wasn't approved, or couldn't be made. Allow it for your network in Windows Security > Firewall & network protection > Allow an app through firewall.")
    }

    pub(super) fn remove(programs: &[String]) -> Result<(), Report> {
        if programs.iter().all(|program| inbound_for(program) == Some(Inbound::Unasked)) {
            return Ok(());
        }
        as_administrator(&format!("{}exit 0\n", removal(programs)));
        if programs.iter().all(|program| inbound_for(program) == Some(Inbound::Unasked)) {
            return Ok(());
        }
        bail!("Apparition's rules are still in Windows Defender Firewall: the change wasn't approved, or couldn't be made.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: &str = r"C:\Users\Zoë\Apparition\Apparition.exe";

    #[test]
    fn a_program_no_rule_names_is_unasked() {
        let rules = [r"v2.30|Action=Allow|Active=TRUE|Dir=In|Protocol=6|App=C:\Program Files\Sunshine\sunshine.exe|Name=Sunshine|"];
        assert_eq!(verdict(rules, APP), Inbound::Unasked);
        assert_eq!(verdict([], APP), Inbound::Unasked);
    }

    #[test]
    fn an_allow_rule_for_the_program_lets_it_in_whatever_the_case_of_its_path() {
        let rule = r"v2.30|Action=Allow|Active=TRUE|Dir=In|App=c:\users\zoë\apparition\apparition.exe|Name=Apparition|";
        assert_eq!(verdict([rule], APP), Inbound::Allowed);
    }

    #[test]
    fn the_block_rule_an_unanswered_question_leaves_wins() {
        let rules = [
            format!(r"v2.30|Action=Allow|Active=TRUE|Dir=In|Protocol=6|Profile=Private|App={APP}|Name=Apparition|"),
            format!(r"v2.10|Action=Block|Active=TRUE|Dir=In|Protocol=6|Profile=Private|App={APP}|Name=Apparition.exe|Desc=Apparition.exe|"),
        ];
        assert_eq!(verdict(rules.iter().map(String::as_str), APP), Inbound::Blocked);
    }

    #[test]
    fn rules_that_are_off_outgoing_or_for_another_protocol_do_not_count() {
        let rules = [
            format!(r"v2.30|Action=Allow|Active=FALSE|Dir=In|Protocol=6|App={APP}|"),
            format!(r"v2.30|Action=Allow|Active=TRUE|Dir=Out|Protocol=6|App={APP}|"),
            format!(r"v2.30|Action=Block|Active=TRUE|Dir=In|Protocol=17|App={APP}|"),
        ];
        assert_eq!(verdict(rules.iter().map(String::as_str), APP), Inbound::Unasked);
    }
}
