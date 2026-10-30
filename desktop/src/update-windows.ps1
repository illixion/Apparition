param([Parameter(Mandatory=$true)][string]$Stage)
$ErrorActionPreference = 'Stop'
$parentExited = $false
$changed = @()
$work = $null
$plan = Get-Content -Raw -LiteralPath (Join-Path $Stage 'plan.json') | ConvertFrom-Json
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
    $new = Start-App
    if ($new.WaitForExit(1500) -and $new.ExitCode -ne 0) {
        throw "The updated app could not start ($($new.ExitCode))."
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
    if ($parentExited -and (Test-Path -LiteralPath $plan.executable)) {
        $null = Start-App
    }
} finally {
    # Keep recoverable backups if rollback itself failed.
    if ($work -and !(Get-ChildItem -LiteralPath $work -Filter '*.old' -ErrorAction SilentlyContinue)) {
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    } elseif ($work -and !$failure) {
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -LiteralPath $Stage -Recurse -Force -ErrorAction SilentlyContinue
}
