# WSL + Rust build env install script (run as Administrator)
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File scripts\wsl_install.ps1
$ErrorActionPreference = 'Continue'
$log = Join-Path $PSScriptRoot 'wsl_setup.log'

function Log([string]$m) {
    $line = '[{0}] {1}' -f (Get-Date -Format 'HH:mm:ss'), $m
    Add-Content -Path $log -Value $line -Encoding UTF8
    Write-Host $line
}

Remove-Item $log -ErrorAction SilentlyContinue
Log 'start elevated setup'

foreach ($f in 'Microsoft-Windows-Subsystem-Linux', 'VirtualMachinePlatform') {
    Log "enable feature $f"
    dism.exe /online /enable-feature /featurename:$f /all /norestart 2>&1 | ForEach-Object { Log "$_" }
}
Log 'features done'

Log 'wsl --install --no-distribution'
wsl.exe --install --no-distribution --web-download 2>&1 | ForEach-Object { Log "$_" }
Log "wsl install exit=$LASTEXITCODE"

$distro = (wsl.exe --list --quiet 2>$null | Out-String)
if ($distro -notmatch 'Ubuntu') {
    Log 'install Ubuntu'
    wsl.exe --install -d Ubuntu --no-launch 2>&1 | ForEach-Object { Log "$_" }
    Log "ubuntu install exit=$LASTEXITCODE"
} else {
    Log 'Ubuntu already present'
}

Log 'DONE - reboot if prompted, then run wsl_init_rust.ps1'
Write-Host ''
Write-Host 'Install finished. Reboot first if the system asks.' -ForegroundColor Green
