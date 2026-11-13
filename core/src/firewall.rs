//! Windows Defender Firewall and the ports Apparition listens on.
//!
//! Windows drops what comes in for a program until a rule allows that program. It asks the
//! first time the program listens, but only on the desktop and only once: started over SSH or
//! at login with nobody to answer, the program is blocked without a word (an unanswered question
//! leaves a block rule behind), and a device then finds that this computer "didn't answer".
//!
//! So Apparition looks for itself in the firewall's rules ([`inbound`]) and, where it isn't
//! allowed, puts the rule there ([`allow`]): what answering Windows' own question does, with the
//! same administrator approval. The rule names this program's file, covers incoming connections
//! only, and is made for the kinds of network (private, public, domain) this computer is on at
//! that moment, as Windows' own question would be.
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

/// Lets connections in for this program: removes the rules that name it and adds one that
/// allows it. Windows asks for an administrator's approval unless this program already runs as
/// one. Fails when that is declined, or can't be asked (no desktop).
pub fn allow() -> Result<(), rootcause::Report> {
    #[cfg(windows)]
    {
        windows::allow()
    }
    #[cfg(not(windows))]
    {
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

    fn program() -> Option<String> {
        std::env::current_exe().ok()?.to_str().map(str::to_owned)
    }

    pub(super) fn inbound() -> Option<Inbound> {
        let program = program()?;
        let key = windows_registry::LOCAL_MACHINE.open(RULES).ok()?;
        let rules: Vec<String> = key.values().ok()?.filter_map(|(_, value)| String::try_from(value).ok()).collect();
        Some(verdict(rules.iter().map(String::as_str), &program))
    }

    /// The PowerShell that makes the rule, for `program`. Run as an administrator. The firewall
    /// cmdlets take and give the same names in every language.
    fn script(program: &str) -> String {
        // A single-quoted PowerShell string takes everything literally but the quote itself.
        let quoted = program.replace('\'', "''");
        format!(
            r#"$ErrorActionPreference = 'Stop'
$program = '{quoted}'
$kinds = @(Get-NetConnectionProfile | ForEach-Object {{ if ($_.NetworkCategory -eq 'DomainAuthenticated') {{ 'Domain' }} else {{ "$($_.NetworkCategory)" }} }} | Sort-Object -Unique)
if ($kinds.Count -eq 0) {{ $kinds = @('Private') }}
Get-NetFirewallApplicationFilter -Program $program -ErrorAction SilentlyContinue | Get-NetFirewallRule | Where-Object {{ $_.Direction -eq 'Inbound' }} | Remove-NetFirewallRule
New-NetFirewallRule -DisplayName 'Apparition' -Description 'Lets your devices reach Apparition on this computer.' -Direction Inbound -Action Allow -Program $program -Profile $kinds | Out-Null
exit 0
"#
        )
    }

    fn encoded(script: &str) -> String {
        let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        base64::engine::general_purpose::STANDARD.encode(utf16)
    }

    fn powershell(script: &str) -> bool {
        Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", &encoded(script)])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .is_ok_and(|output| output.status.success())
    }

    pub(super) fn allow() -> Result<(), Report> {
        let program = program().ok_or_else(|| report!("couldn't tell where this program is"))?;
        let rule = script(&program);
        // As this program: enough when it runs as an administrator already (over SSH, say).
        if !powershell(&rule) {
            // Else as an administrator, which Windows asks the person to approve.
            let elevate = format!(
                "$ErrorActionPreference = 'Stop'\n$run = Start-Process powershell.exe -Verb RunAs -WindowStyle Hidden -Wait -PassThru -ArgumentList '-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-EncodedCommand','{}'\nexit $run.ExitCode\n",
                encoded(&rule)
            );
            powershell(&elevate);
        }
        match inbound() {
            Some(Inbound::Allowed) => Ok(()),
            _ => bail!("Windows Defender Firewall still blocks Apparition: the change wasn't approved, or couldn't be made. Allow it for your network in Windows Security > Firewall & network protection > Allow an app through firewall."),
        }
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
