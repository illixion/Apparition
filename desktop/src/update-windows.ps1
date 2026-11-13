# -Elevated: run as an administrator (update-windows-elevate.ps1), for a copy in a folder only an
# administrator may write to, or whose background service holds its file. The app is then started
# again by the one that asked, as the person, not from here; and -PlanHash is the plan the app
# wrote, which sits where anything of the person's could have changed it since.
param([Parameter(Mandatory=$true)][string]$Stage, [switch]$Elevated, [string]$PlanHash)
$ErrorActionPreference = 'Stop'
$parentExited = $false
$changed = @()
$work = $null
$service = $false
$plan = Get-Content -Raw -LiteralPath (Join-Path $Stage 'plan.json') | ConvertFrom-Json
# Device Guard / AppLocker policies run PowerShell in Constrained Language mode, where the
# .NET calls below are refused. Say so plainly, before anything changes.
if ($ExecutionContext.SessionState.LanguageMode -ne 'FullLanguage') {
    @{ version = $plan.version; blocked = $true; error = "Windows policy runs PowerShell in $($ExecutionContext.SessionState.LanguageMode) mode, which the updater needs to be unrestricted." } |
        ConvertTo-Json | Set-Content -LiteralPath $plan.result -Encoding UTF8
    exit 1
}
if ($PlanHash -and (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $Stage 'plan.json')).Hash -ne $PlanHash) { exit 1 }
function Start-App {
    $options = @{ FilePath = $plan.executable; WorkingDirectory = $plan.directory; PassThru = $true }
    if ($plan.arguments) { $options.ArgumentList = $plan.arguments }
    Start-Process @options
}
try {
    $parent = [System.Diagnostics.Process]::GetProcessById($plan.pid)
    $null = $parent.Handle
    $work = Join-Path $plan.directory ('.apparition-update-' + [guid]::NewGuid().ToString('N'))
    $null = New-Item -ItemType Directory -Path $work
    foreach ($file in $plan.files) {
        $source = Join-Path (Join-Path $Stage 'unpacked') $file.source
        $staged = Join-Path $work $file.source
        Copy-Item -LiteralPath $source -Destination $staged
        $stream = [System.IO.File]::OpenRead($staged)
        $sha = [System.Security.Cryptography.SHA256]::Create()
        try { $actual = [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '') }
        finally { $stream.Dispose(); $sha.Dispose() }
        if ($actual -ne $file.sha256) {
            throw 'The staged update changed after verification.'
        }
    }
    Set-Content -LiteralPath (Join-Path $Stage 'ready') -Value 'ready'
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    while (!(Test-Path -LiteralPath (Join-Path $Stage 'go'))) {
        if ([DateTime]::UtcNow -gt $deadline) { throw 'The application did not authorize the update.' }
        Start-Sleep -Milliseconds 100
    }
    if (!$parent.WaitForExit(60000)) { throw 'Apparition did not quit; no files were replaced.' }
    $parentExited = $true
    # The background service runs apparition-cli.exe from this folder, when it is installed:
    # Windows won't move a running program's file, so it stops for the swap.
    $running = Get-CimInstance Win32_Service -Filter "Name='Apparition'" -ErrorAction SilentlyContinue
    if ($running -and $running.ProcessId -and $running.PathName.IndexOf($plan.directory + '\', [StringComparison]::OrdinalIgnoreCase) -ge 0) {
        $service = $true
        Stop-Service -Name Apparition -Force
        try { $null = [System.Diagnostics.Process]::GetProcessById($running.ProcessId).WaitForExit(15000) } catch { }
    }
    foreach ($file in $plan.files) {
        $target = Join-Path $plan.directory $file.target
        $backup = Join-Path $work ($file.source + '.old')
        $entry = @{ target = $target; backup = $backup; old = $false; installed = $false }
        $changed += $entry
        if (Test-Path -LiteralPath $target) {
            Move-Item -LiteralPath $target -Destination $backup
            $entry.old = $true
        }
        Move-Item -LiteralPath (Join-Path $work $file.source) -Destination $target
        $entry.installed = $true
    }
    if (!$Elevated) {
        $new = Start-App
        if ($new.WaitForExit(1500) -and $new.ExitCode -ne 0) {
            throw "The updated app could not start ($($new.ExitCode))."
        }
    }
    # What Windows' list of installed apps says, when setup put this copy here.
    foreach ($hive in 'HKLM:', 'HKCU:') {
        $key = "$hive\Software\Microsoft\Windows\CurrentVersion\Uninstall\Apparition"
        $entry = Get-ItemProperty -LiteralPath $key -ErrorAction SilentlyContinue
        if ($entry -and $entry.InstallLocation -eq $plan.directory) {
            Set-ItemProperty -LiteralPath $key -Name DisplayVersion -Value $plan.version -ErrorAction SilentlyContinue
        }
    }
} catch {
    $failure = $_.Exception.Message + " (updater line $($_.InvocationInfo.ScriptLineNumber))"
    [array]::Reverse($changed)
    foreach ($entry in $changed) {
        try {
            if ($entry.installed) { Remove-Item -LiteralPath $entry.target }
            if ($entry.old) { Move-Item -LiteralPath $entry.backup -Destination $entry.target }
        } catch { $failure += ' Could not restore a file: ' + $_.Exception.Message + '. Backups: ' + $work }
    }
    @{ version = $plan.version; error = $failure } | ConvertTo-Json |
        Set-Content -LiteralPath $plan.result -Encoding UTF8
    if (!$Elevated -and $parentExited -and (Test-Path -LiteralPath $plan.executable)) {
        $null = Start-App
    }
} finally {
    if ($service) {
        try { Start-Service -Name Apparition } catch { }
    }
    # Keep recoverable backups if rollback itself failed.
    if ($work -and !(Get-ChildItem -LiteralPath $work -Filter '*.old' -ErrorAction SilentlyContinue)) {
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    } elseif ($work -and !$failure) {
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -LiteralPath $Stage -Recurse -Force -ErrorAction SilentlyContinue
}
