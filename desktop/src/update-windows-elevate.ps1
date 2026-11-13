# Runs the update helper (update-windows.ps1) as an administrator, for a copy of Apparition that
# only an administrator may replace, and starts the app again afterwards. This part runs as the
# person: an app started by the elevated helper would run as the administrator who approved,
# which on a PC where that is someone else's account is the wrong person altogether.
# {STAGE}, {HELPER} and {PLAN} are filled in by updater.rs: the staging folder, and the SHA-256
# of the helper and of the plan as the app wrote them.
$ErrorActionPreference = 'Stop'
$stage = '{STAGE}'
$plan = Get-Content -Raw -LiteralPath (Join-Path $stage 'plan.json') | ConvertFrom-Json
$helper = @'
$ErrorActionPreference = 'Stop'
$bytes = [System.IO.File]::ReadAllBytes((Join-Path $args[0] 'helper.ps1'))
$sha = [System.Security.Cryptography.SHA256]::Create()
if ([BitConverter]::ToString($sha.ComputeHash($bytes)).Replace('-', '') -ne '{HELPER}') { exit 1 }
& ([scriptblock]::Create([System.Text.Encoding]::UTF8.GetString($bytes))) -Stage $args[0] -Elevated -PlanHash '{PLAN}'
'@
# -EncodedCommand takes no arguments of its own, so the folder goes into the text.
$helper = $helper.Replace('$args[0]', "'" + $stage.Replace("'", "''") + "'")
$encoded = [Convert]::ToBase64String([System.Text.Encoding]::Unicode.GetBytes($helper))
$run = Start-Process powershell.exe -Verb RunAs -WindowStyle Hidden -Wait -PassThru -ArgumentList '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-EncodedCommand', $encoded
# The app has quit only if the helper got as far as replacing it (or putting it back).
$quit = $false
try { $quit = [System.Diagnostics.Process]::GetProcessById($plan.pid).HasExited } catch { $quit = $true }
if ($quit -and (Test-Path -LiteralPath $plan.executable)) {
    $options = @{ FilePath = $plan.executable; WorkingDirectory = $plan.directory }
    if ($plan.arguments) { $options.ArgumentList = $plan.arguments }
    Start-Process @options
}
