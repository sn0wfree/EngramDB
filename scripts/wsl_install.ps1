# WSL + Rust 构建环境安装脚本（需管理员运行）
# 用法：右键「使用 PowerShell 运行」或管理员 PowerShell 执行
$ErrorActionPreference = 'Stop'
$log = 'D:\Github\EngramDB\_other_extract\wsl_setup.log'
function Log($m) {
    $line = "[{0}] {1}" -f (Get-Date -Format 'HH:mm:ss'), $m
    Add-Content -Path $log -Value $line -Encoding UTF8
    Write-Host $line
}

Remove-Item $log -ErrorAction SilentlyContinue
Log "start elevated setup"

# 1) Windows 功能
foreach ($f in 'Microsoft-Windows-Subsystem-Linux', 'VirtualMachinePlatform') {
    Log "enable feature $f"
    dism.exe /online /enable-feature /featurename:$f /all /norestart | Tee-Object -FilePath $log -Append | Out-Null
}
Log "features done"

# 2) 安装 WSL（不带发行版，避免交互）
Log "wsl --install --no-distribution"
wsl.exe --install --no-distribution --web-download 2>&1 | Tee-Object -FilePath $log -Append | Out-Null
Log "wsl install exit=$LASTEXITCODE"

# 3) 安装 Ubuntu（若尚未安装）
$distro = (wsl.exe --list --quiet 2>$null | Out-String)
if ($distro -notmatch 'Ubuntu') {
    Log "install Ubuntu"
    wsl.exe --install -d Ubuntu --no-launch 2>&1 | Tee-Object -FilePath $log -Append | Out-Null
    Log "ubuntu install exit=$LASTEXITCODE"
} else {
    Log "Ubuntu already present"
}

Log "DONE - 请重启后再执行 init_ubuntu_rust.ps1 或手动进入 wsl"
Write-Host "`n安装完成。若系统提示需要重启，请先重启。" -ForegroundColor Green
